//! Logitech HID++ frames that arrive on their own (not as a reply), and the battery payloads the
//! poll driver and the event decoder share. Free of Win32 so it can be unit tested anywhere.
//!
//! Device connection, HID++ 1.0 sub id 0x41, sent by Unifying / Bolt / LIGHTSPEED receivers:
//!   `[0x10][device index 1..6][0x41][protocol][flags][wpid lo][wpid hi]`
//!   flags bit 6 set = link not established (device switched off or out of range).
//! The receiver sends it only while bit 0x000100 ("wireless") of its register 0x00 is set. This
//! app never writes that register; G HUB, Options+ and Solaar set it.
//!
//! HID++ 2.0 feature events (sw id 0, function 0), sent by the device itself, so they also arrive
//! from LIGHTSPEED dongles that speak only HID++ 2.0, from G headsets and over Bluetooth:
//!   `[0x10|0x11][device index][feature index][0x00][params...]`
//!   0x1D4B WIRELESS_DEVICE_STATUS: params[0] == 1 = the device (re)connected
//!   0x1004 UNIFIED_BATTERY:        params = state of charge, level flags, charging status
//!   0x1000 BATTERY_STATUS:         params = level, next level, charging status
//! Feature indexes differ per device, so the poll driver remembers the ones it found
//! ([`remember_feature`]) and the decoder only trusts frames on those indexes. Our own requests
//! use sw id 0x0D and other apps' use their own, so no reply can look like an event: no feedback loop.
//!
//! Sources: Solaar (hidpp10 / hidpp20 notifications), openlogi-hidpp (NotificationFlags).
//! Ported from hidpp_events.h of the C++ BatteryHub (PR #535 by Karan Shukla, GPL-3.0).
use std::collections::HashMap;
use std::sync::Mutex;

pub const FEATURE_WIRELESS_STATUS: u16 = 0x1D4B;
pub const FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
pub const FEATURE_BATTERY_STATUS: u16 = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    None,
    /// Receiver: the device on `index` connected (`link_up`) or went away.
    DeviceConnection { index: u8, link_up: bool },
}

pub fn is_hidpp_report(r: &[u8]) -> bool {
    r.len() >= 7 && (r[0] == 0x10 || r[0] == 0x11)
}

pub fn classify(r: &[u8]) -> Notification {
    // 0x41 is not a register-access sub id (0x80..0x83), so a reply to anyone's request can never
    // look like this.
    if !is_hidpp_report(r) || !(1..=6).contains(&r[1]) || r[2] != 0x41 {
        return Notification::None;
    }
    Notification::DeviceConnection { index: r[1], link_up: r[4] & 0x40 == 0 }
}

pub const WIRELESS_NOTIFICATIONS: u32 = 0x00_0100;

/// Request for register 0x00 of the receiver itself (index 0xFF): a read, never a write.
pub fn notification_flags_request() -> [u8; 7] {
    [0x10, 0xFF, 0x81, 0x00, 0, 0, 0]
}

/// Parses the reply to [`notification_flags_request`]. None for any other frame.
pub fn parse_notification_flags(r: &[u8]) -> Option<u32> {
    if !is_hidpp_report(r) || r[1] != 0xFF || r[2] != 0x81 || r[3] != 0x00 {
        return None;
    }
    Some((r[4] as u32) << 16 | (r[5] as u32) << 8 | r[6] as u32)
}

/// A battery reading as HID++ 2.0 reports it, in poll replies and in events alike.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Battery {
    /// -1 when unknown.
    pub percent: i32,
    /// "Full" / "Good" / "Low" / "Critical" when the device reports only a level.
    pub level_text: &'static str,
    pub charging: bool,
    pub full: bool,
}

/// UNIFIED_BATTERY (0x1004) get_status reply or event: `[soc][level flags][status]`.
pub fn unified_battery(p: &[u8]) -> Option<Battery> {
    if p.len() < 3 {
        return None;
    }
    let (soc, level, st) = (p[0] as i32, p[1], p[2]);
    let mut b = Battery { percent: -1, ..Default::default() };
    if soc > 0 && soc <= 100 {
        b.percent = soc;
    } else if level != 0 {
        (b.percent, b.level_text) = if level & 8 != 0 {
            (100, "Full")
        } else if level & 4 != 0 {
            (60, "Good")
        } else if level & 2 != 0 {
            (20, "Low")
        } else {
            (5, "Critical")
        };
    }
    b.charging = matches!(st, 1..=3);
    b.full = st == 3;
    Some(b)
}

