//! Host-side “ESP32 firmware” loop for dry-runs.
//!
//! [`InMemoryDeviceTransport`] already behaves like an ESP32 ACK/state machine on
//! the PC. This module adds the **device thread** that drains the composite
//! queue — the same role UART/MQTT firmware plays on real silicon.
//!
//! # Real ESP32 / Wokwi / QEMU
//!
//! - **Sem broker, sem chip**: use [`super::bootstrap_composite`] +
//!   [`spawn_firmware_thread`] + [`InMemoryDeviceTransport`] (zero cost).
//! - **Com firmware no simulador** (Wokwi, QEMU): Byteflow continua no PC; o
//!   sketch ESP publica/assina MQTT no broker; você implementa
//!   [`DeviceTransport`] apontando para esse broker (fora deste crate).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use byteflow::Value;

use crate::{EventRouter, IoCommand, IoCommandReceiver, IoDriverError};

use super::{
    dispatch_queued_command, event_tag, DeviceId, FleetDriver, FleetError, FleetRegistry,
    QueuedDispatch,
};

/// Result of one poll of the simulated ESP32 command queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SimDrainOutcome {
    /// No command waiting (non-blocking try or recv timeout).
    Idle,
    /// Host GPIO line (relay, motor enable, etc.) — actuate on the board/sim.
    HostGpio { pin: u32, high: bool },
    /// Host UART bytes — send on serial in production.
    HostUart { port: String, byte_len: usize },
    /// MQTT / fleet fast command forwarded into [`InMemoryDeviceTransport`].
    FleetCommandAccepted {
        queue_command_id: u64,
        fleet_command_id: u64,
    },
}

fn map_queued(
    fleet_driver: &FleetDriver,
    queued: crate::QueuedCommand,
) -> Result<SimDrainOutcome, IoDriverError> {
    match dispatch_queued_command(fleet_driver, queued)? {
        QueuedDispatch::HostDevice(IoCommand::GpioWrite { pin, high }) => {
            Ok(SimDrainOutcome::HostGpio { pin, high })
        }
        QueuedDispatch::HostDevice(IoCommand::UartWrite { port, bytes }) => {
            Ok(SimDrainOutcome::HostUart {
                port,
                byte_len: bytes.len(),
            })
        }
        QueuedDispatch::FleetFast {
            queue_command_id,
            fleet_command_id,
        } => Ok(SimDrainOutcome::FleetCommandAccepted {
            queue_command_id,
            fleet_command_id,
        }),
        QueuedDispatch::HostDevice(other) => Err(IoDriverError::unsupported(format!(
            "ESP32 sim: dispatch returned host device for non-GPIO/UART command: {other:?}"
        ))),
    }
}

/// Non-blocking drain (one command if present).
pub fn drain_once(
    receiver: &IoCommandReceiver,
    fleet_driver: &FleetDriver,
) -> Result<SimDrainOutcome, IoDriverError> {
    match receiver.try_recv() {
        Ok(queued) => map_queued(fleet_driver, queued),
        Err(std::sync::mpsc::TryRecvError::Empty) => Ok(SimDrainOutcome::Idle),
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(IoDriverError::closed(
            "ESP32 simulator: host command queue disconnected",
        )),
    }
}

/// Blocking drain with timeout (typical firmware loop tick).
pub fn poll(
    receiver: &IoCommandReceiver,
    fleet_driver: &FleetDriver,
    timeout: Duration,
) -> Result<SimDrainOutcome, IoDriverError> {
    match receiver.recv_timeout(timeout) {
        Ok(queued) => map_queued(fleet_driver, queued),
        Err(RecvTimeoutError::Timeout) => Ok(SimDrainOutcome::Idle),
        Err(RecvTimeoutError::Disconnected) => Err(IoDriverError::closed(
            "ESP32 simulator: host command queue disconnected",
        )),
    }
}

/// Background thread mimicking ESP32 main loop (`recv_timeout` + dispatch).
///
/// Set `stop` to `true` to exit; join the returned handle before shutdown.
pub fn spawn_firmware_thread(
    receiver: IoCommandReceiver,
    fleet_driver: Arc<FleetDriver>,
    stop: Arc<AtomicBool>,
    poll_interval: Duration,
    mut on_outcome: impl FnMut(SimDrainOutcome) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match poll(&receiver, &fleet_driver, poll_interval) {
                Ok(SimDrainOutcome::Idle) => {}
                Ok(outcome) => on_outcome(outcome),
                Err(_) => break,
            }
        }
    })
}

/// Inject `DEVICE_ONLINE` as if the ESP32 just joined the broker.
pub fn announce_online(
    fleet: &FleetRegistry,
    events: &EventRouter,
    device: DeviceId,
) -> Result<(), FleetError> {
    fleet.deliver_event(events, device, event_tag::DEVICE_ONLINE, Value::Unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{bootstrap_composite, FleetProfile};
    use crate::IoConfig;

    #[test]
    fn sim_drain_accepts_fleet_mqtt_from_queue() -> Result<(), Box<dyn std::error::Error>> {
        let (io, _fleet, receiver, fleet_driver) =
            bootstrap_composite(FleetProfile::generic_site(), IoConfig::default(), 8)?;
        io.driver().try_submit(IoCommand::Custom {
            op: super::super::custom_op::QUERY_STATUS,
            args: Value::Int(1),
        })?;
        match drain_once(&receiver, &fleet_driver)? {
            SimDrainOutcome::FleetCommandAccepted { .. } => {}
            other => return Err(format!("expected fleet accept, got {other:?}").into()),
        }
        io.shutdown()?;
        Ok(())
    }
}
