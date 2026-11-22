//! BatteryHub (Rust): a recode of github.com/krisgfx9/Wireless-Device-Battery-Monitor (C++, GPL-3.0), rewritten in Rust.
//! Shared core used by both editions: device model, Win32 HID layer and the drivers.
//!
//! The program is Windows only. The refresh scheduling and the push-event decoders are plain
//! Rust, so their tests also run on other platforms (`cargo test --lib`).

pub mod drivers;
pub mod hid_coll;
pub mod refresh;

#[cfg(windows)]
pub mod ble;
#[cfg(windows)]
pub mod device;
#[cfg(windows)]
pub mod hid;
#[cfg(windows)]
pub mod hid_listener;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod report;
#[cfg(windows)]
pub mod report_window;
#[cfg(windows)]
pub mod settings;
#[cfg(windows)]
pub mod ui;
#[cfg(windows)]
pub mod update;
#[cfg(windows)]
pub mod util;

#[cfg(windows)]
pub use device::{poll_all_devices, DeviceKind, DeviceStatus};
