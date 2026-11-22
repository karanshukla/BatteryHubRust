//! Logitech HID++ (VID 046D): Unifying / Bolt / Lightspeed / Nano receivers, wired or Bluetooth
//! devices and G headsets. Features are discovered through the root feature, so devices are
//! supported without a per-model table.
//!
//! Message: [report][device index][feature index][function << 4 | sw id][params...]
//!   report 0x10 = short (7 bytes), 0x11 = long (20 bytes)
//!   HID++ 2.0 error: [.., 0xFF, feature, fn|sw, code]   HID++ 1.0 error: [.., 0x8F, sub id, address, code]
//! Battery features: 0x1004 unified battery, 0x1000 battery level, 0x1001 battery voltage,
//! 0x1F20 ADC measurement (headsets). HID++ 1.0 devices: registers 0x0D and 0x07.
//! References: Solaar, HeadsetControl (see the original logitech.cpp).
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use super::hidpp_events::{self, FEATURE_BATTERY_STATUS, FEATURE_UNIFIED_BATTERY, FEATURE_WIRELESS_STATUS};
use crate::hid::{self, HidColl, HidDev};
use crate::util::{guess_kind, hex4, interpolate, Deadline};

const SW_ID: u8 = 0x0D;

/// LIGHTSPEED dongles that speak only HID++ 2.0 and relay one paired device on index 1.
fn is_passthrough_dongle(pid: u16) -> bool {
    pid == 0xC54F
}

enum Reply {
    Ok(Vec<u8>),
    Err(u8),
    None,
}

struct Hidpp {
    s: Option<HidDev>,
    l: Option<HidDev>,
}

impl Hidpp {
    fn open(short_c: Option<&HidColl>, long_c: Option<&HidColl>) -> Option<Self> {
        let s = short_c.and_then(HidDev::open);
        let l = long_c.and_then(HidDev::open);
        (s.is_some() || l.is_some()).then_some(Self { s, l })
    }

    fn request(&self, idx: u8, b2: u8, b3: u8, params: &[u8], force_short: bool, timeout_ms: u64) -> Reply {
        let use_long = self.l.is_some() && !force_short;
        let dev = if use_long { self.l.as_ref() } else { self.s.as_ref() };
        let Some(dev) = dev else { return Reply::None };
        let mut msg = vec![0u8; if use_long { 20 } else { 7 }];
        msg[0] = if use_long { 0x11 } else { 0x10 };
        msg[1] = idx;
        msg[2] = b2;
        msg[3] = b3;
        for (i, v) in params.iter().enumerate() {
            if 4 + i < msg.len() {
                msg[4 + i] = *v;
            }
        }
        if !dev.write(&msg) {
            return Reply::None;
        }
        let end = Deadline::in_ms(timeout_ms);
        while !end.passed() {
            for d in [self.l.as_ref(), self.s.as_ref()].into_iter().flatten() {
                let Some(r) = d.read(15) else { continue };
                if r.len() < 7 || (r[0] != 0x10 && r[0] != 0x11) || r[1] != idx {
                    continue;
                }
                if r[2] == b2 && r[3] == b3 {
                    return Reply::Ok(r[4..].to_vec());
                }
                if (r[2] == 0xFF || r[2] == 0x8F) && r[3] == b2 && r[4] == b3 {
                    return Reply::Err(r[5]);
                }
            }
        }
        Reply::None
    }

    /// HID++ 2.0 call: feature index + function number.
    fn call(&self, idx: u8, feature: u8, func: u8, params: &[u8]) -> Option<Vec<u8>> {
        match self.request(idx, feature, (func << 4) | SW_ID, params, false, 500) {
            Reply::Ok(p) => Some(p),
            _ => None,
        }
    }

    fn feature_index(&self, idx: u8, feature_id: u16) -> u8 {
        self.call(idx, 0x00, 0, &[(feature_id >> 8) as u8, feature_id as u8])
            .and_then(|p| p.first().copied())
            .unwrap_or(0)
    }
}

/// Voltage -> % for a typical Li-ion cell (mice/keyboards).
fn li_ion_percent(mv: i32) -> i32 {
    const V: [i32; 13] = [4186, 4067, 3989, 3922, 3859, 3811, 3778, 3751, 3717, 3671, 3646, 3579, 3500];
    const P: [i32; 13] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 5, 2, 0];
    interpolate(mv, &V, &P)
}

