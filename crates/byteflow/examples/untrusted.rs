//! Run one untrusted module with the sandbox that already exists.
//!
//! `Runtime::new` stays permissive. This example opts into
//! [`RuntimeConfig::sandbox`], an empty native table, attestation, and a
//! bytecode `Spawn` with rights `NONE`.

use byteflow::{
    assemble, decode, encode, fingerprint_bf, verify, FlowOutcome, NativeTable, QuotaConfig,
    Runtime, RuntimeConfig,
};

const GUEST: &str = r#"
.chunk "guest"
.abi 5
.fn main entry=0 arity=0 regs=4
.fn spin entry=4 arity=0 regs=1
0000  Spawn r0, fn[1], argc=0, rights=0x0
0001  Delegate r1, r0, rights=0x0
0002  LoadImm r2, 1
0003  Return r2
0004  Jump -1
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match assemble("this is not a module") {
        Err(err) => println!("assembler rejected invalid text: {err}"),
        Ok(_) => return Err("invalid text was assembled".into()),
    }
    match decode(b"not a module") {
        Err(err) => println!("decoder rejected invalid bytes: {err}"),
        Ok(_) => return Err("invalid bytes were decoded".into()),
    }

    let chunk = assemble(GUEST)?;
    verify(&chunk)?;
    let bytes = encode(&chunk);
    let path = std::env::temp_dir().join("byteflow-untrusted-guest.bf");
    std::fs::write(&path, &bytes)?;
    let loaded = std::fs::read(&path)?;
    let loaded_chunk = decode(&loaded)?;
    verify(&loaded_chunk)?;
    println!("loaded {} ({} bytes)", path.display(), loaded.len());

    let digest = fingerprint_bf(&loaded);
    let mut config = RuntimeConfig::sandbox();
    config.workers = 1;
    config.quota = QuotaConfig {
        cpu_budget: 5_000,
        ..QuotaConfig::sandbox()
    };
    let rt = Runtime::with_attested(&loaded, digest, NativeTable::empty(), config)?;
    let outcome = rt
        .spawn(0, &[])?
        .join_timeout(std::time::Duration::from_secs(2))
        .ok_or("guest timed out")?;
    let failed = wait_for_child_failure(&rt);
    println!("main outcome: {outcome:?}");
    println!(
        "metrics spawned={} completed={} failed={}",
        rt.metrics().processes_spawned,
        rt.metrics().processes_completed,
        rt.metrics().processes_failed
    );
    rt.shutdown();
    println!("host process still running");
    match outcome {
        FlowOutcome::Completed(byteflow::Value::Int(1)) if failed => Ok(()),
        other => {
            Err(format!("expected main=1 and a failed child, got {other:?} failed={failed}").into())
        }
    }
}

fn wait_for_child_failure(rt: &Runtime) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if rt.metrics().processes_failed >= 1 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    rt.metrics().processes_failed >= 1
}
