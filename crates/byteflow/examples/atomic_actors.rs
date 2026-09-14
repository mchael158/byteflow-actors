//! Atomic Hop: server loop + two `Ask` clients.
//!
//! Built only with the public [`byteflow`] facade (`Program` / `Fn` / `Runtime`).
//!
//! ```text
//! cargo run -p byteflow-actors --example atomic_actors
//!
//! # scheduler stderr (spawn/send/recv/finish) — separate from print's stdout
//! $env:BYTEFLOW_LOG="info"
//! cargo run -p byteflow-actors --example atomic_actors
//! ```

use byteflow::{
    Chunk, FlowOutcome, MailboxConfig, Program, Runtime, RuntimeConfig, Value, DEFAULT_QUANTUM,
    std_native_table,
};

const TAG_REQ: i32 = 1;
const TAG_REP: i32 = 2;
const ROUNDS: i32 = 8;

fn atomic_actors_chunk() -> Chunk {
    let mut program = Program::new("atomic-actors");
    let server = program.function("server", 0, |f| {
        let loop_lbl = f.label();
        f.bind(loop_lbl);
        let req = f.receive_match_imm(TAG_REQ as u16);
        let payload = f.hop_payload(req);
        f.add_imm(payload, 1);
        f.send_reply(req, TAG_REP, payload);
        f.jump(loop_lbl);
    });
    let client = program.function("client", 2, |f| {
        let server_cap = f.reg(0);
        let parent_cap = f.reg(1);
        let acc = f.load_i32(0);
        let i = f.load_i32(0);
        let n = f.load_i32(ROUNDS);
        f.while_lt(i, n, |f| {
            let req = f.hop_fresh(TAG_REQ, i);
            let reply = f.ask(server_cap, req);
            let got = f.hop_payload(reply);
            let sum = f.add(acc, got);
            f.mov(acc, sum);
            f.add_imm(i, 1);
        });
        let done = f.hop_fresh(TAG_REP, acc);
        f.send(parent_cap, done);
        f.return_(acc);
    });
    program.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let me = f.self_cap();
        let w1 = f.window(3);
        f.mov(w1.at(1), server_cap);
        f.mov(w1.at(2), me);
        f.spawn_at(w1.at(0), client, 2);
        let w2 = f.window(3);
        f.mov(w2.at(1), server_cap);
        f.mov(w2.at(2), me);
        f.spawn_at(w2.at(0), client, 2);
        let a = f.receive_match_imm(TAG_REP as u16);
        let b = f.receive_match_imm(TAG_REP as u16);
        let pa = f.hop_payload(a);
        let pb = f.hop_payload(b);
        let out = f.add(pa, pb);
        f.return_(out);
    });
    program.build()
}

fn main() {
    let rt = match Runtime::with_natives_and_config(
        atomic_actors_chunk(),
        std_native_table(),
        RuntimeConfig {
            workers: 2,
            quantum: DEFAULT_QUANTUM,
            mailbox: MailboxConfig::DEFAULT,
            ..Default::default()
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            std::process::exit(1);
        }
    };
    let Some(main_fn) = rt.function_index("main") else {
        eprintln!("missing main");
        std::process::exit(1);
    };
    let handle = match rt.spawn(main_fn, &[]) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("spawn: {e}");
            std::process::exit(1);
        }
    };
    let outcome = handle.join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(72)) => {
            println!("atomic actors ok: two clients × 8 Ask = 72");
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
