//! Product-agnostic device commands and stable op / tag ids.

use byteflow::Value;

use super::device::DeviceId;
use crate::IoDriverError;

/// HostAwait op ids (stable across profiles).
pub mod host_op {
    pub use crate::samples::host_op::*;
}

/// Inbound Atomic Hop tags for device flows.
pub mod event_tag {
    pub use crate::samples::event_tag::*;

    pub const DEVICE_ONLINE: u16 = 14;
    pub const DEVICE_OFFLINE: u16 = 15;
}

/// Fast-path [`crate::IoCommand::Custom`] discriminators.
pub mod custom_op {
    pub const ABORT_ACTION: u32 = 100;
    /// Alias of [`ABORT_ACTION`] (historical laundry naming).
    pub const ABORT_CYCLE: u32 = ABORT_ACTION;
    pub const QUERY_STATUS: u32 = 101;
}

/// Command toward an edge device (ESP32, MCU, relay board, …).
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceCommand {
    StartAction {
        device: DeviceId,
    },
    AbortAction {
        device: DeviceId,
    },
    QueryStatus {
        device: DeviceId,
    },
    OtaChunk {
        device: DeviceId,
        seq: u32,
        bytes: Vec<u8>,
    },
    MqttPublishWaitAck {
        device: DeviceId,
        topic: String,
        payload: Value,
        qos: u8,
    },
    /// Arm / credit / unlock with a positive integer unit (cents, tokens, …).
    ApplyCredit {
        device: DeviceId,
        units: i64,
    },
}

fn expect_list<'a>(args: &'a Value, ctx: &str) -> Result<&'a [Value], IoDriverError> {
    match args {
        Value::List(items) => Ok(items.as_ref()),
        _ => Err(IoDriverError::invalid_args(format!("{ctx} expects a List"))),
    }
}

fn list_value(values: Vec<Value>) -> Result<Value, IoDriverError> {
    Value::try_list(values).ok_or_else(|| {
        IoDriverError::invalid_args("device HostAwait args exceed Byteflow list limits")
    })
}

impl DeviceCommand {
    pub fn start_args(device: impl Into<DeviceId>) -> Value {
        let device = device.into();
        Value::Int(i64::from(device.get()))
    }

    /// Alias of [`Self::start_args`] (historical laundry naming).
    pub fn start_cycle_args(device: impl Into<DeviceId>) -> Value {
        Self::start_args(device)
    }

    pub fn ota_chunk_args(
        device: impl Into<DeviceId>,
        seq: u32,
        bytes: Vec<u8>,
    ) -> Result<Value, IoDriverError> {
        let device = device.into();
        list_value(vec![
            Value::Int(i64::from(device.get())),
            Value::Int(i64::from(seq)),
            Value::bytes(bytes),
        ])
    }

    pub fn credit_args(device: impl Into<DeviceId>, units: i64) -> Result<Value, IoDriverError> {
        let device = device.into();
        list_value(vec![Value::Int(i64::from(device.get())), Value::Int(units)])
    }

    pub fn mqtt_wait_args(
        device: impl Into<DeviceId>,
        topic: impl Into<String>,
        payload: Value,
        qos: u8,
    ) -> Result<Value, IoDriverError> {
        let device = device.into();
        list_value(vec![
            Value::Int(i64::from(device.get())),
            Value::str(topic.into()),
            payload,
            Value::Int(i64::from(qos)),
        ])
    }

    pub fn parse_start(args: &Value, max_devices: u8) -> Result<DeviceId, IoDriverError> {
        DeviceId::from_value(args, max_devices).ok_or_else(|| {
            IoDriverError::invalid_args(format!(
                "START_ACTION_AND_WAIT expects Int device id 1..={max_devices}"
            ))
        })
    }

    pub fn parse_ota_chunk(
        args: &Value,
        max_devices: u8,
    ) -> Result<(DeviceId, u32, Vec<u8>), IoDriverError> {
        let list = expect_list(args, "DEVICE_OTA_CHUNK")?;
        if list.len() < 3 {
            return Err(IoDriverError::invalid_args(
                "DEVICE_OTA_CHUNK expects List [device, seq, bytes]",
            ));
        }
        let device = DeviceId::from_value(&list[0], max_devices)
            .ok_or_else(|| IoDriverError::invalid_args("DEVICE_OTA_CHUNK device id invalid"))?;
        let seq = list[1]
            .as_int()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                IoDriverError::invalid_args("DEVICE_OTA_CHUNK seq must be a non-negative Int")
            })?;
        let bytes = list[2]
            .as_bytes()
            .map(|b| b.to_vec())
            .ok_or_else(|| IoDriverError::invalid_args("DEVICE_OTA_CHUNK payload must be Bytes"))?;
        Ok((device, seq, bytes))
    }

    pub fn parse_credit(args: &Value, max_devices: u8) -> Result<(DeviceId, i64), IoDriverError> {
        let list = expect_list(args, "APPLY_CREDIT")?;
        if list.len() < 2 {
            return Err(IoDriverError::invalid_args(
                "APPLY_CREDIT expects List [device, units]",
            ));
        }
        let device = DeviceId::from_value(&list[0], max_devices)
            .ok_or_else(|| IoDriverError::invalid_args("APPLY_CREDIT device id invalid"))?;
        let units = list[1]
            .as_int()
            .ok_or_else(|| IoDriverError::invalid_args("APPLY_CREDIT units must be Int"))?;
        Ok((device, units))
    }

    pub fn parse_mqtt_wait(
        args: &Value,
        max_devices: u8,
    ) -> Result<(DeviceId, String, Value, u8), IoDriverError> {
        let list = expect_list(args, "MQTT_PUBLISH_AND_WAIT_ACK")?;
        if list.len() < 4 {
            return Err(IoDriverError::invalid_args(
                "MQTT_PUBLISH_AND_WAIT_ACK expects List [device, topic, payload, qos]",
            ));
        }
        let device = DeviceId::from_value(&list[0], max_devices).ok_or_else(|| {
            IoDriverError::invalid_args("MQTT_PUBLISH_AND_WAIT_ACK device id invalid")
        })?;
        let topic = list[1].as_str().map(str::to_owned).ok_or_else(|| {
            IoDriverError::invalid_args("MQTT_PUBLISH_AND_WAIT_ACK topic must be Str")
        })?;
        let payload = list[2].clone();
        let qos = list[3]
            .as_int()
            .and_then(|n| u8::try_from(n).ok())
            .ok_or_else(|| {
                IoDriverError::invalid_args("MQTT_PUBLISH_AND_WAIT_ACK qos must be 0..=2")
            })?;
        if qos > 2 {
            return Err(IoDriverError::invalid_args(
                "MQTT_PUBLISH_AND_WAIT_ACK qos must be 0, 1, or 2",
            ));
        }
        Ok((device, topic, payload, qos))
    }
}
