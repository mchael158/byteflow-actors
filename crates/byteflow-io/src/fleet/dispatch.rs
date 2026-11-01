//! Route commands dequeued from [`crate::IoCommandReceiver`] in composite hosts.

use crate::{IoCommand, IoDriver, IoDriverError, QueuedCommand};

use super::FleetDriver;

/// What the host device thread should do with one dequeued command.
#[derive(Clone, Debug, PartialEq)]
pub enum QueuedDispatch {
    /// GPIO/UART (or other board I/O): execute on the host device thread.
    HostDevice(IoCommand),
    /// MQTT / fleet custom fast path: accepted by [`FleetDriver`] → edge transport.
    FleetFast {
        /// Correlation id from the fleet driver (not necessarily equal to queue id).
        fleet_command_id: u64,
        /// Original queue id returned to bytecode from [`IoDriver::try_submit`].
        queue_command_id: u64,
    },
}

/// Dispatch one command from the composite fast queue.
///
/// Call this from the host-owned thread that drains [`crate::IoCommandReceiver`].
/// Without this step, MQTT and fleet `Custom` commands never reach the edge
/// transport when using [`super::bootstrap_composite`].
pub fn dispatch_queued_command(
    fleet_driver: &FleetDriver,
    queued: QueuedCommand,
) -> Result<QueuedDispatch, IoDriverError> {
    let queue_command_id = queued.id;
    match queued.command {
        cmd @ (IoCommand::GpioWrite { .. } | IoCommand::UartWrite { .. }) => {
            Ok(QueuedDispatch::HostDevice(cmd))
        }
        cmd @ (IoCommand::MqttPublish { .. } | IoCommand::Custom { .. }) => {
            let fleet_command_id = fleet_driver.try_submit(cmd)?;
            Ok(QueuedDispatch::FleetFast {
                fleet_command_id,
                queue_command_id,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{cmd, FleetProfile, FleetRegistry, InMemoryDeviceTransport};
    use std::sync::Arc;

    #[test]
    fn mqtt_dispatches_to_fleet_transport() -> Result<(), Box<dyn std::error::Error>> {
        let profile = FleetProfile::generic_site();
        let fleet = Arc::new(FleetRegistry::new(profile.clone())?);
        let transport = InMemoryDeviceTransport::new(Arc::clone(&fleet));
        let driver = FleetDriver::new(Arc::clone(&fleet), transport);
        let device = fleet.device_id(1).expect("valid");
        let topic = cmd(&profile, device);

        let queued = QueuedCommand {
            id: 7,
            command: IoCommand::MqttPublish {
                topic,
                payload: byteflow::Value::Int(1),
                qos: 0,
            },
        };
        match dispatch_queued_command(&driver, queued)? {
            QueuedDispatch::FleetFast {
                fleet_command_id,
                queue_command_id,
            } => {
                assert_eq!(queue_command_id, 7);
                assert_eq!(fleet_command_id, 1);
            }
            other => return Err(format!("expected FleetFast, got {other:?}").into()),
        }
        assert_eq!(driver.in_memory_published()?.len(), 1);
        Ok(())
    }

    #[test]
    fn mqtt_fast_path_preserves_qos() -> Result<(), Box<dyn std::error::Error>> {
        let profile = FleetProfile::generic_site();
        let fleet = Arc::new(FleetRegistry::new(profile.clone())?);
        let transport = InMemoryDeviceTransport::new(Arc::clone(&fleet));
        let driver = FleetDriver::new(Arc::clone(&fleet), transport);
        let device = fleet.device_id(1).expect("valid");
        let topic = cmd(&profile, device);

        let queued = QueuedCommand {
            id: 1,
            command: IoCommand::MqttPublish {
                topic,
                payload: byteflow::Value::Int(1),
                qos: 2,
            },
        };
        dispatch_queued_command(&driver, queued)?;
        let published = driver.in_memory_published()?;
        assert_eq!(published.len(), 1);
        assert!(
            matches!(
                &published[0],
                super::super::protocol::DeviceCommand::MqttPublishWaitAck { qos: 2, .. }
            ),
            "expected qos=2, got {:?}",
            published[0]
        );
        Ok(())
    }
}
