//! Criterion benches for Byteflow hot paths.
//!
//! ```text
//! cargo bench -p byteflow-actors --bench runtime
//! ```

use std::time::Duration;

use byteflow::{
    Chunk, FlowOutcome, MailboxConfig, Program, Runtime, RuntimeConfig, Value, DEFAULT_QUANTUM,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

fn trivial_chunk() -> Chunk {
    let mut program = Program::new("bench-spawn");
    program.function("worker", 0, |f| {
        let one = f.load_i32(1);
        f.return_(one);
    });
    program.build()
}

fn hop_chunk() -> Chunk {
    let mut program = Program::new("bench-hop");
    let echo = program.function("echo", 0, |f| {
        let msg = f.receive();
        let payload = f.hop_payload(msg);
        f.send_reply(msg, 2, payload);
        f.return_(payload);
    });
    program.function("client", 0, |f| {
        let cap = f.spawn(echo, &[]);
        let n = f.load_i32(1);
        let req = f.hop_fresh(1, n);
        let reply = f.ask(cap, req);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    program.build()
}

fn tiny_runtime(chunk: Chunk) -> Runtime {
    match Runtime::with_config(
        chunk,
        RuntimeConfig {
            workers: 2,
            quantum: DEFAULT_QUANTUM,
            mailbox: MailboxConfig::DEFAULT,
            ..Default::default()
        },
    ) {
        Ok(rt) => rt,
        Err(e) => panic!("runtime: {e}"),
    }
}

fn spawn_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn_join");
    for n in [64usize, 256, 1024] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let rt = tiny_runtime(trivial_chunk());
                let Some(worker) = rt.function_index("worker") else {
                    panic!("missing worker");
                };
                let mut handles = Vec::with_capacity(n);
                for _ in 0..n {
                    match rt.spawn(worker, &[]) {
                        Ok(h) => handles.push(h),
                        Err(e) => panic!("spawn: {e}"),
                    }
                }
                let mut ok = 0usize;
                for h in handles {
                    if matches!(h.join(), FlowOutcome::Completed(Value::Int(1))) {
                        ok += 1;
                    }
                }
                rt.shutdown();
                assert_eq!(ok, n);
            });
        });
    }
    group.finish();
}

fn atomic_hop_ask(c: &mut Criterion) {
    let mut group = c.benchmark_group("atomic_hop_ask");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.bench_function("ask_roundtrip", |b| {
        b.iter(|| {
            let rt = match Runtime::with_natives_and_config(
                hop_chunk(),
                byteflow::std_native_table(),
                RuntimeConfig {
                    workers: 2,
                    quantum: DEFAULT_QUANTUM,
                    mailbox: MailboxConfig::DEFAULT,
                    ..Default::default()
                },
            ) {
                Ok(rt) => rt,
                Err(e) => panic!("runtime: {e}"),
            };
            let Some(client) = rt.function_index("client") else {
                panic!("missing client");
            };
            let outcome = match rt.spawn(client, &[]) {
                Ok(h) => h.join(),
                Err(e) => panic!("spawn: {e}"),
            };
            rt.shutdown();
            assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(1))));
        });
    });
    group.finish();
}

fn countdown_chunk(immediate: bool) -> byteflow::Chunk {
    let mut program = Program::new(if immediate { "sub-imm" } else { "sub-pair" });
    program.function("main", 0, |f| {
        let n = f.load_i32(2_000);
        let head = f.label();
        let body = f.label();
        let done = f.label();
        f.bind(head);
        let finished = f.lt_imm(n, 1);
        f.branch_if_falsy(finished, body);
        f.jump(done);
        f.bind(body);
        if immediate {
            f.sub_imm(n, 1);
        } else {
            let one = f.load_i32(1);
            let next = f.sub(n, one);
            f.move_reg(n, next);
        }
        f.jump(head);
        f.bind(done);
        f.return_(n);
    });
    program.build()
}

fn sub_immediate(c: &mut Criterion) {
    let immediate = std::sync::Arc::new(countdown_chunk(true));
    let paired = std::sync::Arc::new(countdown_chunk(false));
    let mut group = c.benchmark_group("sub_immediate");
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_millis(400));
    for (name, chunk) in [("sub_imm", immediate), ("load_imm_plus_sub", paired)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut vm = match byteflow::Vm::new(
                    std::sync::Arc::clone(&chunk),
                    byteflow::NativeTable::empty(),
                    0,
                    &[],
                ) {
                    Ok(vm) => vm,
                    Err(e) => panic!("vm: {e}"),
                };
                match vm.run(20_000) {
                    byteflow::VmResult::Complete(Value::Int(0)) => {}
                    other => panic!("countdown: {other:?}"),
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, spawn_join, atomic_hop_ask, sub_immediate);
criterion_main!(benches);
