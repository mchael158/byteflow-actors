//! Site profile: capacity and topic layout without tying to one product.

use std::fmt;

/// Configuration for a low-cost device fleet of any kind.
///
/// The same Byteflow I/O stack can drive washers, dryers, lockers, kiosks,
/// sensors, etc. Only this profile changes — not the runtime contracts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetProfile {
    /// Inclusive maximum device id (`1..=max_devices`). Cap is `u8::MAX`.
    pub max_devices: u8,
    /// MQTT (or logical) topic root, e.g. `site/device` or `laundry/washer`.
    pub topic_root: String,
    /// Short label used in logs / `Display` (`device`, `washer`, `locker`…).
    pub label: String,
}

impl FleetProfile {
    /// Generic default: up to 56 endpoints under `site/device`.
    ///
    /// 56 is a practical low-cost ESP mesh size, not a washer requirement.
    pub fn generic_site() -> Self {
        Self {
            max_devices: 56,
            topic_root: "site/device".to_owned(),
            label: "device".to_owned(),
        }
    }

    /// Optional laundry profile (same mechanics; washer naming only).
    pub fn laundry_washers() -> Self {
        Self {
            max_devices: 56,
            topic_root: "laundry/washer".to_owned(),
            label: "washer".to_owned(),
        }
    }

    /// Validate `max_devices >= 1`.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.max_devices == 0 {
            return Err(ProfileError::ZeroCapacity);
        }
        if self.topic_root.is_empty() || self.label.is_empty() {
            return Err(ProfileError::EmptyName);
        }
        Ok(())
    }

    pub(crate) fn format_id(&self, id: u8) -> String {
        format!("{}-{:02}", self.label, id)
    }
}

impl Default for FleetProfile {
    fn default() -> Self {
        Self::generic_site()
    }
}

impl fmt::Display for FleetProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}[1..={}] @ {}",
            self.label, self.max_devices, self.topic_root
        )
    }
}

/// Invalid [`FleetProfile`] construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileError {
    ZeroCapacity,
    EmptyName,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => write!(f, "fleet max_devices must be >= 1"),
            Self::EmptyName => write!(f, "fleet topic_root and label must be non-empty"),
        }
    }
}

impl std::error::Error for ProfileError {}
