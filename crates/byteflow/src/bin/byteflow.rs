use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use byteflow::samples::{self, add_forty_two, ping_pong};
use byteflow::{
    decode, disassemble, encode, std_native_table, verify, FlowOutcome, NativeTable, Runtime,
    RuntimeConfig,
};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => "help".into(),
    };
    let result = match cmd.as_str() {
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "verify" => match args.next() {
            Some(path) => cmd_verify(&path),
            None => usage("byteflow verify <file.bf>"),
        },
        "disasm" => match args.next() {
            Some(path) => cmd_disasm(&path),
            None => usage("byteflow disasm <file.bf>"),
        },
        "run" => match args.next() {
            Some(path) => cmd_run(&path, args.next().as_deref()),
            None => usage("byteflow run <file.bf> [function]"),
        },
        "pack" => match (args.next(), args.next()) {
            (Some(demo), Some(out)) => cmd_pack(&demo, &out),
            _ => usage("byteflow pack <demo> <out.bf>"),
        },
        "demo" => {
            let demo = match args.next() {
                Some(d) => d,
                None => "ping-pong".into(),
            };
            cmd_demo(&demo)
        }
        other => {
            eprintln!("unknown command {other:?}");
            print_help();
            Err(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn usage(msg: &str) -> Result<(), ()> {
    eprintln!("usage: {msg}");
    Err(())
}

fn print_help() {
    eprintln!(
        "\
byteflow — verify, disassemble and run .bf modules (assembled via Program)

USAGE:
    byteflow demo [ping-pong|atomic|selective|ask|ask-timeout|server-loop|monitor|add]
    byteflow pack  <ping-pong|atomic|ask|add> <out.bf>
    byteflow verify <file.bf>
    byteflow disasm <file.bf>
    byteflow run    <file.bf> [function]

`run` and hop demos attach the std native table (print=0, now_ms=1, make_msg=2, …, msg_reply_cap=7).
Every `Send` is an Atomic Hop (`Value::Message` only).
"
    );
}

fn load_bf(path: &str) -> Result<byteflow::Chunk, ()> {
    let bytes = fs::read(path).map_err(|e| {
        eprintln!("read {path}: {e}");
    })?;
    decode(&bytes).map_err(|e| {
        eprintln!("{path}: {e}");
    })
}

fn cmd_verify(path: &str) -> Result<(), ()> {
    let chunk = load_bf(path)?;
    match verify(&chunk) {
        Ok(()) => {
            println!(
                "ok  {}  {} functions  {} instructions",
                chunk.name,
                chunk.functions.len(),
                chunk.code.len()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("verify failed: {e}");
            Err(())
        }
    }
}

fn cmd_disasm(path: &str) -> Result<(), ()> {
    let chunk = load_bf(path)?;
    print!("{}", disassemble(&chunk));
    Ok(())
}

fn cmd_run(path: &str, function: Option<&str>) -> Result<(), ()> {
    let chunk = load_bf(path)?;
    run_chunk(&chunk, function, std_native_table())
}

fn cmd_pack(demo: &str, out: &str) -> Result<(), ()> {
    let chunk = demo_chunk(demo)?;
    let bytes = encode(&chunk);
    if let Some(dir) = Path::new(out).parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| eprintln!("mkdir: {e}"))?;
        }
    }
    fs::write(out, bytes).map_err(|e| eprintln!("write {out}: {e}"))?;
    println!(
        "wrote {out} ({} bytes, chunk {:?})",
        match fs::metadata(out) {
            Ok(m) => m.len(),
            Err(_) => 0,
        },
        chunk.name
    );
    Ok(())
}

fn cmd_demo(name: &str) -> Result<(), ()> {
    let chunk = demo_chunk(name)?;
    // Atomic Hop demos (`ping-pong`) need make_msg / msg_*; `add` does not.
    let natives = match name {
        "ping-pong"
        | "ping_pong"
        | "atomic"
        | "atomic-actors"
        | "atomic-request-reply"
        | "selective"
        | "selective-receive"
        | "ask"
        | "ask-reply"
        | "ask-timeout"
        | "server-loop"
        | "server"
        | "monitor"
        | "monitor-down"
        | "ask-exit"
        | "ask-target-exits" => std_native_table(),
        _ => NativeTable::empty(),
    };
    run_chunk(&chunk, Some("main"), natives)
}

fn demo_chunk(name: &str) -> Result<byteflow::Chunk, ()> {
    match name {
        "ping-pong" | "ping_pong" => Ok(ping_pong()),
        "atomic" | "atomic-actors" => Ok(samples::atomic_actors()),
        "atomic-request-reply" => Ok(samples::atomic_request_reply()),
        "selective" | "selective-receive" => Ok(samples::selective_receive()),
        "ask" | "ask-reply" => Ok(samples::ask_reply()),
        "ask-timeout" => Ok(samples::ask_timeout_expires()),
        "ask-exit" | "ask-target-exits" => Ok(samples::ask_target_exits()),
        "server-loop" | "server" => Ok(samples::server_loop()),
        "monitor" | "monitor-down" => Ok(samples::monitor_down()),
        "add" | "add-forty-two" | "42" => Ok(add_forty_two()),
        "boom" => Ok(samples::boom()),
        other => {
            eprintln!("unknown demo {other:?} (try ping-pong, atomic, selective, ask, ask-timeout, server-loop, monitor, add)");
            Err(())
        }
    }
}

fn run_chunk(
    chunk: &byteflow::Chunk,
    function: Option<&str>,
    natives: Arc<NativeTable>,
) -> Result<(), ()> {
    if let Err(e) = verify(chunk) {
        eprintln!("verify failed: {e}");
        return Err(());
    }

    let rt = Runtime::with_natives_and_config(
        chunk.clone(),
        natives,
        RuntimeConfig {
            workers: 1,
            quantum: byteflow::DEFAULT_QUANTUM,
            mailbox: byteflow::MailboxConfig::DEFAULT,
            ..Default::default()
        },
    )
    .map_err(|e| eprintln!("runtime: {e}"))?;

    let idx = match function {
        Some(name) => match rt.function_index(name) {
            Some(i) => i,
            None => {
                eprintln!("no function named {name:?}");
                return Err(());
            }
        },
        None => match rt.function_index("main") {
            Some(i) => i,
            None if !chunk.functions.is_empty() => 0,
            None => {
                eprintln!("chunk has no functions");
                return Err(());
            }
        },
    };

    let outcome = rt
        .spawn(idx, &[])
        .map_err(|e| eprintln!("spawn: {e}"))?
        .join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(v) => println!("{v}"),
        FlowOutcome::Failed(err) => {
            eprintln!("Flow failed: {err}");
            eprintln!("{metrics}");
            return Err(());
        }
    }
    eprintln!("{metrics}");
    Ok(())
}
