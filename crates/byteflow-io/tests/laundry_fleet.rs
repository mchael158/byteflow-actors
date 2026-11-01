use std::sync::Arc;
use std::time::Duration;

use byteflow::{FlowOutcome, NullSink, Program, Runtime, Value};
use byteflow_io::laundry::{
    abort_token, bootstrap, bootstrap_with_delay, event_tag, host_op, topics, EspCommand,
    FleetError, MachineId, MachineState, MACHINE_COUNT,
};
use byteflow_io::{build_native_table, EventRouter, IoConfig, IoDriver, IoErrorKind};

#[test]
fn machine_id_accepts_only_one_through_fifty_six() {
    assert!(MachineId::new(0).is_none());
    assert!(MachineId::new(1).is_some());
    assert!(MachineId::new(MACHINE_COUNT).is_some());
    assert!(MachineId::new(MACHINE_COUNT + 1).is_none());
    assert_eq!(MachineId::all().count(), usize::from(MACHINE_COUNT));
}

#[test]
fn fleet_binds_all_fifty_six_machines() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _driver) = bootstrap(IoConfig {
        worker_threads: 2,
        queue_capacity: 64,
        ..IoConfig::default()
    })?;

    let mut program = Program::new("fleet-bind");
    program.function("washer", 0, |f| {
        let n = f.load_i32(1);
        f.return_(n);
    });
    let natives = build_native_table(io.driver().clone(), Arc::new(NullSink))?;
    let runtime = Runtime::with_natives_and_config(
        program.build(),
        natives,
        io.runtime_config(Arc::new(NullSink)),
    )?;

    for id in MachineId::all() {
        let handle = runtime.spawn(0, &[])?;
        fleet.bind(id, handle.id())?;
        let _ = handle.join_timeout(Duration::from_secs(1));
    }

    assert_eq!(fleet.bound_count()?, usize::from(MACHINE_COUNT));
    runtime.shutdown();
    io.shutdown()?;
    Ok(())
}

#[test]
fn deliver_event_requires_bound_flow() -> Result<(), Box<dyn std::error::Error>> {
    let (_io, fleet, _driver) = bootstrap(IoConfig::default())?;
    let mut program = Program::new("unbound");
    program.function("main", 0, |f| {
        let n = f.load_i32(0);
        f.return_(n);
    });
    let runtime = Arc::new(Runtime::new(program.build())?);
    let events = EventRouter::new(Arc::clone(&runtime));
    let machine = MachineId::new(3).expect("valid");

    let err = fleet.deliver_event(&events, machine, event_tag::COIN_INSERTED, Value::Int(100));
    assert!(matches!(err, Err(FleetError::Unbound)));

    drop(events);
    drop(runtime);
    Ok(())
}

#[test]
fn payment_then_start_via_host_await() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _) = bootstrap(IoConfig::default())?;
    let machine = MachineId::new(2).expect("valid");
    let driver = io.driver().clone();
    let cancel = byteflow_io::CancellationToken::default();

    let credited = driver.execute_host_await(
        host_op::PAYMENT_STATUS,
        EspCommand::credit_args(machine, 500)?,
        &cancel,
    )?;
    assert_eq!(credited, Value::Int(500));
    assert_eq!(fleet.lookup(machine)?.state, MachineState::Credited);

    let started = driver.execute_host_await(
        host_op::START_MACHINE_CYCLE_AND_WAIT,
        EspCommand::start_cycle_args(machine),
        &cancel,
    )?;
    assert_eq!(started, Value::Int(i64::from(machine.get())));
    assert_eq!(fleet.lookup(machine)?.state, MachineState::Running);
    io.shutdown()?;
    Ok(())
}

