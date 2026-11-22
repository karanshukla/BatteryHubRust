//! Device drivers. Each looks at the enumerated HID collections (or Windows' Bluetooth / gaming
//! APIs), talks to the devices it recognises and appends what it finds to the PollContext.
#[cfg(windows)]
pub mod airpods;
#[cfg(windows)]
pub mod bluetooth;
#[cfg(windows)]
pub mod generic;
#[cfg(windows)]
pub mod logitech;
#[cfg(windows)]
pub mod playstation;
#[cfg(windows)]
pub mod razer;
#[cfg(windows)]
pub mod roccat;
#[cfg(windows)]
pub mod sdi;
#[cfg(windows)]
pub mod steelseries;
#[cfg(windows)]
pub mod xinput;

// Push events: plain Rust, tested on any platform.
pub mod event_decoders;
pub mod hidpp_events;
pub mod logitech_events;
