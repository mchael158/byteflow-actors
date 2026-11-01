//! Generic device identity and host-owned fleet registry.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use byteflow::{FlowId, SendError, Value};

use super::profile::{FleetProfile, ProfileError};
use crate::EventRouter;

/// Validated device identity for a given [`FleetProfile`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceId(u8);

impl DeviceId {
    /// Create an id in `1..=max` for the profile capacity.
    pub fn new(raw: u8, max_devices: u8) -> Option<Self> {
        if max_devices == 0 {
            return None;
        }
        if (1..=max_devices).contains(&raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// Parse from Byteflow `Int` against `max_devices`.
    pub fn from_value(value: &Value, max_devices: u8) -> Option<Self> {
        let n = value.as_int()?;
        let raw = u8::try_from(n).ok()?;
        Self::new(raw, max_devices)
    }

    pub fn get(self) -> u8 {
        self.0
    }

    pub fn index(self) -> usize {
        usize::from(self.0 - 1)
    }
}

/// Host-tracked lifecycle (product-agnostic).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeviceState {
    /// Ready; no pending work.
    Idle,
    /// Armed / credited / unlocked — waiting for the main action.
    Armed,
    /// Action in progress on the edge device.
    Busy,
    /// Fault reported by device or host policy.
    Fault,
    /// Unreachable / never seen.
    #[default]
    Offline,
}

/// Historical laundry names for the same states.
impl DeviceState {
    /// Alias of [`DeviceState::Armed`].
    #[allow(non_upper_case_globals)]
    pub const Credited: Self = Self::Armed;
    /// Alias of [`DeviceState::Busy`].
    #[allow(non_upper_case_globals)]
    pub const Running: Self = Self::Busy;
}

impl From<DeviceState> for i64 {
    fn from(value: DeviceState) -> Self {
        match value {
            DeviceState::Idle => 0,
            DeviceState::Armed => 1,
            DeviceState::Busy => 2,
            DeviceState::Fault => 3,
            DeviceState::Offline => 4,
        }
    }
}

/// One registry slot.
#[derive(Clone, Debug)]
pub struct DeviceSlot {
    pub state: DeviceState,
    pub flow: Option<FlowId>,
}

impl Default for DeviceSlot {
    fn default() -> Self {
        Self {
            state: DeviceState::Offline,
            flow: None,
        }
    }
}

/// Registry / bind / deliver errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FleetError {
    InvalidDevice,
    Unbound,
    /// [`FleetRegistry::bind`] was called while a flow is already bound.
    AlreadyBound,
    Send(SendError),
    Poisoned,
    Profile(ProfileError),
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDevice => write!(f, "device id out of range for this fleet profile"),
            Self::Unbound => write!(f, "no flow is bound to this device"),
            Self::AlreadyBound => write!(f, "device already has a bound flow; unbind first"),
            Self::Send(err) => write!(f, "fleet event delivery failed: {err}"),
            Self::Poisoned => write!(f, "fleet registry mutex is poisoned"),
            Self::Profile(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for FleetError {}

impl From<SendError> for FleetError {
    fn from(value: SendError) -> Self {
        Self::Send(value)
    }
}

impl From<ProfileError> for FleetError {
    fn from(value: ProfileError) -> Self {
        Self::Profile(value)
    }
}

/// Host-owned map of devices → flows/states for one site.
///
/// Bytecode never supplies [`FlowId`]. The host binds after spawn; edge
/// callbacks deliver through [`FleetRegistry::deliver_event`].
#[derive(Debug)]
pub struct FleetRegistry {
    profile: FleetProfile,
    slots: Mutex<HashMap<DeviceId, DeviceSlot>>,
    next_request_id: Mutex<u64>,
}

impl FleetRegistry {
    /// Create a registry sized to `profile.max_devices`.
    pub fn new(profile: FleetProfile) -> Result<Self, FleetError> {
        profile.validate()?;
        let mut slots = HashMap::with_capacity(usize::from(profile.max_devices));
        for raw in 1..=profile.max_devices {
            let id = DeviceId::new(raw, profile.max_devices).expect("in range");
            slots.insert(id, DeviceSlot::default());
        }
        Ok(Self {
            profile,
            slots: Mutex::new(slots),
            next_request_id: Mutex::new(1),
        })
    }

    pub fn profile(&self) -> &FleetProfile {
        &self.profile
    }

    pub fn max_devices(&self) -> u8 {
        self.profile.max_devices
    }

    pub fn device_id(&self, raw: u8) -> Option<DeviceId> {
        DeviceId::new(raw, self.profile.max_devices)
    }

    pub fn all_ids(&self) -> impl Iterator<Item = DeviceId> + '_ {
        let max = self.profile.max_devices;
        (1..=max).filter_map(move |raw| DeviceId::new(raw, max))
    }

    pub fn bind(&self, device: impl Into<DeviceId>, flow: FlowId) -> Result<(), FleetError> {
        let device = device.into();
        self.ensure_known(device)?;
        let mut slots = self.slots.lock().map_err(|_| FleetError::Poisoned)?;
        let slot = slots.get_mut(&device).ok_or(FleetError::InvalidDevice)?;
        if slot.flow.is_some() {
            return Err(FleetError::AlreadyBound);
        }
        slot.flow = Some(flow);
        if slot.state == DeviceState::Offline {
            slot.state = DeviceState::Idle;
        }
        Ok(())
    }

    pub fn unbind(&self, device: impl Into<DeviceId>) -> Result<(), FleetError> {
        let device = device.into();
        self.ensure_known(device)?;
        let mut slots = self.slots.lock().map_err(|_| FleetError::Poisoned)?;
        let slot = slots.get_mut(&device).ok_or(FleetError::InvalidDevice)?;
        slot.flow = None;
        Ok(())
    }

    pub fn lookup(&self, device: impl Into<DeviceId>) -> Result<DeviceSlot, FleetError> {
        let device = device.into();
        self.ensure_known(device)?;
        let slots = self.slots.lock().map_err(|_| FleetError::Poisoned)?;
        slots.get(&device).cloned().ok_or(FleetError::InvalidDevice)
    }

    pub fn set_state(
        &self,
        device: impl Into<DeviceId>,
        state: DeviceState,
    ) -> Result<(), FleetError> {
        let device = device.into();
        self.ensure_known(device)?;
        let mut slots = self.slots.lock().map_err(|_| FleetError::Poisoned)?;
        let slot = slots.get_mut(&device).ok_or(FleetError::InvalidDevice)?;
        slot.state = state;
        Ok(())
    }

    pub fn bound_count(&self) -> Result<usize, FleetError> {
        let slots = self.slots.lock().map_err(|_| FleetError::Poisoned)?;
        Ok(slots.values().filter(|slot| slot.flow.is_some()).count())
    }

    pub fn deliver_event(
        &self,
        events: &EventRouter,
        device: impl Into<DeviceId>,
        tag: u16,
        payload: Value,
    ) -> Result<(), FleetError> {
        let device = device.into();
        self.ensure_known(device)?;
        let flow = {
            let slots = self.slots.lock().map_err(|_| FleetError::Poisoned)?;
            slots
                .get(&device)
                .ok_or(FleetError::InvalidDevice)?
                .flow
                .ok_or(FleetError::Unbound)?
        };
        let request_id = {
            let mut next = self
                .next_request_id
                .lock()
                .map_err(|_| FleetError::Poisoned)?;
            let id = *next;
            *next = next.saturating_add(1);
            id
        };
        events.send(flow, request_id, tag, payload)?;
        Ok(())
    }

    pub fn display_name(&self, device: DeviceId) -> String {
        self.profile.format_id(device.get())
    }

    fn ensure_known(&self, device: DeviceId) -> Result<(), FleetError> {
        if device.get() == 0 || device.get() > self.profile.max_devices {
            Err(FleetError::InvalidDevice)
        } else {
            Ok(())
        }
    }
}

pub type SharedFleet = Arc<FleetRegistry>;
