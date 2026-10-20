//! A flow starts its own supervisor.
//!
//! [`crate::Opcode::StartChild`] used to require a host [`super::supervisor::Supervisor`]
//! that had already spawned the caller. [`crate::Opcode::StartSupervisor`]
//! starts one from bytecode, with a strategy immediate, and later
//! `StartChild` calls attach to it. The host supervisor, when present and
//! when this flow has not started its own, stays the fallback.
//!
//! The drive thread lives on [`super::runtime::Shared`] until
//! [`crate::Runtime::shutdown`], so dropping the starter flow does not join
//! that thread from a worker (that join can deadlock the last worker).

use std::sync::Mutex;

use crate::bytecode::CapRights;

use super::error::{report_fault, SpawnError};
use super::process::Flow;
use super::runtime::{RuntimeSpawner, Shared};
use super::supervisor::{RestartStrategy, Supervisor, SupervisorConfig, SupervisorLink};
use super::sync_lock;

/// Decode the `StartSupervisor` immediate: `0` one-for-one, `1` one-for-all,
/// `2` rest-for-one.
pub(crate) fn strategy_from_u8(byte: u8) -> Option<RestartStrategy> {
    match byte {
        0 => Some(RestartStrategy::OneForOne),
        1 => Some(RestartStrategy::OneForAll),
        2 => Some(RestartStrategy::RestForOne),
        _ => None,
    }
}

/// Link `StartChild` should use: the supervisor this flow started, otherwise
/// the host supervisor that started this flow.
pub(crate) fn link_for(flow: &Flow) -> Option<SupervisorLink> {
    match flow.local_supervisor.clone() {
        Some(link) => Some(link),
        None => flow.supervisor.clone(),
    }
}

pub(crate) fn start(
    shared: &std::sync::Arc<Shared>,
    flow: &mut Flow,
    strategy: u8,
) -> Result<(), String> {
    if !flow.authority.rights().contains(CapRights::SPAWN) {
        return Err("start_supervisor denied: flow lacks SPAWN right".into());
    }
    if flow.local_supervisor.is_some() {
        return Err("start_supervisor: this flow already started one".into());
    }
    let Some(strategy) = strategy_from_u8(strategy) else {
        return Err("start_supervisor: strategy must be 0..=2".into());
    };
    let spawner = RuntimeSpawner {
        shared: std::sync::Arc::clone(shared),
        chunk: flow.vm.chunk_arc(),
        natives: flow.vm.natives_arc(),
    };
    let supervisor = Supervisor::with_config(
        spawner,
        SupervisorConfig {
            strategy,
            ..SupervisorConfig::default()
        },
    )
    .map_err(|e: SpawnError| e.to_string())?;
    let link = supervisor.link();
    {
        let mut owned = sync_lock::lock(&shared.flow_supervisors, "flow_supervisor::start")
            .map_err(|e| e.to_string())?;
        owned.push(supervisor);
    }
    flow.local_supervisor = Some(link);
    Ok(())
}

/// Join every supervisor a flow started. Call after workers have stopped.
pub(crate) fn shutdown_all(owned: &Mutex<Vec<Supervisor>>) {
    let supervisors = match sync_lock::lock(owned, "flow_supervisor::shutdown_all") {
        Ok(mut list) => std::mem::take(&mut *list),
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    for supervisor in supervisors {
        supervisor.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use crate::{FlowOutcome, Program, RestartPolicy, RestartStrategy, Runtime, Value};

    #[test]
    fn start_child_without_a_host_supervisor() -> Result<(), Box<dyn std::error::Error>> {
        let mut program = Program::new("local-sup");
        let child = program.function("child", 0, |f| {
            let n = f.load_i32(7);
            f.return_(n);
        });
        program.function("main", 0, |f| {
            f.start_supervisor(RestartStrategy::OneForOne);
            let cap = f.start_child(child, RestartPolicy::Never);
            f.return_(cap);
        });
        let rt = Runtime::new(program.build())?;
        let outcome = rt.spawn(1, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Cap(_)) => Ok(()),
            other => Err(format!("expected a child cap, got {other:?}").into()),
        }
    }
}
