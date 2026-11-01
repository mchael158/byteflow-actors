//! Fleet IoDriver: natives / HostAwait → [`DeviceTransport`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use byteflow::{HostAwaitOp, Value};

use super::protocol::{custom_op, host_op, DeviceCommand};
use super::topics;
use super::transport::{DeviceTransport, InMemoryDeviceTransport};
use super::SharedFleet;
use crate::{CancellationToken, IoCommand, IoDriver, IoDriverError};

pub const DEFAULT_DEVICE_DEADLINE: Duration = Duration::from_secs(15);

/// Generic fleet driver — works for any [`super::FleetProfile`].
pub struct FleetDriver {
    fleet: SharedFleet,
    transport: Arc<dyn DeviceTransport>,
    next_id: AtomicU64,
    closed: AtomicBool,
    deadline: Duration,
}

impl std::fmt::Debug for FleetDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetDriver")
            .field("profile", &self.fleet.profile().to_string())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl FleetDriver {
    pub fn new(fleet: SharedFleet, transport: Arc<dyn DeviceTransport>) -> Arc<Self> {
        Self::with_deadline(fleet, transport, DEFAULT_DEVICE_DEADLINE)
    }

    pub fn with_deadline(
        fleet: SharedFleet,
        transport: Arc<dyn DeviceTransport>,
        deadline: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            fleet,
            transport,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            deadline,
        })
    }

    pub fn fleet(&self) -> &SharedFleet {
        &self.fleet
    }

    pub fn transport(&self) -> &Arc<dyn DeviceTransport> {
        &self.transport
    }

    /// Snapshot of commands accepted by the in-memory simulator (tests / dry-run).
    pub fn in_memory_published(&self) -> Result<Vec<DeviceCommand>, IoDriverError> {
        self.transport
            .as_ref()
            .as_any()
            .downcast_ref::<InMemoryDeviceTransport>()
            .ok_or_else(|| {
                IoDriverError::unsupported("fleet driver transport is not InMemoryDeviceTransport")
            })?
            .published()
    }

    fn alloc_id(&self) -> Result<u64, IoDriverError> {
        self.next_id
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| IoDriverError::exhausted("fleet command ID space exhausted"))
    }

    fn ensure_open(&self) -> Result<(), IoDriverError> {
        if self.closed.load(Ordering::Acquire) {
            Err(IoDriverError::closed("fleet driver is shut down"))
        } else {
            Ok(())
        }
    }

    fn max(&self) -> u8 {
        self.fleet.max_devices()
    }

    fn map_fast_command(&self, command: &IoCommand) -> Result<DeviceCommand, IoDriverError> {
        match command {
            IoCommand::MqttPublish {
                topic,
                payload,
                qos,
            } => {
                let device = topics::parse_device(self.fleet.profile(), topic).ok_or_else(|| {
                    IoDriverError::invalid_args(format!(
                        "MQTT topic is not under {}: {topic}",
                        self.fleet.profile().topic_root
                    ))
                })?;
                if *qos > 2 {
                    return Err(IoDriverError::invalid_args(
                        "MQTT publish qos must be 0, 1, or 2",
                    ));
                }
                Ok(DeviceCommand::MqttPublishWaitAck {
                    device,
                    topic: topic.clone(),
                    payload: payload.clone(),
                    qos: *qos,
                })
            }
            IoCommand::Custom { op, args } => match *op {
                custom_op::ABORT_ACTION => {
                    let device = super::DeviceId::from_value(args, self.max()).ok_or_else(|| {
                        IoDriverError::invalid_args(format!(
                            "ABORT_ACTION expects Int device id 1..={}",
                            self.max()
                        ))
                    })?;
                    Ok(DeviceCommand::AbortAction { device })
                }
                custom_op::QUERY_STATUS => {
                    let device = super::DeviceId::from_value(args, self.max()).ok_or_else(|| {
                        IoDriverError::invalid_args(format!(
                            "QUERY_STATUS expects Int device id 1..={}",
                            self.max()
                        ))
                    })?;
                    Ok(DeviceCommand::QueryStatus { device })
                }
                other => Err(IoDriverError::unsupported(format!(
                    "unsupported fleet custom op {other}"
                ))),
            },
            IoCommand::GpioWrite { .. } | IoCommand::UartWrite { .. } => {
                Err(IoDriverError::unsupported(
                    "fleet driver does not handle GPIO/UART natives; use fleet::bootstrap_composite",
                ))
            }
        }
    }
}

impl IoDriver for FleetDriver {
    fn try_submit(&self, command: IoCommand) -> Result<u64, IoDriverError> {
        self.ensure_open()?;
        let cmd = self.map_fast_command(&command)?;
        self.transport.enqueue(&cmd)?;
        let id = self.alloc_id()?;
        Ok(id)
    }

    fn execute_host_await(
        &self,
        op: HostAwaitOp,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, IoDriverError> {
        self.ensure_open()?;
        let deadline = Instant::now() + self.deadline;
        let max = self.max();

        let command = match op {
            host_op::START_ACTION_AND_WAIT => {
                let device = DeviceCommand::parse_start(&args, max)?;
                DeviceCommand::StartAction { device }
            }
            host_op::DEVICE_OTA_CHUNK => {
                let (device, seq, bytes) = DeviceCommand::parse_ota_chunk(&args, max)?;
                DeviceCommand::OtaChunk { device, seq, bytes }
            }
            host_op::APPLY_CREDIT => {
                let (device, units) = DeviceCommand::parse_credit(&args, max)?;
                DeviceCommand::ApplyCredit { device, units }
            }
            host_op::MQTT_PUBLISH_AND_WAIT_ACK => {
                let (device, topic, payload, qos) = DeviceCommand::parse_mqtt_wait(&args, max)?;
                DeviceCommand::MqttPublishWaitAck {
                    device,
                    topic,
                    payload,
                    qos,
                }
            }
            other => {
                return Err(IoDriverError::unsupported(format!(
                    "fleet driver: unknown HostAwait op {other}"
                )));
            }
        };

        self.transport.publish_and_wait(&command, cancel, deadline)
    }

    fn shutdown(&self) -> Result<(), IoDriverError> {
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

impl crate::HostAwaitHandler for FleetDriver {
    fn execute(
        &self,
        op: HostAwaitOp,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, IoDriverError> {
        IoDriver::execute_host_await(self, op, args, cancel)
    }

    fn shutdown(&self) -> Result<(), IoDriverError> {
        IoDriver::shutdown(self)
    }
}