/// BATTERY_STATUS (0x1000) get_level reply or event: `[level][next level][status]`.
pub fn battery_status(p: &[u8]) -> Option<Battery> {
    if p.len() < 3 {
        return None;
    }
    let st = p[2];
    let mut b = Battery {
        percent: if p[0] <= 100 { p[0] as i32 } else { -1 },
        charging: (1..=4).contains(&st),
        full: st == 3,
        ..Default::default()
    };
    if b.percent == 0 && st == 3 {
        b.percent = 100;
    }
    Some(b)
}

/// What a HID++ 2.0 feature event says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureEvent {
    /// WIRELESS_DEVICE_STATUS: the device just (re)connected.
    Reconnected,
    Battery(Battery),
}

/// Decodes a HID++ 2.0 event whose feature index is `feature_id` on that device.
pub fn decode_feature_event(feature_id: u16, r: &[u8]) -> Option<FeatureEvent> {
    // Function 0 with sw id 0: an event, not a reply to anyone.
    if !is_hidpp_report(r) || r[3] != 0x00 {
        return None;
    }
    let p = &r[4..];
    match feature_id {
        FEATURE_WIRELESS_STATUS => (p[0] == 1).then_some(FeatureEvent::Reconnected),
        FEATURE_UNIFIED_BATTERY => unified_battery(p).map(FeatureEvent::Battery),
        FEATURE_BATTERY_STATUS => battery_status(p).map(FeatureEvent::Battery),
        _ => None,
    }
}

/// Feature indexes the poll driver found, per physical receiver / cable / Bluetooth link
/// ([`crate::hid_coll::HidColl::physical_key`]) and device index.
static FEATURES: Mutex<Option<FeatureMap>> = Mutex::new(None);

/// (link, device index) -> (feature index, feature id) pairs.
type FeatureMap = HashMap<(String, u8), Vec<(u8, u16)>>;

/// The poll driver found `feature_id` at `index` on device `dev` behind `link`. Index 0 records
/// that the device does not have the feature, so it is not asked again; it never matches an event.
pub fn remember_feature(link: &str, dev: u8, feature_id: u16, index: u8) {
    if link.is_empty() {
        return;
    }
    let mut map = FEATURES.lock().unwrap_or_else(|e| e.into_inner());
    let list = map.get_or_insert_with(HashMap::new).entry((link.to_string(), dev)).or_default();
    list.retain(|&(i, f)| (i != index || index == 0) && f != feature_id);
    list.push((index, feature_id));
}

/// The feature at `index` on device `dev` behind `link`, if the poll driver saw it.
pub fn feature_at(link: &str, dev: u8, index: u8) -> Option<u16> {
    if index == 0 {
        return None;
    }
    let map = FEATURES.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()?.get(&(link.to_string(), dev))?.iter().find(|&&(i, _)| i == index).map(|&(_, f)| f)
}