#[test]
fn host_await_start_cycle_from_bytecode() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _) = bootstrap(IoConfig::default())?;
    let machine = MachineId::new(1).expect("valid");
    fleet.set_state(machine, MachineState::Credited)?;

    let mut program = Program::new("start-cycle-host-credit");
    program.function("main", 0, |f| {
        let machine_arg = f.load_i32(i32::from(machine.get()));
        let _ticket = f.host_await(host_op::START_MACHINE_CYCLE_AND_WAIT, machine_arg);
        let msg = f.receive();
        f.return_(msg);
    });

    let natives = build_native_table(io.driver().clone(), Arc::new(NullSink))?;
    let runtime = Runtime::with_natives_and_config(
        program.build(),
        natives,
        io.runtime_config(Arc::new(NullSink)),
    )?;
    let handle = runtime.spawn(0, &[])?;
    fleet.bind(machine, handle.id())?;

    let outcome = handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("start cycle timed out")?;
    runtime.shutdown();
    io.shutdown()?;

    assert!(matches!(
        outcome,
        FlowOutcome::Completed(Value::Message(ref msg))
            if matches!(msg.payload.as_ref(), Value::Int(1))
    ));
    assert_eq!(fleet.lookup(machine)?.state, MachineState::Running);
    Ok(())
}

#[test]
fn ota_chunk_fails_when_cancelled() -> Result<(), Box<dyn std::error::Error>> {
    let (io, _fleet, driver) =
        bootstrap_with_delay(IoConfig::default(), Duration::from_millis(200))?;
    let machine = MachineId::new(5).expect("valid");
    let cancel = byteflow_io::CancellationToken::default();
    abort_token(&cancel);

    let args = EspCommand::ota_chunk_args(machine, 0, vec![1, 2, 3, 4])?;
    let result = driver.execute_host_await(host_op::ESP_OTA_CHUNK, args, &cancel);
    let err = result.expect_err("cancelled OTA must fail");
    assert_eq!(err.kind(), IoErrorKind::Cancelled);
    io.shutdown()?;
    Ok(())
}

#[test]
fn ota_chunk_advances_sequence() -> Result<(), Box<dyn std::error::Error>> {
    let (io, _, driver) = bootstrap(IoConfig::default())?;
    let machine = MachineId::new(8).expect("valid");
    let cancel = byteflow_io::CancellationToken::default();

    let first = driver.execute_host_await(
        host_op::ESP_OTA_CHUNK,
        EspCommand::ota_chunk_args(machine, 0, b"chunk0".to_vec())?,
        &cancel,
    )?;
    assert_eq!(first, Value::Int(0));

    let second = driver.execute_host_await(
        host_op::ESP_OTA_CHUNK,
        EspCommand::ota_chunk_args(machine, 1, b"chunk1".to_vec())?,
        &cancel,
    )?;
    assert_eq!(second, Value::Int(1));

    let bad = driver.execute_host_await(
        host_op::ESP_OTA_CHUNK,
        EspCommand::ota_chunk_args(machine, 99, b"nope".to_vec())?,
        &cancel,
    );
    assert_eq!(
        bad.expect_err("bad OTA seq").kind(),
        IoErrorKind::InvalidArgs
    );
    io.shutdown()?;
    Ok(())
}

#[test]
fn topics_parse_washer_id() {
    let id = MachineId::new(42).expect("valid");
    assert_eq!(topics::parse_machine(&topics::cmd(id)), Some(id));
    assert_eq!(topics::parse_machine("other/topic"), None);
}

#[test]
fn event_router_coin_to_bound_washer() -> Result<(), Box<dyn std::error::Error>> {
    let (_io, fleet, _) = bootstrap(IoConfig::default())?;
    let machine = MachineId::new(4).expect("valid");

    let mut program = Program::new("coin-event");
    program.function("main", 0, |f| {
        let msg = f.receive();
        f.return_(msg);
    });
    let runtime = Arc::new(Runtime::new(program.build())?);
    let handle = runtime.spawn(0, &[])?;
    fleet.bind(machine, handle.id())?;

    let events = EventRouter::new(Arc::clone(&runtime));
    fleet.deliver_event(&events, machine, event_tag::COIN_INSERTED, Value::Int(250))?;

    let outcome = handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("coin event timed out")?;
    drop(events);
    drop(runtime);

    match outcome {
        FlowOutcome::Completed(Value::Message(msg))
            if msg.tag == event_tag::COIN_INSERTED
                && matches!(msg.payload.as_ref(), Value::Int(250)) =>
        {
            Ok(())
        }
        other => Err(format!("unexpected outcome {other:?}").into()),
    }
}