/// Voltage -> % for G headsets (HeadsetControl and G HUB power models).
fn headset_percent(pid: u16, mv: i32) -> i32 {
    const P6: [i32; 6] = [100, 50, 30, 20, 5, 0];
    const P21: [i32; 21] = [100, 95, 90, 85, 80, 75, 70, 65, 60, 55, 50, 45, 40, 35, 30, 25, 20, 15, 10, 5, 0];
    match pid {
        0x0A66 => interpolate(mv, &[4200, 3850, 3790, 3750, 3680, 3330], &P6),
        0x0AC4 => interpolate(mv, &[4175, 3817, 3766, 3730, 3664, 3310], &P6),
        0x0A5C | 0x0A89 | 0x0A5B | 0x0A87 | 0x0AFE => interpolate(
            mv,
            &[4100, 3950, 3850, 3750, 3650, 3500, 3300, 3150],
            &[100, 80, 60, 40, 20, 10, 5, 0],
        ),
        0x0AB5 => interpolate(
            mv,
            &[4175, 4127, 4086, 4054, 4001, 3968, 3944, 3914, 3875, 3840, 3818, 3800, 3783, 3771, 3759, 3744, 3725, 3695, 3675, 3630, 3341],
            &P21,
        ),
        0x0AAA | 0x0ABA => interpolate(
            mv,
            &[4102, 4046, 3993, 3950, 3915, 3877, 3838, 3809, 3789, 3772, 3758, 3747, 3736, 3722, 3707, 3687, 3661, 3644, 3548, 3426, 3293],
            &P21,
        ),
        0x0AA7 | 0x0AFB => interpolate(mv, &[4150, 3830, 3780, 3740, 3670, 3320], &P6),
        _ => interpolate(mv, &[4100, 3850, 3780, 3730, 3650, 3300], &P6),
    }
}

fn kind_from_type(t: u8) -> DeviceKind {
    match t {
        0x00 | 0x02 => DeviceKind::Keyboard,
        0x03..=0x05 => DeviceKind::Mouse,
        0x08 => DeviceKind::Headset,
        0x0B | 0x0C => DeviceKind::Gamepad,
        _ => DeviceKind::Other,
    }
}

/// Fills battery fields from whichever battery feature the device has. Returns false if none.
/// The battery features found are remembered for the event listener (see hidpp_events), keyed by
/// `link`, the physical receiver / cable / Bluetooth link.
fn read_battery20(h: &Hidpp, link: &str, idx: u8, pid: u16, s: &mut DeviceStatus) -> bool {
    let apply = |b: hidpp_events::Battery, s: &mut DeviceStatus| {
        s.percent = b.percent;
        if !b.level_text.is_empty() {
            s.level_text = b.level_text.into();
        }
        s.charging = b.charging;
        s.full = b.full;
    };
    let f = h.feature_index(idx, FEATURE_UNIFIED_BATTERY); // get_status (fn 1)
    if f != 0 {
        if let Some(b) = h.call(idx, f, 1, &[]).as_deref().and_then(hidpp_events::unified_battery) {
            hidpp_events::remember_feature(link, idx, FEATURE_UNIFIED_BATTERY, f);
            // Keep what an earlier reading said when this one reports neither a % nor a level.
            if b.percent >= 0 {
                apply(b, s);
            } else {
                s.charging = b.charging;
                s.full = b.full;
            }
            return true;
        }
    }
    let f = h.feature_index(idx, FEATURE_BATTERY_STATUS); // get_level (fn 0)
    if f != 0 {
        if let Some(b) = h.call(idx, f, 0, &[]).as_deref().and_then(hidpp_events::battery_status) {
            hidpp_events::remember_feature(link, idx, FEATURE_BATTERY_STATUS, f);
            apply(b, s);
            return true;
        }
    }
    let f = h.feature_index(idx, 0x1001); // BATTERY_VOLTAGE
    if f != 0 {
        if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| p.len() >= 3) {
            s.millivolts = ((p[0] as i32) << 8) | p[1] as i32;
            s.charging = p[2] & 0x80 != 0;
            if s.millivolts != 0 {
                s.percent = li_ion_percent(s.millivolts);
            }
            return true;
        }
    }
    let f = h.feature_index(idx, 0x1F20); // ADC_MEASUREMENT (headsets)
    if f != 0 {
        if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| p.len() >= 3) {
            s.millivolts = ((p[0] as i32) << 8) | p[1] as i32;
            s.charging = p[2] == 0x03;
            if s.millivolts != 0 {
                s.percent = headset_percent(pid, s.millivolts);
            }
            return true;
        }
    }
    false
}

