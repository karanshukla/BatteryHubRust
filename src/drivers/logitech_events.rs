//! Logitech push events:
//!  * device-connection notifications (0x41) from Unifying / Bolt / LIGHTSPEED receivers;
//!  * HID++ 2.0 feature events from the device itself: WIRELESS_DEVICE_STATUS (reconnected) and
//!    battery events, which also come from HID++ 2.0-only LIGHTSPEED dongles, G headsets and
//!    Bluetooth devices, none of which send 0x41.
//!
//! Frame layouts and the notification-flag caveat: [`super::hidpp_events`].
//! Ported from logitech_events.cpp of the C++ BatteryHub (PR #535 by Karan Shukla, GPL-3.0) and
//! extended with the HID++ 2.0 feature events.
use std::sync::OnceLock;

use super::event_decoders::{DeviceSignal, EventChannel, HidEventDecoder};
use super::hidpp_events::{self as hidpp, FeatureEvent, Notification};
use crate::hid_coll::HidColl;

pub struct LogitechEventDecoder;

impl HidEventDecoder for LogitechEventDecoder {
    fn name(&self) -> &'static str {
        "Logitech HID++"
    }

    fn channel(&self) -> &EventChannel {
        static CHANNEL: OnceLock<EventChannel> = OnceLock::new();
        CHANNEL.get_or_init(|| EventChannel {
            vid: 0x046D,
            usages: vec![
                (0xFF00, 0x0001), // receivers: short HID++ collection
                (0xFF00, 0x0002), // receivers: long HID++ collection
                (0xFF43, 0x0202), // Bluetooth devices and G headsets: long HID++ collection
                (0xFF43, 0x0301), // HID++ 2.0-only LIGHTSPEED dongles (PRO 2): short
                (0xFF43, 0x0302), // and long
            ],
            report_ids: vec![0x10, 0x11], // short and long HID++ reports
            min_input_len: 7,
            ..Default::default()
        })
    }

    fn decode(&self, c: &HidColl, r: &[u8]) -> Option<DeviceSignal> {
        if let Notification::DeviceConnection { index, link_up } = hidpp::classify(r) {
            let subject = index.to_string();
            let what = format!("device {subject} {}", if link_up { "connected" } else { "disconnected" });
            return Some(DeviceSignal { subject, present: Some(link_up), what, ..Default::default() });
        }
        if !hidpp::is_hidpp_report(r) {
            return None;
        }
        let (dev, index) = (r[1], r[2]);
        let feature = hidpp::feature_at(&c.physical_key(), dev, index)?;
        let subject = dev.to_string();
        match hidpp::decode_feature_event(feature, r)? {
            FeatureEvent::Reconnected => Some(DeviceSignal {
                what: format!("device {subject} reconnected"),
                subject,
                present: Some(true),
                momentary: true,
                ..Default::default()
            }),
            FeatureEvent::Battery(b) => Some(DeviceSignal {
                what: format!(
                    "device {subject} battery {}{}",
                    if b.percent >= 0 { format!("{}%", b.percent) } else { "?".into() },
                    if b.charging { ", charging" } else { "" }
                ),
                subject,
                present: Some(true),
                percent: (b.percent >= 0).then_some(b.percent),
                charging: Some(b.charging),
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver(path: &str, usage: u16, len: u16) -> HidColl {
        HidColl { path: path.into(), vid: 0x046D, pid: 0xC547, usage_page: 0xFF00, usage_id: usage, input_len: len, ..Default::default() }
    }

    const SHORT: &str = r"\\?\hid#vid_046d&pid_c547&mi_02&col01#8&2a2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const LONG: &str = r"\\?\hid#vid_046d&pid_c547&mi_02&col02#8&2a2b3c4d&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";

    #[test]
    fn listens_on_receivers_dongles_headsets_and_bluetooth() {
        let d = LogitechEventDecoder;
        let ch = d.channel();
        assert!(ch.matches(&receiver(SHORT, 1, 7)));
        assert!(ch.matches(&receiver(LONG, 2, 20)));
        let headset = HidColl { vid: 0x046D, pid: 0x0AFE, usage_page: 0xFF43, usage_id: 0x0202, input_len: 20, ..Default::default() };
        assert!(ch.matches(&headset));
        let mouse = HidColl { vid: 0x046D, pid: 0xC547, usage_page: 0x0001, usage_id: 0x0002, input_len: 8, ..Default::default() };
        assert!(!ch.matches(&mouse), "never the mouse input itself");
    }

    #[test]
    fn decodes_receiver_connection() {
        let d = LogitechEventDecoder;
        let c = receiver(SHORT, 1, 7);
        let s = d.decode(&c, &[0x10, 0x02, 0x41, 0x10, 0x01, 0x82, 0xB0]).unwrap();
        assert_eq!((s.subject.as_str(), s.present, s.what.as_str()), ("2", Some(true), "device 2 connected"));
        let s = d.decode(&c, &[0x10, 0x02, 0x41, 0x10, 0x41, 0x82, 0xB0]).unwrap();
        assert_eq!(s.present, Some(false));
        assert_eq!(d.decode(&c, &[0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00]), None, "a register reply");
    }

    #[test]
    fn decodes_feature_events_only_on_features_the_poll_found() {
        let d = LogitechEventDecoder;
        let long = receiver(LONG, 2, 20);
        let link = long.physical_key();
        let battery = [0x11, 0x03, 0x08, 0x00, 42, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(d.decode(&long, &battery), None, "index 8 unknown yet: could be any feature");

        hidpp::remember_feature(&link, 3, hidpp::FEATURE_UNIFIED_BATTERY, 0x08);
        hidpp::remember_feature(&link, 3, hidpp::FEATURE_WIRELESS_STATUS, 0x05);
        let s = d.decode(&long, &battery).unwrap();
        assert_eq!((s.percent, s.charging), (Some(42), Some(true)));
        assert_eq!(s.what, "device 3 battery 42%, charging");

        // The short collection of the same receiver shares the feature map.
        let short = receiver(SHORT, 1, 7);
        let wake = [0x10, 0x03, 0x05, 0x00, 0x01, 0x01, 0x01];
        let s = d.decode(&short, &wake).unwrap();
        assert!(s.momentary && s.present == Some(true));
        // Our own reply on the same index is not an event.
        assert_eq!(d.decode(&long, &[0x11, 0x03, 0x08, 0x1D, 42, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);
    }
}
