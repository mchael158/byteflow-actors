//! Pluggable edge transport. Default: in-memory simulator (no MQTT SDK).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byteflow::Value;

use super::device::{DeviceId, DeviceState, FleetError, SharedFleet};
use super::protocol::DeviceCommand;
use crate::{CancellationToken, IoDriverError};

/// Host bridge to cheap edge devices (ESP32, MCU, UART bridge, …).
///
/// Production: MQTT or UART. Tests: [`InMemoryDeviceTransport`].
///
/// Custom transports must implement [`DeviceTransport::as_any`] (typically
/// `fn as_any(&self) -> &dyn std::any::Any { self }`).
pub trait DeviceTransport: Send + Sync + fmt::Debug + 'static {
    fn publish_and_wait(
        &self,
        command: &DeviceCommand,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Value, IoDriverError>;

    fn enqueue(&self, command: &DeviceCommand) -> Result<(), IoDriverError>;

    /// Downcast for host diagnostics / tests.
    fn as_any(&self) -> &dyn std::any::Any;
}

fn map_fleet_err(err: FleetError) -> IoDriverError {
    match err {
        FleetError::Poisoned => IoDriverError::poisoned(err.to_string()),
        FleetError::InvalidDevice | FleetError::Unbound | FleetError::AlreadyBound => {
            IoDriverError::invalid_args(err.to_string())
        }
        FleetError::Send(_) | FleetError::Profile(_) => IoDriverError::other(err.to_string()),
    }
}

/// Simulated fleet: instant ACK, optional delay, state updates.
#[derive(Debug)]
pub struct InMemoryDeviceTransport {
    fleet: SharedFleet,
    delay: Duration,
    ota_seq: Mutex<HashMap<DeviceId, u32>>,
    published: Mutex<Vec<DeviceCommand>>,
}

impl InMemoryDeviceTransport {
    pub fn new(fleet: SharedFleet) -> Arc<Self> {
        Arc::new(Self {
            fleet,
            delay: Duration::ZERO,
            ota_seq: Mutex::new(HashMap::new()),
            published: Mutex::new(Vec::new()),
        })
    }

    pub fn with_delay(fleet: SharedFleet, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            fleet,
            delay,
            ota_seq: Mutex::new(HashMap::new()),
            published: Mutex::new(Vec::new()),
        })
    }

    pub fn published(&self) -> Result<Vec<DeviceCommand>, IoDriverError> {
        self.published
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| IoDriverError::poisoned("device transport mutex poisoned"))
    }

    fn sleep_cooperative(
        &self,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), IoDriverError> {
        let end = Instant::now() + self.delay;
        while Instant::now() < end {
            if cancel.is_cancelled() {
                return Err(IoDriverError::cancelled("device operation cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(IoDriverError::deadline(
                    "device operation deadline exceeded",
                ));
            }
            std::thread::sleep(
                Duration::from_millis(5).min(end.saturating_duration_since(Instant::now())),
            );
        }
        if cancel.is_cancelled() {
            return Err(IoDriverError::cancelled("device operation cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(IoDriverError::deadline(
                "device operation deadline exceeded",
            ));
        }
        Ok(())
    }

    fn record(&self, command: &DeviceCommand) -> Result<(), IoDriverError> {
        self.published
            .lock()
            .map_err(|_| IoDriverError::poisoned("device transport mutex poisoned"))?
            .push(command.clone());
        Ok(())
    }
}

impl DeviceTransport for InMemoryDeviceTransport {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn enqueue(&self, command: &DeviceCommand) -> Result<(), IoDriverError> {
        self.record(command)?;
        if let DeviceCommand::AbortAction { device } = command {
            let _ = self.fleet.set_state(*device, DeviceState::Idle);
        }
        Ok(())
    }

    fn publish_and_wait(
        &self,
        command: &DeviceCommand,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Value, IoDriverError> {
        self.sleep_cooperative(cancel, deadline)?;
        self.record(command)?;

        match command {
            DeviceCommand::StartAction { device } => {
                let slot = self.fleet.lookup(*device).map_err(map_fleet_err)?;
                if slot.state != DeviceState::Armed && slot.state != DeviceState::Idle {
                    return Err(IoDriverError::invalid_args(format!(
                        "{} cannot start from state {:?}",
                        self.fleet.display_name(*device),
                        slot.state
                    )));
                }
                self.fleet
                    .set_state(*device, DeviceState::Busy)
                    .map_err(map_fleet_err)?;
                Ok(Value::Int(i64::from(device.get())))
            }
            DeviceCommand::AbortAction { device } => {
                self.fleet
                    .set_state(*device, DeviceState::Idle)
                    .map_err(map_fleet_err)?;
                Ok(Value::Int(i64::from(device.get())))
            }
            DeviceCommand::QueryStatus { device } => {
                let slot = self.fleet.lookup(*device).map_err(map_fleet_err)?;
                Ok(Value::Int(i64::from(slot.state)))
            }
            DeviceCommand::OtaChunk { device, seq, bytes } => {
                if bytes.is_empty() {
                    return Err(IoDriverError::invalid_args("OTA chunk must not be empty"));
                }
                let mut seqs = self
                    .ota_seq
                    .lock()
                    .map_err(|_| IoDriverError::poisoned("device transport mutex poisoned"))?;
                let expected = seqs.get(device).copied().unwrap_or(0);
                if *seq != expected {
                    return Err(IoDriverError::invalid_args(format!(
                        "OTA seq mismatch for {}: expected {expected}, got {seq}",
                        self.fleet.display_name(*device)
                    )));
                }
                seqs.insert(*device, expected.saturating_add(1));
                Ok(Value::Int(i64::from(*seq)))
            }
            DeviceCommand::MqttPublishWaitAck {
                device,
                topic: _,
                payload: _,
                qos: _,
            } => Ok(Value::Int(i64::from(device.get()))),
            DeviceCommand::ApplyCredit { device, units } => {
                if *units <= 0 {
                    return Err(IoDriverError::invalid_args("credit units must be positive"));
                }
                self.fleet
                    .set_state(*device, DeviceState::Armed)
                    .map_err(map_fleet_err)?;
                Ok(Value::Int(*units))
            }
        }
    }
}
