//! Vendor knowledge for push events, split three ways so each vendor's part reads top to bottom:
//!
//! ```text
//!   where   EventChannel     data: which HID collections and report ids carry unsolicited frames
//!   layout  <vendor> protocol module, shared with the poll driver: byte offsets live in one place
//!   meaning DeviceSignal     vendor-neutral, optional fields: each decoder fills what it knows
//! ```
//!
//! `HidEventListener` (the engine) never knows a vendor. To add one: write the protocol parser
//! (with a frame test), implement [`HidEventDecoder`] in `<vendor>_events.rs` next to its driver,
//! and register it in [`hid_event_decoders`].
//!
//! Ported from event_decoders.h of the C++ BatteryHub (PR #535 by Karan Shukla, GPL-3.0).
use std::collections::HashMap;

use crate::hid_coll::HidColl;

/// Where a vendor's unsolicited frames arrive. Empty lists match anything.
#[derive(Clone, Debug, Default)]
pub struct EventChannel {
    pub vid: u16,
    pub pids: Vec<u16>,
    /// (usage page, usage) pairs of the collections to hold open.
    pub usages: Vec<(u16, u16)>,
    /// First byte of the frame; frames with other report ids never reach `decode`.
    pub report_ids: Vec<u8>,
    pub min_input_len: u16,
}

impl EventChannel {
    pub fn matches(&self, c: &HidColl) -> bool {
        c.vid == self.vid
            && c.in_len() >= self.min_input_len
            && (self.pids.is_empty() || self.pids.contains(&c.pid))
            && (self.usages.is_empty() || self.usages.contains(&(c.page(), c.usage())))
    }

    pub fn accepts(&self, frame: &[u8]) -> bool {
        !frame.is_empty() && (self.report_ids.is_empty() || self.report_ids.contains(&frame[0]))
    }
}

/// What a frame says about one device. A field is set only when the frame carries it: a Logitech
/// receiver says "slot 2 is back" (present), a device that pushes its battery fills percent and
/// charging. Any change is reason to poll.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSignal {
    /// Which device on the channel (receiver slot, ...); empty = the only one.
    pub subject: String,
    pub present: Option<bool>,
    pub percent: Option<i32>,
    pub charging: Option<bool>,
    /// The frame reports an event ("just reconnected"), not a state: always news, never a repeat.
    pub momentary: bool,
    /// For the `events` diagnostic, e.g. "device 2 connected".
    pub what: String,
}

pub trait HidEventDecoder: Send + Sync {
    fn name(&self) -> &'static str;
    fn channel(&self) -> &EventChannel;
    /// Called only for frames the channel accepts, read from collection `c`. None when the frame
    /// is not an event (a reply, input, noise).
    fn decode(&self, c: &HidColl, frame: &[u8]) -> Option<DeviceSignal>;
    /// How long to wait after a signal before polling (a just-connected device needs a moment).
    fn settle_ms(&self) -> u32 {
        1500
    }
}

/// Signals are edges, not levels. Vendor software (G HUB, Options+, Solaar) asks Logitech
/// receivers to re-announce paired devices every few seconds, and devices that push their battery
/// repeat the same value; only a change is worth a poll. Applied by the engine to every decoder.
#[derive(Default)]
pub struct SignalEdges {
    known: HashMap<String, Known>,
}

#[derive(Default)]
struct Known {
    present: Option<bool>,
    percent: Option<i32>,
    charging: Option<bool>,
}

impl SignalEdges {
    /// `channel` identifies where the signal came from (the collection path).
    pub fn is_change(&mut self, channel: &str, s: &DeviceSignal) -> bool {
        let key = format!("{channel}|{}", s.subject);
        let inserted = !self.known.contains_key(&key);
        let k = self.known.entry(key).or_default();
        // First word about this device: it cannot be a repeat.
        let mut changed = inserted || s.momentary;
        changed |= update(&mut k.present, s.present);
        changed |= update(&mut k.percent, s.percent);
        changed |= update(&mut k.charging, s.charging);
        changed
    }
}

