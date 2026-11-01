//! Native table composition: Byteflow slots `0..=7` plus I/O slots `8..=10`.

use std::sync::Arc;

use byteflow::{
    expect_arg, expect_bool, expect_int, std_native, std_native_table_with, Fault, NativeTable,
    NativeTableBuilder, OutputSink, Value,
};

use crate::{IoCommand, IoDriver, NativeTableBuildError};

/// Stable `CallNative` indices.
///
/// Standard slots are re-exported from [`byteflow::std_native`] so they cannot
/// drift from the core ABI. Existing values must never be renumbered; append
/// only.
pub mod native_index {
    pub use byteflow::std_native::{
        MAKE_MSG, MSG_PAYLOAD, MSG_REPLY_CAP, MSG_REQUEST_ID, MSG_SENDER, MSG_TAG, NOW_MS, PRINT,
    };

    /// First slot reserved for this integration crate.
    pub const FIRST_BYTEFLOW_IO: u32 = 8;

    /// `io_gpio_write` — enqueue a GPIO write.
    pub const GPIO_WRITE: u32 = 8;
    /// `io_mqtt_publish_fast` — enqueue an MQTT publish without waiting for ACK.
    pub const MQTT_PUBLISH_FAST: u32 = 9;
    /// `io_uart_write_fast` — enqueue a UART write without waiting for TX done.
    pub const UART_WRITE_FAST: u32 = 10;
}

/// Compose Byteflow’s eight standard natives with the fast I/O natives.
///
/// The standard [`NativeTable`] is immutable and [`NativeTableBuilder`] starts
/// empty, so this function copies the callable standard entries into a fresh
/// builder before registering slots `8..=10`. No standard slot is renumbered
/// or left unregistered.
///
/// # Errors
///
/// Returns [`NativeTableBuildError`] when a standard native is missing or a
/// registration is rejected by the builder.
pub fn build_native_table(
    driver: Arc<dyn IoDriver>,
    output: Arc<dyn OutputSink>,
) -> Result<Arc<NativeTable>, NativeTableBuildError> {
    let standard = std_native_table_with(output);
    let mut builder = NativeTableBuilder::new();

    let standard_entries: [(u32, &'static str); 8] = [
        (std_native::PRINT, "print"),
        (std_native::NOW_MS, "now_ms"),
        (std_native::MAKE_MSG, "make_msg"),
        (std_native::MSG_SENDER, "msg_sender"),
        (std_native::MSG_REQUEST_ID, "msg_request_id"),
        (std_native::MSG_TAG, "msg_tag"),
        (std_native::MSG_PAYLOAD, "msg_payload"),
        (std_native::MSG_REPLY_CAP, "msg_reply_cap"),
    ];

    for (index, name) in standard_entries {
        let native = standard
            .get(index)
            .cloned()
            .ok_or(NativeTableBuildError::MissingStandardNative { index, name })?;
        builder = builder.register_at(index, name, move |args| native(args))?;
    }

    let gpio_driver = Arc::clone(&driver);
    builder = builder.register_at(native_index::GPIO_WRITE, "io_gpio_write", move |args| {
        let pin = expect_u32(args, 0, "io_gpio_write")?;
        let high = expect_bool(args, 1, "io_gpio_write")?;
        let command_id = gpio_driver
            .try_submit(IoCommand::GpioWrite { pin, high })
            .map_err(|error| native_fault("io_gpio_write", error.to_string()))?;
        command_id_value(command_id, "io_gpio_write")
    })?;

    let mqtt_driver = Arc::clone(&driver);
    builder = builder.register_at(
        native_index::MQTT_PUBLISH_FAST,
        "io_mqtt_publish_fast",
        move |args| {
            let topic = expect_str(args, 0, "io_mqtt_publish_fast")?;
            let payload = expect_arg(args, 1, "io_mqtt_publish_fast")?.clone();
            let qos = expect_u8(args, 2, "io_mqtt_publish_fast")?;
            if qos > 2 {
                return Err(native_fault(
                    "io_mqtt_publish_fast",
                    "qos must be 0, 1, or 2",
                ));
            }

            let command_id = mqtt_driver
                .try_submit(IoCommand::MqttPublish {
                    topic,
                    payload,
                    qos,
                })
                .map_err(|error| native_fault("io_mqtt_publish_fast", error.to_string()))?;
            command_id_value(command_id, "io_mqtt_publish_fast")
        },
    )?;

    let uart_driver = Arc::clone(&driver);
    builder = builder.register_at(
        native_index::UART_WRITE_FAST,
        "io_uart_write_fast",
        move |args| {
            let port = expect_str(args, 0, "io_uart_write_fast")?;
            let bytes = expect_bytes(args, 1, "io_uart_write_fast")?;
            let command_id = uart_driver
                .try_submit(IoCommand::UartWrite { port, bytes })
                .map_err(|error| native_fault("io_uart_write_fast", error.to_string()))?;
            command_id_value(command_id, "io_uart_write_fast")
        },
    )?;

    Ok(builder.build())
}

fn expect_u32(args: &[Value], index: usize, name: &str) -> Result<u32, Fault> {
    let integer = expect_int(args, index, name)?;
    u32::try_from(integer)
        .map_err(|_| native_fault(name, format!("argument {index} is outside the u32 range")))
}

fn expect_u8(args: &[Value], index: usize, name: &str) -> Result<u8, Fault> {
    let integer = expect_int(args, index, name)?;
    u8::try_from(integer)
        .map_err(|_| native_fault(name, format!("argument {index} is outside the u8 range")))
}

fn expect_str(args: &[Value], index: usize, name: &str) -> Result<String, Fault> {
    expect_arg(args, index, name)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| native_fault(name, format!("argument {index} must be Str")))
}

fn expect_bytes(args: &[Value], index: usize, name: &str) -> Result<Vec<u8>, Fault> {
    expect_arg(args, index, name)?
        .as_bytes()
        .map(|bytes| bytes.to_vec())
        .ok_or_else(|| native_fault(name, format!("argument {index} must be Bytes")))
}

fn command_id_value(command_id: u64, name: &str) -> Result<Value, Fault> {
    let value = i64::try_from(command_id)
        .map_err(|_| native_fault(name, "driver command ID exceeds Byteflow Int range"))?;
    Ok(Value::Int(value))
}

fn native_fault(name: &str, message: impl Into<String>) -> Fault {
    Fault::NativeError(format!("{name}: {}", message.into()))
}
