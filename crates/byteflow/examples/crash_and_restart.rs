//! A child that always traps; the supervisor brings it back until intensity
//! (2 restarts in 5s) is exceeded.
//!
//! Built only with the public [`byteflow`] facade.
//!
//! ```text
//! cargo run -p byteflow-actors --example crash_and_restart
//! ```

use std::thread;
use std::time::Duration;

use byteflow::{
    ChildSpec, Chunk, MailboxConfig, Program, RestartPolicy, RestartStrategy, Runtime,
    RuntimeConfig, Supervisor, SupervisorConfig,
};

fn boom_chunk() -> Chunk {
    let mut program = Program::new("boom");
    program.function("boom", 0, |f| f.trap(1));
    program.build()
}

fn main() {
    let rt = match Runtime::with_config(
        boom_chunk(),
        RuntimeConfig {
            workers: 1,
            quantum: 1_000,
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
    let Some(boom) = rt.function_index("boom") else {
        eprintln!("missing boom");
        std::process::exit(1);
    };
    let sup = match Supervisor::with_config(
        rt.spawner(),
        SupervisorConfig {
            max_restarts: 2,
            max_period: Duration::from_secs(5),
            strategy: RestartStrategy::OneForOne,
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("supervisor: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = sup.start_child(ChildSpec::new("boom", boom).restart(RestartPolicy::OnFailure))
    {
        eprintln!("start_child: {e}");
        std::process::exit(1);
    }

    let start = std::time::Instant::now();
    while !sup.intensity_exceeded() {
        if start.elapsed() > Duration::from_secs(2) {
            eprintln!("supervisor did not hit intensity in time");
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(5));
    }

    let metrics = rt.metrics();
    println!(
        "intensity exceeded after {} failures",
        metrics.processes_failed
    );
    println!("{metrics}");
    sup.shutdown();
    rt.shutdown();
}
