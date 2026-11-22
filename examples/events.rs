//! Console diagnostic: prints the push events the HID event listener sees, so a contributor can
//! check that a device sends them before relying on them.
//!
//!   cargo run --example events -- [seconds]     (default 60)
//!
//! Reads every device once first, so the Logitech HID++ 2.0 feature indexes (battery, wireless
//! status) are known, and reads (never writes) each receiver's notification flags.
use std::sync::Arc;

use batteryhub::drivers::event_decoders::hid_event_decoders;
use batteryhub::drivers::hidpp_events as hidpp;
use batteryhub::hid::{self, HidDev};
use batteryhub::hid_listener::HidEventListener;
use batteryhub::refresh::{RefreshReason, RefreshSink};
use batteryhub::util::{hex4, Deadline};

/// The listener the app runs, with a sink that prints instead of polling.
struct ConsoleSink;

impl RefreshSink for ConsoleSink {
    fn request_refresh(&self, why: RefreshReason, delay_ms: u32) {
        println!("  => refresh requested ({}, in {delay_ms} ms)", why.name());
    }
}

fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).filter(|&s| s > 0).unwrap_or(60);

    for d in batteryhub::poll_all_devices() {
        println!("found: {} ({}) {}", d.name, d.via, d.status_text());
    }

    // Logitech receivers send connection events (0x41) only while their "wireless" notification
    // flag is set. HID++ 2.0 feature events come from the devices themselves and do not need it.
    for c in hid::enum_hid() {
        if c.vid != 0x046D || c.page() != 0xFF00 || c.usage() != 0x0001 || c.out_len() != 7 {
            continue;
        }
        let Some(d) = HidDev::open(&c) else { continue };
        d.flush();
        let mut state = "no answer (not a receiver?)".to_string();
        if d.write(&hidpp::notification_flags_request()) {
            let end = Deadline::in_ms(500);
            while !end.passed() {
                let Some(flags) = d.read(50).and_then(|r| hidpp::parse_notification_flags(&r)) else { continue };
                state = if flags & hidpp::WIRELESS_NOTIFICATIONS != 0 {
                    "on".into()
                } else {
                    "OFF (receiver connection events need G HUB, Options+ or Solaar to switch them on)".into()
                };
                break;
            }
        }
        println!("receiver {}:{} wireless notifications: {state}", hex4(c.vid), hex4(c.pid));
    }

    let log: batteryhub::hid_listener::Log = Box::new(|m: &str| println!("{m}"));
    let Some(mut listener) = HidEventListener::start(hid_event_decoders(), Arc::new(ConsoleSink), Some(log)) else {
        eprintln!("could not start the listener");
        std::process::exit(1);
    };
    println!("listening for {seconds} s: switch a wireless device off and on...");
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    listener.stop();
}
