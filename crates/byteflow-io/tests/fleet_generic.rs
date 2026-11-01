//! Generic fleet profile — not laundry-specific.

use std::sync::Arc;
use std::time::Duration;

use byteflow::{FlowOutcome, NullSink, Program, Runtime, Value};
use byteflow_io::fleet::{
    bootstrap, bootstrap_composite, dispatch_queued_command, event_tag, host_op, DeviceCommand,
    DeviceState, FleetError, FleetProfile, QueuedDispatch, DEFAULT_FAST_QUEUE_CAPACITY,
};
use byteflow_io::{build_native_table, EventRouter, IoCommand, IoConfig};

#[test]
fn generic_profile_topics_and_arm_start() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _) = bootstrap(FleetProfile::generic_site(), IoConfig::default())?;
    let device = fleet.device_id(3).expect("valid");
    assert_eq!(
        byteflow_io::fleet::cmd(fleet.profile(), device),
        "site/device/3/cmd"
    );

    let cancel = byteflow_io::CancellationToken::default();
    let driver = io.driver().clone();
    let units = driver.execute_host_await(
        host_op::APPLY_CREDIT,
        DeviceCommand::credit_args(device, 100)?,
        &cancel,
    )?;
    assert_eq!(units, Value::Int(100));
    assert_eq!(fleet.lookup(device)?.state, DeviceState::Armed);

    let started = driver.execute_host_await(
        host_op::START_ACTION_AND_WAIT,
        DeviceCommand::start_args(device),
        &cancel,
    )?;
    assert_eq!(started, Value::Int(3));
    assert_eq!(fleet.lookup(device)?.state, DeviceState::Busy);
    io.shutdown()?;
    Ok(())
}

#[test]
fn custom_small_fleet_capacity() -> Result<(), Box<dyn std::error::Error>> {
    let profile = FleetProfile {
        max_devices: 4,
        topic_root: "lab/node".to_owned(),
        label: "node".to_owned(),
    };
    let (io, fleet, _) = bootstrap(profile, IoConfig::default())?;
    assert!(fleet.device_id(0).is_none());
    assert!(fleet.device_id(4).is_some());
    assert!(fleet.device_id(5).is_none());
    assert_eq!(fleet.all_ids().count(), 4);
    io.shutdown()?;
    Ok(())
}

#[test]
fn composite_dispatch_routes_gpio_to_host() -> Result<(), Box<dyn std::error::Error>> {
    let (io, _fleet, receiver, fleet_driver) =
        bootstrap_composite(FleetProfile::generic_site(), IoConfig::default(), 4)?;
    io.driver().try_submit(IoCommand::GpioWrite {
        pin: 7,
        high: false,
    })?;
    let queued = receiver.try_recv()?;
    match dispatch_queued_command(&fleet_driver, queued)? {
        QueuedDispatch::HostDevice(IoCommand::GpioWrite { pin, high }) => {
            assert_eq!(pin, 7);
            assert!(!high);
        }
        other => return Err(format!("expected HostDevice GPIO, got {other:?}").into()),
    }
    io.shutdown()?;
    Ok(())
}

#[test]
fn composite_dispatch_mqtt_reaches_in_memory_transport() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, receiver, fleet_driver) = bootstrap_composite(
        FleetProfile::generic_site(),
        IoConfig::default(),
        DEFAULT_FAST_QUEUE_CAPACITY,
    )?;
    let device = fleet.device_id(5).expect("valid");
    let topic = byteflow_io::fleet::cmd(fleet.profile(), device);

    let natives = build_native_table(io.driver().clone(), Arc::new(NullSink))?;
    let mqtt = natives
        .get(byteflow_io::native_index::MQTT_PUBLISH_FAST)
        .expect("mqtt native");
    mqtt(&[Value::str(topic), Value::Int(1), Value::Int(0)])?;

    let queued = receiver.try_recv()?;
    match dispatch_queued_command(&fleet_driver, queued)? {
        QueuedDispatch::FleetFast { .. } => {}
        other => return Err(format!("expected FleetFast, got {other:?}").into()),
    }
    assert_eq!(fleet_driver.in_memory_published()?.len(), 1);
    io.shutdown()?;
    Ok(())
}

