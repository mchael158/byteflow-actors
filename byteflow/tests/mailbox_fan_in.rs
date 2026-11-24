//! Fan-in under backpressure: many flows, running in parallel, each send one
//! hop to a single collector whose inbox is far smaller than the number of
//! senders.
//!
//! Regression: a sender that found both the inbox and the waiter list full
//! (more than `2 × capacity` senders in flight) was finalized with
//! `waiting senders full`, so the collector waited forever for its hop.
//! Every sender must now park and be admitted in turn.

use std::time::Duration;

use byteflow::{
    std_native_table, Chunk, FlowOutcome, MailboxCapacity, MailboxConfig, OverflowPolicy, Program,
    Runtime, RuntimeConfig, Value,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TAG_DONE: i32 = 1;

/// `main` spawns `senders` workers that each send one hop back, then
/// receives all of them and returns the count.
fn fan_in(senders: i32) -> Chunk {
    let mut p = Program::new("fan-in");
    let worker = p.function("worker", 1, |f| {
        let collector = f.reg(0);
        let one = f.load_i32(1);
        let hop = f.hop_fresh(TAG_DONE, one);
        f.send(collector, hop);
        f.return_(one);
    });
    p.function("main", 0, |f| {
        let me = f.self_cap();
        let limit = f.load_i32(senders);
        let spawned = f.load_i32(0);
        f.while_lt(spawned, limit, |f| {
            f.spawn(worker, &[me]);
            f.add_imm(spawned, 1);
        });
        let got = f.load_i32(0);
        f.while_lt(got, limit, |f| {
            f.receive_match_imm(TAG_DONE as u16);
            f.add_imm(got, 1);
        });
        f.return_(got);
    });
    p.build()
}

fn run(senders: i32, workers: usize, mailbox: MailboxConfig) -> TestResult {
    let rt = Runtime::with_natives_and_config(
        fan_in(senders),
        std_native_table(),
        RuntimeConfig {
            workers,
            mailbox,
            ..Default::default()
        },
    )?;
    let main = rt.function_index("main").ok_or("missing main")?;
    let outcome = rt
        .spawn(main, &[])?
        .join_timeout(Duration::from_secs(30))
        .ok_or_else(|| format!("collector hung: senders={senders} workers={workers}"))?;
    let metrics = rt.metrics();
    rt.shutdown();

    assert!(
        matches!(outcome, FlowOutcome::Completed(Value::Int(n)) if n == i64::from(senders)),
        "senders={senders} workers={workers}: {outcome:?}"
    );
    assert_eq!(
        metrics.processes_failed, 0,
        "no sender may fail under backpressure: {metrics}"
    );
    Ok(())
}

#[test]
fn many_senders_into_the_default_mailbox() -> TestResult {
    // 256-hop inbox. 512 senders in flight used to be the ceiling.
    for (senders, workers) in [(2_000, 1), (2_000, 4), (10_000, 8)] {
        run(senders, workers, MailboxConfig::DEFAULT)?;
    }
    Ok(())
}

#[test]
fn many_senders_into_a_tiny_mailbox() -> TestResult {
    let cap = MailboxCapacity::new(2).ok_or("capacity")?;
    let tiny = MailboxConfig::new(cap, OverflowPolicy::Reject);
    for _ in 0..5 {
        run(500, 4, tiny)?;
    }
    Ok(())
}
