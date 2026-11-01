//! Simulated ESP32 on the host: firmware thread + Byteflow runtime on the same PC.
//!
//! [`InMemoryDeviceTransport`] plays the chip ACK/state logic; [`spawn_firmware_thread`]
//! plays the sketch loop. For Wokwi/QEMU with real firmware, keep Byteflow on the PC
//! and plug a MQTT [`DeviceTransport`] into your broker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use byteflow::{FlowOutcome, NullSink, Program, Runtime, Value};
use byteflow_io::fleet::{
    bootstrap_composite, host_op, spawn_firmware_thread, DeviceState, FleetProfile,
    SimDrainOutcome, DEFAULT_FAST_QUEUE_CAPACITY,
};
use byteflow_io::{build_native_table, IoConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    let (io, fleet, receiver, fleet_driver) = bootstrap_composite(
        FleetProfile::generic_site(),
        IoConfig::default(),
        DEFAULT_FAST_QUEUE_CAPACITY,
    )?;

    let sim_stop = Arc::clone(&stop);
    let sim_handle = spawn_firmware_thread(
        receiver,
        Arc::clone(&fleet_driver),
        sim_stop,
        Duration::from_millis(50),
        |outcome| match outcome {
            SimDrainOutcome::HostGpio { pin, high } => {
                println!("[esp32-sim] GPIO pin {pin} <- {high}");
            }
            SimDrainOutcome::HostUart { port, byte_len } => {
                println!("[esp32-sim] UART {port} len={byte_len}");
            }
            SimDrainOutcome::FleetCommandAccepted {
                queue_command_id,
                fleet_command_id,
            } => println!(
                "[esp32-sim] MQTT/fleet cmd queue_id={queue_command_id} fleet_id={fleet_command_id}"
            ),
            SimDrainOutcome::Idle => {}
        },
    );

    let device = fleet.device_id(1).expect("device 1");

    let mut program = Program::new("esp32-sim-host");
    let action = program.function("action", 0, |f| {
        let id = f.load_i32(1);
        let _ticket = f.host_await(host_op::START_ACTION_AND_WAIT, id);
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
    let handle = runtime.spawn(action, &[])?;
    fleet.bind(device, handle.id())?;

    let outcome = handle
        .join_timeout(Duration::from_secs(3))
        .ok_or("action timed out")?;

    stop.store(true, Ordering::Release);
    sim_handle.join().ok();

    match outcome {
        FlowOutcome::Completed(Value::Message(msg))
            if matches!(msg.payload.as_ref(), Value::Int(1)) =>
        {
            println!(
                "esp32-sim OK: START_ACTION via bridge; state={:?}",
                fleet.lookup(device)?.state
            );
        }
        other => return Err(format!("unexpected outcome: {other:?}").into()),
    }

    match Arc::try_unwrap(runtime) {
        Ok(rt) => rt.shutdown(),
        Err(shared) => drop(shared),
    }
    io.shutdown()?;
    Ok(())
}