#[test]
fn composite_host_await_start_action_from_bytecode() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _receiver, _fleet_driver) =
        bootstrap_composite(FleetProfile::generic_site(), IoConfig::default(), 8)?;
    let device = fleet.device_id(1).expect("valid");
    fleet.set_state(device, DeviceState::Armed)?;

    let mut program = Program::new("composite-bridge-start");
    program.function("main", 0, |f| {
        let device_arg = f.load_i32(1);
        let _ticket = f.host_await(host_op::START_ACTION_AND_WAIT, device_arg);
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
    fleet.bind(device, handle.id())?;

    let outcome = handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("composite host_await timed out")?;
    runtime.shutdown();
    io.shutdown()?;

    assert!(matches!(
        outcome,
        FlowOutcome::Completed(Value::Message(ref msg))
            if matches!(msg.payload.as_ref(), Value::Int(1))
    ));
    assert_eq!(fleet.lookup(device)?.state, DeviceState::Busy);
    Ok(())
}

#[test]
fn deliver_event_reaches_bound_flow() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _) = bootstrap(FleetProfile::generic_site(), IoConfig::default())?;
    let device = fleet.device_id(1).expect("valid");

    let mut program = Program::new("inbound-event");
    program.function("main", 0, |f| {
        let msg = f.receive();
        f.return_(msg);
    });
    let runtime = Arc::new(Runtime::new(program.build())?);
    let handle = runtime.spawn(0, &[])?;
    fleet.bind(device, handle.id())?;

    let events = EventRouter::new(Arc::clone(&runtime));
    fleet.deliver_event(&events, device, event_tag::ACTION_COMPLETED, Value::Int(99))?;

    let outcome = handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("deliver_event timed out")?;
    drop(events);
    drop(runtime);
    io.shutdown()?;

    match outcome {
        FlowOutcome::Completed(Value::Message(msg))
            if msg.tag == event_tag::ACTION_COMPLETED
                && matches!(msg.payload.as_ref(), Value::Int(99)) =>
        {
            Ok(())
        }
        other => Err(format!("unexpected outcome {other:?}").into()),
    }
}

#[test]
fn composite_shutdown_rejects_fast_submit() -> Result<(), Box<dyn std::error::Error>> {
    let (io, _fleet, _receiver, _fleet_driver) =
        bootstrap_composite(FleetProfile::generic_site(), IoConfig::default(), 4)?;
    io.shutdown()?;
    let err = io
        .driver()
        .try_submit(IoCommand::GpioWrite { pin: 1, high: true })
        .expect_err("closed composite driver");
    assert_eq!(err.kind(), byteflow_io::IoErrorKind::QueueClosed);
    Ok(())
}

#[test]
fn bind_rejects_second_flow_without_unbind() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, _) = bootstrap(FleetProfile::generic_site(), IoConfig::default())?;
    let mut program = Program::new("bind-twice");
    program.function("noop", 0, |f| {
        let n = f.load_i32(1);
        f.return_(n);
    });
    let natives = build_native_table(io.driver().clone(), Arc::new(NullSink))?;
    let runtime = Arc::new(Runtime::with_natives(program.build(), natives)?);
    let h1 = runtime.spawn(0, &[])?;
    let h2 = runtime.spawn(0, &[])?;
    let device = fleet.device_id(1).expect("valid");
    fleet.bind(device, h1.id())?;
    let err = fleet.bind(device, h2.id()).expect_err("second bind");
    assert!(matches!(err, FleetError::AlreadyBound));
    io.shutdown()?;
    Ok(())
}
