//! Topic helpers driven by [`FleetProfile::topic_root`].

use super::device::DeviceId;
use super::profile::FleetProfile;

pub fn cmd(profile: &FleetProfile, device: DeviceId) -> String {
    format!("{}/{}/cmd", profile.topic_root, device.get())
}

pub fn status(profile: &FleetProfile, device: DeviceId) -> String {
    format!("{}/{}/status", profile.topic_root, device.get())
}

pub fn ota(profile: &FleetProfile, device: DeviceId) -> String {
    format!("{}/{}/ota", profile.topic_root, device.get())
}

pub fn ack(profile: &FleetProfile, device: DeviceId) -> String {
    format!("{}/{}/ack", profile.topic_root, device.get())
}

/// Parse `"{topic_root}/{id}/…"` into a [`DeviceId`].
pub fn parse_device(profile: &FleetProfile, topic: &str) -> Option<DeviceId> {
    let prefix = format!("{}/", profile.topic_root);
    let rest = topic.strip_prefix(&prefix)?;
    let id_str = rest.split('/').next()?;
    let raw: u8 = id_str.parse().ok()?;
    DeviceId::new(raw, profile.max_devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_generic_and_laundry_roots() {
        let generic = FleetProfile::generic_site();
        let id = DeviceId::new(7, generic.max_devices).expect("valid");
        assert_eq!(parse_device(&generic, &cmd(&generic, id)), Some(id));
        assert_eq!(cmd(&generic, id), "site/device/7/cmd");

        let laundry = FleetProfile::laundry_washers();
        assert_eq!(cmd(&laundry, id), "laundry/washer/7/cmd");
        assert_eq!(parse_device(&laundry, &ack(&laundry, id)), Some(id));
    }
}
