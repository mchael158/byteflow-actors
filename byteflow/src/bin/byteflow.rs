use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use byteflow::samples::{self, add_forty_two, ping_pong};
use byteflow::{
    assemble, decode, disassemble, encode, std_native_table, verify, FlowOutcome, NativeTable,
    Runtime, RuntimeConfig,
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
        "version" | "-V" | "--version" => {
            println!("byteflow {}", env!("CARGO_PKG_VERSION"));
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
        "asm" => {
            let input = args.next();
            let flag = args.next();
            let output = args.next();
            match (input, flag.as_deref(), output) {
                (Some(input), Some("-o"), Some(output)) => cmd_asm(&input, &output),
                _ => usage("byteflow asm <file> -o <out.bf>"),
            }
        }
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
            banner_error(&format!("unknown command {other:?}"));
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
    let t = Palette::new();
    eprintln!("{} {}", t.paint(CRIMSON, "usage:"), t.paint(FLARE, msg));
    Err(())
}

fn banner_error(msg: &str) {
    let t = Palette::new();
    eprintln!("{} {}", t.paint(CRIMSON, "error:"), t.paint(FLARE, msg));
}

fn print_help() {
    let t = Palette::new();
    let cmd = |name: &str, spec: &str, what: &str| {
        format!(
            "  {}  {}  {}",
            t.paint(FLARE, &format!("{name:<6}")),
            t.paint(HOT, &format!("{spec:<22}")),
            what
        )
    };
    let lines = [
        format!(
            "{} {}  verify, disassemble and run .bf modules",
            t.paint(CRIMSON, "byteflow"),
            env!("CARGO_PKG_VERSION"),
        ),
        String::new(),
        t.paint(CRIMSON, "modules"),
        cmd("run", "<file.bf> [function]", "run `main` (or the named function)"),
        cmd("verify", "<file.bf>", "run the static verifier"),
        cmd("disasm", "<file.bf>", "print the bytecode listing"),
        cmd("asm", "<listing> -o <out.bf>", "assemble a text listing"),
        String::new(),
        t.paint(CRIMSON, "built-in demos"),
        cmd("demo", "<name>", "ping-pong atomic selective selective-after"),
        cmd("", "", "match-eq exit dict ask ask-timeout server-loop"),
        cmd("", "", "monitor alive add"),
        cmd("pack", "<demo> <out.bf>", "write a demo as a .bf module"),
        String::new(),
        format!(
            "{}  run/hop demos attach the std natives (print=0 · now_ms=1 · make_msg=2 · … · msg_reply_cap=7)",
            t.paint(SCARLET, "note")
        ),
    ];
    for line in lines {
        eprintln!("{line}");
    }
}

/// Standard 16-color ANSI. A terminal palette remaps these codes.
const CRIMSON: &str = "1;31";
const SCARLET: &str = "91";
const HOT: &str = "93";
const FLARE: &str = "97";

struct Palette {
    on: bool,
}

impl Palette {
    fn new() -> Self {
        Self {
            on: env::var_os("NO_COLOR").is_none() && io::stderr().is_terminal(),
        }
    }

    fn paint(&self, codes: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{codes}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

fn load_bf(path: &str) -> Result<byteflow::Chunk, ()> {
    let bytes = fs::read(path).map_err(|e| {
        banner_error(&format!("read {path}: {e}"));
    })?;
    decode(&bytes).map_err(|e| {
        banner_error(&format!("{path}: {e}"));
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
            banner_error(&format!("verify failed: {e}"));
            Err(())
        }
    }
}

fn cmd_asm(input: &str, output: &str) -> Result<(), ()> {
    let text = fs::read_to_string(input).map_err(|e| {
        eprintln!("read {input}: {e}");
    })?;
    let chunk = assemble(&text).map_err(|e| {
        eprintln!("{input}: {e}");
    })?;
    let bytes = encode(&chunk);
    if let Some(dir) = Path::new(output).parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| eprintln!("mkdir: {e}"))?;
        }
    }
    fs::write(output, &bytes).map_err(|e| eprintln!("write {output}: {e}"))?;
    println!("wrote {output} ({} bytes)", bytes.len());
    Ok(())
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
        | "selective-after"
        | "selective-timeout"
        | "alive"
        | "process-alive"
        | "match-eq"
        | "receive-eq"
        | "exit"
        | "exit-signal"
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
        "selective-after" => Ok(samples::selective_receive_after()),
        "selective-timeout" => Ok(samples::selective_receive_timeout()),
        "alive" | "process-alive" => Ok(samples::process_alive()),
        "match-eq" | "receive-eq" => Ok(samples::receive_match_eq()),
        "exit" | "exit-signal" => Ok(samples::exit_signal()),
        "dict" | "process-dict" => Ok(samples::process_dict()),
        "ask" | "ask-reply" => Ok(samples::ask_reply()),
        "ask-timeout" => Ok(samples::ask_timeout_expires()),
        "ask-exit" | "ask-target-exits" => Ok(samples::ask_target_exits()),
        "server-loop" | "server" => Ok(samples::server_loop()),
        "monitor" | "monitor-down" => Ok(samples::monitor_down()),
        "add" | "add-forty-two" | "42" => Ok(add_forty_two()),
        "boom" => Ok(samples::boom()),
        other => {
            eprintln!("unknown demo {other:?} (try ping-pong, atomic, selective, selective-after, match-eq, exit, dict, ask, server-loop, monitor, alive, add)");
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
        banner_error(&format!("verify failed: {e}"));
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
            banner_error(&format!("flow failed: {err}"));
            eprintln!("{metrics}");
            return Err(());
        }
    }
    eprintln!("{metrics}");
    Ok(())
}