fn update<T: PartialEq + Copy>(known: &mut Option<T>, now: Option<T>) -> bool {
    match now {
        Some(v) if *known != Some(v) => {
            *known = Some(v);
            true
        }
        _ => false,
    }
}

/// Every registered decoder, for the listener.
pub fn hid_event_decoders() -> Vec<Box<dyn HidEventDecoder>> {
    vec![Box::new(super::logitech_events::LogitechEventDecoder)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coll(vid: u16, pid: u16, page: u16, usage: u16, input_len: u16) -> HidColl {
        HidColl { vid, pid, usage_page: page, usage_id: usage, input_len, ..Default::default() }
    }

    #[test]
    fn channel_matching() {
        let ch = EventChannel { vid: 0x046D, usages: vec![(0xFF00, 1), (0xFF00, 2)], report_ids: vec![0x10, 0x11], min_input_len: 7, ..Default::default() };
        assert!(ch.matches(&coll(0x046D, 0xC547, 0xFF00, 1, 7)));
        assert!(ch.matches(&coll(0x046D, 0xC548, 0xFF00, 2, 20)));
        assert!(!ch.matches(&coll(0x046D, 0xC547, 0x0001, 2, 7)), "the mouse collection itself");
        assert!(!ch.matches(&coll(0x1038, 0x12AD, 0xFF00, 1, 7)), "another vendor");
        assert!(!ch.matches(&coll(0x046D, 0xC547, 0xFF00, 1, 3)), "too short for HID++");
        assert!(ch.accepts(&[0x10, 1, 0x41]));
        assert!(!ch.accepts(&[0x20, 1, 0x41]));
        assert!(!ch.accepts(&[]));
        let pinned = EventChannel { vid: 0x046D, pids: vec![0xC547], ..Default::default() };
        assert!(pinned.matches(&coll(0x046D, 0xC547, 0xFF00, 1, 7)));
        assert!(!pinned.matches(&coll(0x046D, 0xC548, 0xFF00, 1, 7)));
    }

    fn present(subject: &str, on: bool) -> DeviceSignal {
        DeviceSignal { subject: subject.into(), present: Some(on), ..Default::default() }
    }

    #[test]
    fn edges_ignore_repeats() {
        let mut e = SignalEdges::default();
        assert!(e.is_change("rx", &present("1", true)), "first word");
        assert!(!e.is_change("rx", &present("1", true)), "re-announced by vendor software");
        assert!(e.is_change("rx", &present("1", false)));
        assert!(e.is_change("rx", &present("1", true)));
        assert!(e.is_change("rx", &present("2", true)), "another slot");
        assert!(e.is_change("rx2", &present("1", true)), "another receiver");
    }

    #[test]
    fn edges_compare_every_field() {
        let mut e = SignalEdges::default();
        let batt = |p: i32, c: bool| DeviceSignal { percent: Some(p), charging: Some(c), ..Default::default() };
        assert!(e.is_change("dev", &batt(80, false)));
        assert!(!e.is_change("dev", &batt(80, false)));
        assert!(e.is_change("dev", &batt(79, false)));
        assert!(e.is_change("dev", &batt(79, true)));
        // A signal that carries only presence leaves the battery fields alone.
        assert!(e.is_change("dev", &present("", true)));
        assert!(!e.is_change("dev", &batt(79, true)));
    }

    #[test]
    fn momentary_signals_are_always_news() {
        let mut e = SignalEdges::default();
        let wake = DeviceSignal { subject: "1".into(), present: Some(true), momentary: true, ..Default::default() };
        assert!(e.is_change("rx", &wake));
        assert!(e.is_change("rx", &wake), "woke up again after sleeping");
    }

    #[test]
    fn registry_has_logitech() {
        let names: Vec<&str> = hid_event_decoders().iter().map(|d| d.name()).collect();
        assert!(names.contains(&"Logitech HID++"));
    }
}
