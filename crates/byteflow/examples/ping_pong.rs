//! Two flows, one Atomic Hop round-trip (`Value::Message`).
//!
//! Assembles the program with [`byteflow::Program`] / [`byteflow::Fn`] and
//! runs it on [`byteflow::Runtime`] — the public host API only.
//!
//! ```text
//! cargo run -p byteflow-actors --example ping_pong
//! ```

use byteflow::{
    Chunk, FlowOutcome, MailboxConfig, Program, Runtime, RuntimeConfig, Value, DEFAULT_QUANTUM,
    std_native_table,
};

const TAG_PING: i32 = 10;
const TAG_PONG: i32 = 11;

fn ping_pong_chunk() -> Chunk {
    let mut program = Program::new("ping-pong");
    let pong = program.function("pong", 0, |f| {
        let msg = f.receive();
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_PONG, payload);
        f.exit(payload);
    });
    program.function("main", 0, |f| {
        let child = f.spawn(pong, 0);
        let payload = f.load_i32(1);
        let req = f.hop_fresh(TAG_PING, payload);
        let rid = f.hop_request_id(req);
        f.send(child, req);
        let reply = f.receive_match_corr_imm(TAG_PONG as u16, rid);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    program.build()
}

fn main() {
    let rt = match Runtime::with_natives_and_config(
        ping_pong_chunk(),
        std_native_table(),
        RuntimeConfig {
            workers: 1,
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
        FlowOutcome::Completed(Value::Int(2)) => {
            println!("pong replied 2 (Atomic Hop)");
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