fn read_name_and_type20(h: &Hidpp, idx: u8, s: &mut DeviceStatus) {
    let f = h.feature_index(idx, 0x0005); // DEVICE_NAME
    if f == 0 {
        return;
    }
    if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| !p.is_empty()) {
        let len = p[0] as usize;
        let mut name = String::new();
        while name.len() < len {
            let Some(p) = h.call(idx, f, 1, &[name.len() as u8]).filter(|p| !p.is_empty()) else { break };
            let before = name.len();
            for &ch in &p {
                if name.len() >= len || ch == 0 {
                    break;
                }
                name.push(ch as char);
            }
            if name.len() == before {
                break;
            }
        }
        if !name.is_empty() {
            s.name = if name.starts_with("Logitech") || name.starts_with("G ") || name.starts_with("MX") {
                name
            } else {
                format!("Logitech {name}")
            };
        }
    }
    if let Some(p) = h.call(idx, f, 2, &[]).filter(|p| !p.is_empty()) {
        s.kind = kind_from_type(p[0]);
    }
}

/// DEVICE_INFORMATION (0x0003) unit id: identifies the physical device across receiver / cable.
fn read_unit_id20(h: &Hidpp, idx: u8) -> String {
    let f = h.feature_index(idx, 0x0003);
    if f == 0 {
        return String::new();
    }
    match h.call(idx, f, 0, &[]) {
        Some(p) if p.len() >= 5 && (p[1] | p[2] | p[3] | p[4]) != 0 => {
            format!("{}{}", hex4(((p[1] as u16) << 8) | p[2] as u16), hex4(((p[3] as u16) << 8) | p[4] as u16))
        }
        _ => String::new(),
    }
}

/// HID++ 1.0 devices: register 0x0D (battery %) or 0x07 (battery level 1..7).
fn read_battery10(h: &Hidpp, idx: u8, s: &mut DeviceStatus) -> bool {
    if let Reply::Ok(p) = h.request(idx, 0x81, 0x0D, &[], true, 500) {
        if p.len() >= 3 && p[0] != 0 && p[0] <= 100 {
            s.percent = p[0] as i32;
            s.charging = (0x21..=0x26).contains(&p[2]);
            return true;
        }
    }
    if let Reply::Ok(p) = h.request(idx, 0x81, 0x07, &[], true, 500) {
        if p.len() >= 2 && p[0] != 0 {
            const PCT: [i32; 8] = [0, 5, 10, 20, 35, 50, 75, 100];
            let lvl = p[0].min(7);
            s.percent = PCT[lvl as usize];
            s.level_text = match lvl {
                7 => "Full",
                5 | 6 => "Good",
                3 | 4 => "Low",
                _ => "Critical",
            }
            .into();
            s.charging = (0x21..=0x26).contains(&p[1]);
            return true;
        }
    }
    false
}

/// WIRELESS_DEVICE_STATUS (0x1D4B): the device announces a reconnection with an event on this
/// feature. Looked up once per device and remembered for the event listener.
fn remember_wireless_status(h: &Hidpp, link: &str, idx: u8) {
    if hidpp_events::known_index(link, idx, FEATURE_WIRELESS_STATUS).is_some() {
        return;
    }
    let f = h.feature_index(idx, FEATURE_WIRELESS_STATUS);
    hidpp_events::remember_feature(link, idx, FEATURE_WIRELESS_STATUS, f);
}

fn physical_key(c: &HidColl) -> String {
    c.physical_key()
}

