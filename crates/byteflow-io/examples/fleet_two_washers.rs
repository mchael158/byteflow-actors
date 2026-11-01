//! Generic two-device fleet demo (laundry profile is optional naming only).
//!
//! Arms device-01, starts an action via HostAwait, and injects a coin event
//! into device-02 through [`FleetRegistry::deliver_event`].

use std::sync::Arc;
use std::time::Duration;

use byteflow::{FlowOutcome, NullSink, Program, Runtime, Value};
use byteflow_io::fleet::{bootstrap, event_tag, host_op, DeviceCommand, DeviceState, FleetProfile};
use byteflow_io::{build_native_table, EventRouter, IoConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Swap `generic_site()` for `laundry_washers()` (or any custom profile)
    // without changing the rest of the host wiring.
    let (io, fleet, _) = bootstrap(
        FleetProfile::generic_site(),
        IoConfig {
            worker_threads: 2,
            queue_capacity: 32,
            ..IoConfig::default()
        },
    )?;

    let device_a = fleet.device_id(1).expect("device 1");
    let device_b = fleet.device_id(2).expect("device 2");

    let mut program = Program::new("fleet-two-devices");
    let action = program.function("action", 0, |f| {
        let device = f.load_i32(1);
        let _ticket = f.host_await(host_op::START_ACTION_AND_WAIT, device);
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

    fleet.set_state(device_a, DeviceState::Armed)?;
    let action_handle = runtime.spawn(action, &[])?;
    fleet.bind(device_a, action_handle.id())?;

    let listen_handle = runtime.spawn(listener, &[])?;
    fleet.bind(device_b, listen_handle.id())?;

    let events = EventRouter::new(Arc::clone(&runtime));
    fleet.deliver_event(&events, device_b, event_tag::COIN_INSERTED, Value::Int(500))?;

    let action_out = action_handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("action timed out")?;
    let listen_out = listen_handle
        .join_timeout(Duration::from_secs(2))
        .ok_or("listener timed out")?;

    let _ = DeviceCommand::credit_args(device_a, 500)?;

    match (&action_out, &listen_out) {
        (FlowOutcome::Completed(Value::Int(1)), FlowOutcome::Completed(Value::Message(msg)))
            if msg.tag == event_tag::COIN_INSERTED
                && matches!(msg.payload.as_ref(), Value::Int(500)) =>
        {
            println!(
                "{} action started; {} received coin=500",
                fleet.display_name(device_a),
                fleet.display_name(device_b)
            );
            println!(
                "profile={} bound={} state_a={:?} state_b={:?}",
                fleet.profile(),
                fleet.bound_count()?,
                fleet.lookup(device_a)?.state,
                fleet.lookup(device_b)?.state
            );
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