/// The index of `feature_id` on device `dev` behind `link`, if already known (saves a request).
pub fn known_index(link: &str, dev: u8, feature_id: u16) -> Option<u8> {
    let map = FEATURES.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()?.get(&(link.to_string(), dev))?.iter().find(|&&(_, f)| f == feature_id).map(|&(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_connection_events() {
        // Layout as Solaar decodes it: device 1 on a LIGHTSPEED receiver switched on, then off.
        assert_eq!(classify(&[0x10, 0x01, 0x41, 0x0C, 0x02, 0x9A, 0x40]), Notification::DeviceConnection { index: 1, link_up: true });
        assert_eq!(classify(&[0x10, 0x01, 0x41, 0x0C, 0x42, 0x9A, 0x40]), Notification::DeviceConnection { index: 1, link_up: false });
        // Bolt (protocol 0x10), device 2.
        assert_eq!(classify(&[0x10, 0x02, 0x41, 0x10, 0x01, 0x82, 0xB0]), Notification::DeviceConnection { index: 2, link_up: true });
    }

    #[test]
    fn replies_and_noise_are_not_connection_events() {
        assert_eq!(classify(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), Notification::None); // register reply
        assert_eq!(classify(&[0x10, 0x00, 0x41, 0, 0, 0, 0]), Notification::None); // index 0 is not a device
        assert_eq!(classify(&[0x10, 0x07, 0x41, 0, 0, 0, 0]), Notification::None);
        assert_eq!(classify(&[0x20, 0x01, 0x41, 0, 0, 0, 0]), Notification::None); // not HID++
        assert_eq!(classify(&[0x10, 0x01, 0x41]), Notification::None); // too short
    }

    #[test]
    fn notification_flags() {
        assert_eq!(notification_flags_request(), [0x10, 0xFF, 0x81, 0x00, 0, 0, 0]);
        let flags = parse_notification_flags(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]).unwrap();
        assert_ne!(flags & WIRELESS_NOTIFICATIONS, 0);
        assert_eq!(parse_notification_flags(&[0x10, 0xFF, 0x81, 0x00, 0x00, 0x00, 0x00]), Some(0));
        assert_eq!(parse_notification_flags(&[0x10, 0x01, 0x81, 0x00, 0x00, 0x01, 0x00]), None);
    }

    #[test]
    fn unified_battery_payloads() {
        assert_eq!(unified_battery(&[86, 4, 0]), Some(Battery { percent: 86, level_text: "", charging: false, full: false }));
        assert_eq!(unified_battery(&[0, 2, 1]), Some(Battery { percent: 20, level_text: "Low", charging: true, full: false }));
        assert_eq!(unified_battery(&[100, 8, 3]), Some(Battery { percent: 100, level_text: "", charging: true, full: true }));
        assert_eq!(unified_battery(&[50]), None);
    }

    #[test]
    fn battery_status_payloads() {
        assert_eq!(battery_status(&[40, 20, 0]).unwrap().percent, 40);
        assert_eq!(battery_status(&[0, 0, 3]).unwrap().percent, 100); // full, level not reported
        assert!(battery_status(&[70, 50, 1]).unwrap().charging);
        assert_eq!(battery_status(&[0xFF, 0, 0]).unwrap().percent, -1);
    }

    #[test]
    fn feature_events() {
        // A device on slot 2 (e.g. an MX Master 3S on a Bolt receiver): UNIFIED_BATTERY event, 55%, discharging.
        let ev = [0x11, 0x02, 0x08, 0x00, 55, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decode_feature_event(FEATURE_UNIFIED_BATTERY, &ev), Some(FeatureEvent::Battery(unified_battery(&[55, 4, 0]).unwrap())));
        // WIRELESS_DEVICE_STATUS: reconnection.
        let ev = [0x11, 0x01, 0x05, 0x00, 0x01, 0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decode_feature_event(FEATURE_WIRELESS_STATUS, &ev), Some(FeatureEvent::Reconnected));
        // Our own get_status reply (sw id 0x0D) is not an event.
        let reply = [0x11, 0x02, 0x08, 0x1D, 55, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decode_feature_event(FEATURE_UNIFIED_BATTERY, &reply), None);
        // A feature we do not decode.
        assert_eq!(decode_feature_event(0x2201, &ev), None);
    }

    #[test]
    fn feature_registry() {
        let link = "test-link#feature_registry";
        assert_eq!(feature_at(link, 1, 0x08), None);
        remember_feature(link, 1, FEATURE_UNIFIED_BATTERY, 0x08);
        remember_feature(link, 1, FEATURE_WIRELESS_STATUS, 0x05);
        remember_feature(link, 1, 0x1234, 0); // index 0 = the device does not have it
        assert_eq!(known_index(link, 1, 0x1234), Some(0), "not asked again");
        assert_eq!(feature_at(link, 1, 0), None, "never matches an event");
        assert_eq!(feature_at(link, 1, 0x08), Some(FEATURE_UNIFIED_BATTERY));
        assert_eq!(known_index(link, 1, FEATURE_WIRELESS_STATUS), Some(0x05));
        assert_eq!(feature_at(link, 2, 0x08), None, "per device index");
        // A different device paired on the same slot: its index replaces the old one.
        remember_feature(link, 1, FEATURE_UNIFIED_BATTERY, 0x09);
        assert_eq!(feature_at(link, 1, 0x08), None);
        assert_eq!(feature_at(link, 1, 0x09), Some(FEATURE_UNIFIED_BATTERY));
    }
}