fn poll_device(ctx: &mut PollContext, pid: u16, shorts: &[HidColl], longs: &[HidColl], suffix: &str) -> bool {
    if shorts.is_empty() && longs.is_empty() {
        return false;
    }
    let Some(h) = Hidpp::open(shorts.first(), longs.first()) else { return false };
    let main = longs.first().or(shorts.first()).unwrap();
    let link = main.physical_key();
    let product = hid::product_string(main);
    let is_receiver = product.contains("Receiver");

    let path = main.path.to_lowercase();
    let bluetooth = path.contains("{00001812-") || path.contains("{00001124-");
    let passthrough = is_passthrough_dongle(pid);
    let ping = |idx: u8| match h.request(idx, 0x00, 0x10 | SW_ID, &[0, 0, 0x5A], false, 400) {
        Reply::Ok(p) if p.len() >= 3 && p[0] >= 2 => Reply::Ok(p),
        r => r,
    };
    let indices: Vec<u8> = if passthrough {
        vec![1]
    } else if bluetooth || (!is_receiver && matches!(ping(0xFF), Reply::Ok(p) if p.len() >= 3 && p[0] >= 2)) {
        vec![0xFF]
    } else {
        vec![1, 2, 3, 4, 5, 6]
    };

    let mut any = false;
    for idx in indices {
        let mut s = DeviceStatus {
            key: format!("logi:{}{}:{}", hex4(pid), suffix, idx),
            name: if idx == 0xFF && !product.is_empty() { product.clone() } else { "Logitech wireless device".into() },
            via: if idx == 0xFF { "USB / Bluetooth".into() } else { "Logitech receiver".into() },
            grace_sec: if idx == 0xFF { 0 } else { 1800 },
            ..Default::default()
        };
        s.kind = if passthrough { DeviceKind::Mouse } else { guess_kind(&s.name) };

        let mut found = false;
        match ping(idx) {
            Reply::Ok(p) if p.len() >= 3 && p[0] >= 2 => {
                read_name_and_type20(&h, idx, &mut s);
                found = read_battery20(&h, &link, idx, pid, &mut s);
                if found {
                    remember_wireless_status(&h, &link, idx);
                }
                let unit = if found { read_unit_id20(&h, idx) } else { String::new() };
                if !unit.is_empty() {
                    s.key = format!("logi:unit:{unit}");
                    if idx == 0xFF {
                        s.grace_sec = 60;
                    }
                }
                if found && s.kind == DeviceKind::Other && longs.first().is_some_and(|l| l.page() == 0xFF43) {
                    s.kind = DeviceKind::Headset;
                }
            }
            Reply::Err(0x01) => found = read_battery10(&h, idx, &mut s), // HID++ 1.0 device
            _ => {}
        }
        if found {
            if s.kind == DeviceKind::Other {
                s.kind = guess_kind(&s.name);
            }
            s.vid = 0x046D;
            s.hw = Some((0x046D, pid));
            ctx.out.push(s);
            any = true;
        }
    }
    any
}

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x046D) {
        let passthrough = is_passthrough_dongle(pid);
        let all_shorts = ctx.find(vid, pid, |c| {
            c.out_len() == 7
                && ((c.page() == 0xFF00 && c.usage() == 0x0001) || (passthrough && c.page() == 0xFF43 && c.usage() == 0x0301))
        });
        let all_longs = ctx.find(vid, pid, |c| {
            c.out_len() == 20
                && ((c.page() == 0xFF00 && c.usage() == 0x0002)
                    || (c.page() == 0xFF43 && (c.usage() == 0x0202 || (passthrough && c.usage() == 0x0302))))
        });
        let mut groups: Vec<String> = Vec::new();
        for c in all_shorts.iter().chain(all_longs.iter()) {
            let k = physical_key(c);
            if !groups.contains(&k) {
                groups.push(k);
            }
        }
        let mut any = false;
        for (g, key) in groups.iter().enumerate() {
            let shorts: Vec<HidColl> = all_shorts.iter().filter(|c| &physical_key(c) == key).cloned().collect();
            let longs: Vec<HidColl> = all_longs.iter().filter(|c| &physical_key(c) == key).cloned().collect();
            let suffix = if g > 0 { format!("#{}", g + 1) } else { String::new() };
            any |= poll_device(ctx, pid, &shorts, &longs, &suffix);
        }
        if any {
            ctx.claim(vid, pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn li_ion_curve_end_points_and_order() {
        assert_eq!(li_ion_percent(4300), 100);
        assert_eq!(li_ion_percent(3400), 0);
        assert_eq!(li_ion_percent(3811), 50);
        let mut last = 101;
        for mv in (3400..4300).rev().step_by(25) {
            let p = li_ion_percent(mv);
            assert!(p <= last, "curve must not rise as the voltage falls");
            last = p;
        }
    }

    #[test]
    fn headset_curves() {
        assert_eq!(headset_percent(0x0AB5, 4200), 100); // G733
        assert_eq!(headset_percent(0x0AB5, 3300), 0);
        assert_eq!(headset_percent(0x0A66, 3850), 50); // G533
    }

    #[test]
    fn receiver_collections_group_by_physical_device() {
        let a = HidColl { path: r"\\?\hid#vid_046d&pid_c547&mi_02&col01#8&2a2b3c4d&0&0000#{guid}".into(), ..Default::default() };
        let b = HidColl { path: r"\\?\hid#vid_046d&pid_c547&mi_02&col02#8&2a2b3c4d&0&0001#{guid}".into(), ..Default::default() };
        let other = HidColl { path: r"\\?\hid#vid_046d&pid_c547&mi_02&col01#8&99999999&0&0000#{guid}".into(), ..Default::default() };
        assert_eq!(physical_key(&a), physical_key(&b));
        assert_ne!(physical_key(&a), physical_key(&other));
    }
}
