//! Composite host: drain the fast queue with [`dispatch_queued_command`], then
//! drive fleet work through bytecode `HostAwait` (bridge workers — not direct
//! [`IoDriver::execute_host_await`] on the main thread).

use std::sync::Arc;
use std::time::Duration;

use byteflow::{FlowOutcome, NullSink, Program, Runtime, Value};
use byteflow_io::fleet::{
    bootstrap_composite, dispatch_queued_command, event_tag, host_op, DeviceState, FleetProfile,
    QueuedDispatch, DEFAULT_FAST_QUEUE_CAPACITY,
};
use byteflow_io::{build_native_table, EventRouter, IoCommand, IoConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (io, fleet, receiver, fleet_driver) = bootstrap_composite(
        FleetProfile::generic_site(),
        IoConfig::default(),
        DEFAULT_FAST_QUEUE_CAPACITY,
    )?;

    let device = fleet.device_id(1).expect("device 1");

    // Fast path from host (same as native GPIO would enqueue).
    let queue_id = io.driver().try_submit(IoCommand::GpioWrite {
        pin: 12,
        high: true,
    })?;
    let queued = receiver
        .try_recv()
        .map_err(|e| format!("expected GPIO on device queue: {e}"))?;
    match dispatch_queued_command(&fleet_driver, queued)? {
        QueuedDispatch::HostDevice(IoCommand::GpioWrite { pin, high }) => {
            assert_eq!(pin, 12);
            assert!(high);
            println!("host device thread: GPIO pin {pin} = {high}");
        }
        other => return Err(format!("expected HostDevice, got {other:?}").into()),
    }
    let _ = queue_id;

    let mut program = Program::new("fleet-queued-host");
    let action = program.function("action", 0, |f| {
        let device_arg = f.load_i32(1);
        let _ticket = f.host_await(host_op::START_ACTION_AND_WAIT, device_arg);
        let reply = f.receive();
        let payload = f.hop_payload(reply);
        f.return_(payload);
    });
    let listener = program.function("listener", 0, |f| {
        let msg = f.receive();
        f.return_(msg);
    });

    let natives = build_native_table(io.driver().clone(), Arc::new(NullSink))?;
    let runtime = Arc::new(Runtime::with_natives_and_config(
        program.build(),
        natives,
        io.runtime_config(Arc::new(NullSink)),
    )?);

    fleet.set_state(device, DeviceState::Armed)?;
    let action_handle = runtime.spawn(action, &[])?;
    let listen_handle = runtime.spawn(listener, &[])?;
    fleet.bind(device, action_handle.id())?;
    fleet.bind(fleet.device_id(2).expect("device 2"), listen_handle.id())?;

    let events = EventRouter::new(Arc::clone(&runtime));
    fleet.deliver_event(
        &events,
        fleet.device_id(2).expect("device 2"),
        event_tag::INBOUND_CREDIT,
        Value::Int(250),
    )?;

    let action_out = action_handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("action host_await timed out")?;
    let listen_out = listen_handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("listener timed out")?;

    match (&action_out, &listen_out) {
        (FlowOutcome::Completed(Value::Int(1)), FlowOutcome::Completed(Value::Message(msg)))
            if msg.tag == event_tag::INBOUND_CREDIT
                && matches!(msg.payload.as_ref(), Value::Int(250)) =>
        {
            println!("composite: GPIO dispatched; START_ACTION via bridge; credit on device-02");
        }
        other => return Err(format!("unexpected outcomes: {other:?}").into()),
    }

    drop(events);
    match Arc::try_unwrap(runtime) {
        Ok(rt) => rt.shutdown(),
        Err(shared) => drop(shared),
    }
    io.shutdown()?;
    Ok(())
}
