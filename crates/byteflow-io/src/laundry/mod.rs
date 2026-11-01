//! Optional laundry *profile* on top of [`crate::fleet`].
//!
//! Washers are one product surface — not the core of byteflow-io. Prefer
//! [`crate::fleet`] for new hosts; use this module when you want washer
//! naming (`MachineId`, `laundry/washer/…` topics) without rewriting callsites.

use std::sync::Arc;
use std::time::Duration;

use byteflow::Value;

use crate::fleet::{self, DeviceId, FleetProfile};
use crate::{ByteflowIo, IoConfig};

pub use crate::fleet::custom_op;
pub use crate::fleet::event_tag::{
    self, DEVICE_OFFLINE as WASHER_OFFLINE, DEVICE_ONLINE as WASHER_ONLINE,
};
pub use crate::fleet::host_op;
pub use crate::fleet::{
    abort_token, DeviceCommand as EspCommand, DeviceSlot as MachineSlot,
    DeviceState as MachineState, DeviceTransport as EspTransport, FleetDriver as LaundryEspDriver,
    FleetError, FleetRegistry, InMemoryDeviceTransport as InMemoryEspTransport, SharedFleet,
    DEFAULT_DEVICE_DEADLINE as DEFAULT_ESP_DEADLINE,
};

/// Historical laundry capacity (same default as [`FleetProfile::generic_site`]).
pub const MACHINE_COUNT: u8 = 56;

/// Washer identity helpers bound to [`MACHINE_COUNT`].
///
/// Prefer [`crate::fleet::DeviceId`] + [`FleetProfile`] for non-laundry fleets.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MachineId(DeviceId);

impl MachineId {
    /// Create a washer id in `1..=56`.
    pub fn new(raw: u8) -> Option<Self> {
        DeviceId::new(raw, MACHINE_COUNT).map(Self)
    }

    /// Parse from Byteflow `Int`.
    pub fn from_value(value: &Value) -> Option<Self> {
        DeviceId::from_value(value, MACHINE_COUNT).map(Self)
    }

    pub fn get(self) -> u8 {
        self.0.get()
    }

    pub fn index(self) -> usize {
        self.0.index()
    }

    /// Underlying generic device id.
    pub fn device(self) -> DeviceId {
        self.0
    }

    pub fn all() -> impl Iterator<Item = Self> {
        (1..=MACHINE_COUNT).filter_map(Self::new)
    }
}

impl From<MachineId> for DeviceId {
    fn from(value: MachineId) -> Self {
        value.0
    }
}

impl std::fmt::Display for MachineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "washer-{:02}", self.0.get())
    }
}

/// Convenience topic helpers for the laundry profile.
pub mod topics {
    use super::MachineId;
    use crate::fleet::{self, FleetProfile};

    pub const TOPIC_ROOT: &str = "laundry/washer";

    fn profile() -> FleetProfile {
        FleetProfile::laundry_washers()
    }

    pub fn cmd(machine: MachineId) -> String {
        fleet::cmd(&profile(), machine.device())
    }

    pub fn status(machine: MachineId) -> String {
        fleet::status(&profile(), machine.device())
    }

    pub fn ota(machine: MachineId) -> String {
        fleet::ota(&profile(), machine.device())
    }

    pub fn ack(machine: MachineId) -> String {
        fleet::ack(&profile(), machine.device())
    }

    pub fn parse_machine(topic: &str) -> Option<MachineId> {
        fleet::parse_device(&profile(), topic).map(MachineId)
    }
}

/// Bootstrap laundry-named fleet into [`ByteflowIo`].
pub fn bootstrap(
    config: IoConfig,
) -> Result<(ByteflowIo, SharedFleet, Arc<LaundryEspDriver>), fleet::BootstrapError> {
    fleet::bootstrap(FleetProfile::laundry_washers(), config)
}

/// Bootstrap with artificial ESP delay (cancel / deadline tests).
pub fn bootstrap_with_delay(
    config: IoConfig,
    delay: Duration,
) -> Result<(ByteflowIo, SharedFleet, Arc<LaundryEspDriver>), fleet::BootstrapError> {
    fleet::bootstrap_with_delay(FleetProfile::laundry_washers(), config, delay)
}

/// Laundry profile + composite fast queue (GPIO/UART on device thread + fleet HostAwait).
pub fn bootstrap_composite(
    config: IoConfig,
    queue_capacity: usize,
) -> Result<
    (
        ByteflowIo,
        SharedFleet,
        crate::IoCommandReceiver,
        Arc<LaundryEspDriver>,
    ),
    fleet::BootstrapError,
> {
    fleet::bootstrap_composite(FleetProfile::laundry_washers(), config, queue_capacity)
}

/// Same as [`bootstrap_composite`], with artificial transport delay.
pub fn bootstrap_composite_with_delay(
    config: IoConfig,
    queue_capacity: usize,
    delay: Duration,
) -> Result<
    (
        ByteflowIo,
        SharedFleet,
        crate::IoCommandReceiver,
        Arc<LaundryEspDriver>,
    ),
    fleet::BootstrapError,
> {
    fleet::bootstrap_composite_with_delay(
        FleetProfile::laundry_washers(),
        config,
        queue_capacity,
        delay,
    )
}
