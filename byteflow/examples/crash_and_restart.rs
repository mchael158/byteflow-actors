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
    ChildSpec, Chunk, Program, RestartPolicy, RestartStrategy, Runtime, RuntimeConfig, Supervisor,
    SupervisorConfig,
};

fn boom_chunk() -> Chunk {
    let mut program = Program::new("boom");
    program.function("boom", 0, |f| f.trap(1));
    program.build()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::with_config(
        boom_chunk(),
        RuntimeConfig {
            workers: 1,
            quantum: 1_000,
            ..Default::default()
        },
    )?;
    let boom = rt.function_index("boom").ok_or("missing boom")?;
    let sup = Supervisor::with_config(
        rt.spawner(),
        SupervisorConfig {
            max_restarts: 2,
            max_period: Duration::from_secs(5),
            strategy: RestartStrategy::OneForOne,
        },
    )?;
    sup.start_child(ChildSpec::new("boom", boom).restart(RestartPolicy::OnFailure))?;

    let start = std::time::Instant::now();
    while !sup.intensity_exceeded() {
        if start.elapsed() > Duration::from_secs(2) {
            return Err("supervisor did not hit intensity in time".into());
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
    Ok(())
}
