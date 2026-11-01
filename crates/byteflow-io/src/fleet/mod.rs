//! Generic low-cost device fleet — not tied to washers or any one product.
//!
//! byteflow-io stays a thin host bridge: Byteflow Runtime + pluggable
//! [`DeviceTransport`]. Change only [`FleetProfile`] (capacity, topic root,
//! label) to retarget lockers, sensors, kiosks, laundry, etc.
//!
//! Cost posture:
//! - no MQTT / hardware SDK in this crate (host plugs one in)
//! - in-memory transport for tests and dry-runs
//! - shared registry + one driver for the whole site
//! - ESP32 (or any MCU) stays optional at the edge
//!
//! # Composite wiring
//!
//! Real hosts usually need **both** a bounded fast queue (GPIO/UART on a device
//! thread) and fleet HostAwait work. Use [`bootstrap_composite`]: a
//! [`QueuedIoDriver`] for [`IoDriver::try_submit`] plus [`FleetDriver`] as
//! [`HostAwaitHandler`].

mod device;
mod dispatch;
mod driver;
mod esp32_sim;
mod profile;
mod protocol;
mod topics;
mod transport;

pub use device::{DeviceId, DeviceSlot, DeviceState, FleetError, FleetRegistry, SharedFleet};
pub use dispatch::{dispatch_queued_command, QueuedDispatch};
pub use driver::{FleetDriver, DEFAULT_DEVICE_DEADLINE};
pub use esp32_sim::{announce_online, drain_once, poll, spawn_firmware_thread, SimDrainOutcome};
pub use profile::{FleetProfile, ProfileError};
pub use protocol::{custom_op, event_tag, host_op, DeviceCommand};
pub use topics::{ack, cmd, ota, parse_device, status};
pub use transport::{DeviceTransport, InMemoryDeviceTransport};

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    ByteflowIo, CancellationToken, IoCommandReceiver, IoConfig, IoDriver, IoDriverError,
    IoInitError, QueuedIoDriver,
};

/// Default bounded capacity for the fast command queue in [`bootstrap_composite`].
pub const DEFAULT_FAST_QUEUE_CAPACITY: usize = 64;

struct FleetStack {
    fleet: SharedFleet,
    fleet_driver: Arc<FleetDriver>,
}

fn build_fleet_stack(profile: FleetProfile, delay: Duration) -> Result<FleetStack, FleetError> {
    let fleet = Arc::new(FleetRegistry::new(profile)?);
    let transport = if delay.is_zero() {
        InMemoryDeviceTransport::new(Arc::clone(&fleet))
    } else {
        InMemoryDeviceTransport::with_delay(Arc::clone(&fleet), delay)
    };
    let fleet_driver = FleetDriver::new(Arc::clone(&fleet), transport);
    Ok(FleetStack {
        fleet,
        fleet_driver,
    })
}

/// Wire a profiled fleet into [`ByteflowIo`] with the in-memory transport.
///
/// Fast natives that need GPIO/UART should use [`bootstrap_composite`] instead.
pub fn bootstrap(
    profile: FleetProfile,
    config: IoConfig,
) -> Result<(ByteflowIo, SharedFleet, Arc<FleetDriver>), BootstrapError> {
    bootstrap_with_delay(profile, config, Duration::ZERO)
}

/// Same as [`bootstrap`], with artificial transport delay (cancel / deadline tests).
pub fn bootstrap_with_delay(
    profile: FleetProfile,
    config: IoConfig,
    delay: Duration,
) -> Result<(ByteflowIo, SharedFleet, Arc<FleetDriver>), BootstrapError> {
    let FleetStack {
        fleet,
        fleet_driver,
    } = build_fleet_stack(profile, delay)?;
    let io = ByteflowIo::new(Arc::clone(&fleet_driver) as Arc<dyn IoDriver>, config)?;
    Ok((io, fleet, fleet_driver))
}

/// Fleet HostAwait + bounded fast queue (GPIO/UART/MQTT enqueue on a device thread).
///
/// Returns the [`IoCommandReceiver`] to drain on a host-owned thread. For each
/// dequeued item, call [`dispatch_queued_command`] so MQTT / fleet `Custom`
/// commands reach the edge transport (GPIO/UART stay on the host thread).
pub fn bootstrap_composite(
    profile: FleetProfile,
    config: IoConfig,
    queue_capacity: usize,
) -> Result<(ByteflowIo, SharedFleet, IoCommandReceiver, Arc<FleetDriver>), BootstrapError> {
    bootstrap_composite_with_delay(profile, config, queue_capacity, Duration::ZERO)
}

/// Same as [`bootstrap_composite`], with artificial transport delay.
pub fn bootstrap_composite_with_delay(
    profile: FleetProfile,
    config: IoConfig,
    queue_capacity: usize,
    delay: Duration,
) -> Result<(ByteflowIo, SharedFleet, IoCommandReceiver, Arc<FleetDriver>), BootstrapError> {
    let FleetStack {
        fleet,
        fleet_driver,
    } = build_fleet_stack(profile, delay)?;
    let (queued, receiver) = QueuedIoDriver::new(queue_capacity, Arc::clone(&fleet_driver))?;
    let io = ByteflowIo::new(Arc::clone(&queued) as Arc<dyn IoDriver>, config)?;
    Ok((io, fleet, receiver, fleet_driver))
}

/// Errors from fleet bootstrap.
#[derive(Debug)]
pub enum BootstrapError {
    Fleet(FleetError),
    Io(IoInitError),
    Driver(IoDriverError),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fleet(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Driver(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fleet(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Driver(e) => Some(e),
        }
    }
}

impl From<FleetError> for BootstrapError {
    fn from(value: FleetError) -> Self {
        Self::Fleet(value)
    }
}

impl From<IoInitError> for BootstrapError {
    fn from(value: IoInitError) -> Self {
        Self::Io(value)
    }
}

impl From<IoDriverError> for BootstrapError {
    fn from(value: IoDriverError) -> Self {
        Self::Driver(value)
    }
}

/// Cancel a HostAwait token (tests / host policies).
pub fn abort_token(token: &CancellationToken) {
    token.cancel();
}
