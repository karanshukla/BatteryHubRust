//! Emitter for push-capable HID devices: holds the collections the decoders want open, read-only,
//! with a read always pending, and asks for a refresh when a decoder recognises an event (for
//! example a mouse switched on behind a receiver that stays plugged in, which Windows does not
//! report). Vendor-neutral: all protocol knowledge is in the `HidEventDecoder` implementations.
//!
//! Each open handle gets its own copy of every input report, so listening takes nothing away from
//! the poller or from vendor software. Nothing is ever written to a device.
//!
//! Ported from hid_listener.cpp of the C++ BatteryHub (PR #535 by Karan Shukla, GPL-3.0).
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::drivers::event_decoders::{HidEventDecoder, SignalEdges};
use crate::hid::{self, HidColl};
use crate::refresh::{RefreshReason, RefreshSink};
use crate::util::hex4;

/// Receives every report read and what was made of it (for the `events` diagnostic).
pub type Log = Box<dyn Fn(&str) + Send>;

const MAXIMUM_WAIT_OBJECTS: usize = 64;

/// A Win32 event usable from both threads.
struct Event(HANDLE);
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    /// `manual`: stays signalled until reset (stop); otherwise auto-reset (rescan: a burst of
    /// arrivals is one reopen).
    fn new(manual: bool) -> Option<Self> {
        unsafe { CreateEventW(None, manual, false, PCWSTR::null()).ok().map(Event) }
    }
    fn set(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct HidEventListener {
    stop: Arc<Event>,
    rescan: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl HidEventListener {
    /// Starts listening on a thread of its own. None if Windows refused the events.
    pub fn start(decoders: Vec<Box<dyn HidEventDecoder>>, sink: Arc<dyn RefreshSink>, log: Option<Log>) -> Option<Self> {
        let stop = Arc::new(Event::new(true)?);
        let rescan = Arc::new(Event::new(false)?);
        let mut engine = Engine { decoders, edges: SignalEdges::default(), sink, log, readers: Vec::new() };
        let (s, r) = (stop.clone(), rescan.clone());
        let thread = std::thread::Builder::new()
            .name("hid-events".into())
            .stack_size(64 * 1024)
            .spawn(move || engine.run(&s, &r))
            .ok()?;
        Some(Self { stop, rescan, thread: Some(thread) })
    }

    /// The set of HID devices changed (arrival, removal, resume): reopen whatever is held.
    pub fn devices_changed(&self) {
        self.rescan.set();
    }

    pub fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.stop.set();
            let _ = t.join();
        }
    }
}

impl Drop for HidEventListener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One collection held open with a read always pending on it. Boxed, so the OVERLAPPED and the
/// buffer do not move while a read is in flight.
struct Reader {
    coll: HidColl,
    decoder: usize,
    h: HANDLE,
    ev: HANDLE,
    ov: OVERLAPPED,
    buf: Vec<u8>,
}

// Plain kernel handles. Readers are created, used and dropped only on the listener thread (the
// engine moves there before it opens any).
unsafe impl Send for Reader {}

impl Reader {
    fn open(c: &HidColl, decoder: usize) -> Option<Box<Self>> {
        let h = hid::open_raw(&c.path, GENERIC_READ.0, true)?;
        let Ok(ev) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else {
            unsafe {
                let _ = CloseHandle(h);
            }
            return None;
        };
        let mut r = Box::new(Reader { coll: c.clone(), decoder, h, ev, ov: OVERLAPPED::default(), buf: vec![0; c.in_len() as usize] });
        r.arm().then_some(r)
    }

    /// Queues the next read. A read that completes at once still signals the event.
    fn arm(&mut self) -> bool {
        self.ov = OVERLAPPED { hEvent: self.ev, ..Default::default() };
        unsafe {
            let _ = ResetEvent(self.ev);
            ReadFile(self.h, Some(&mut self.buf), None, Some(&mut self.ov)).is_ok() || GetLastError() == ERROR_IO_PENDING
        }
    }

    /// Bytes of the completed read, None when it failed (the device went away).
    fn completed(&mut self) -> Option<usize> {
        let mut n = 0u32;
        unsafe { GetOverlappedResult(self.h, &self.ov, &mut n, false) }.ok().map(|_| n as usize)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            let mut n = 0u32;
            let _ = CancelIoEx(self.h, Some(&self.ov));
            let _ = GetOverlappedResult(self.h, &self.ov, &mut n, true); // the buffer must outlive the cancelled read
            let _ = CloseHandle(self.h);
            let _ = CloseHandle(self.ev);
        }
    }
}

/// The listener thread's state.
struct Engine {
    decoders: Vec<Box<dyn HidEventDecoder>>,
    /// Survives reopening, so a reopened device's first repeat is not news.
    edges: SignalEdges,
    sink: Arc<dyn RefreshSink>,
    log: Option<Log>,
    // Boxed on purpose: a pending read holds the address of its OVERLAPPED and buffer, so a
    // reader must not move when the vector grows or another reader is removed.
    #[allow(clippy::vec_box)]
    readers: Vec<Box<Reader>>,
}

impl Engine {
    fn run(&mut self, stop: &Event, rescan: &Event) {
        self.reopen();
        loop {
            let mut hs = vec![stop.0, rescan.0];
            hs.extend(self.readers.iter().map(|r| r.ev));
            let w = unsafe { WaitForMultipleObjects(&hs, false, INFINITE) };
            let i = w.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            match i {
                0 => break,
                1 => {
                    self.reopen();
                    continue;
                }
                _ if i - 2 >= self.readers.len() => break, // WAIT_FAILED: should not happen, do not spin
                _ => {}
            }
            let i = i - 2;
            let read = self.readers[i].completed();
            if let Some(n) = read.filter(|&n| n > 0) {
                self.on_report(i, n);
            }
            // A failed read means the device went away; drop it rather than spin on the error.
            // devices_changed() reopens whatever is still there.
            if read.is_none() || !self.readers[i].arm() {
                self.readers.remove(i);
            }
        }
        self.readers.clear();
    }

    fn reopen(&mut self) {
        self.readers.clear();
        for c in hid::enum_hid() {
            let Some(d) = self.decoders.iter().position(|d| d.channel().matches(&c)) else { continue };
            if self.readers.len() >= MAXIMUM_WAIT_OBJECTS - 2 {
                return;
            }
            if let Some(r) = Reader::open(&c, d) {
                if let Some(log) = &self.log {
                    log(&format!("listening: {}  {}:{}  {}:{}", self.decoders[d].name(), hex4(c.vid), hex4(c.pid), hex4(c.page()), hex4(c.usage())));
                }
                self.readers.push(r);
            }
        }
    }

    fn on_report(&mut self, i: usize, n: usize) {
        let r = &self.readers[i];
        let frame = &r.buf[..n.min(r.buf.len())];
        let d = &self.decoders[r.decoder];
        let s = if d.channel().accepts(frame) { d.decode(&r.coll, frame) } else { None };
        let change = s.as_ref().is_some_and(|s| self.edges.is_change(&r.coll.path, s));
        if let Some(log) = &self.log {
            let hex: Vec<String> = frame.iter().map(|b| format!("{b:02X}")).collect();
            let what = match &s {
                Some(s) => format!("  -> {}{}", s.what, if change { "" } else { " (repeat, ignored)" }),
                None => String::new(),
            };
            log(&format!("{}  {}{what}", hex4(r.coll.pid), hex.join(" ")));
        }
        if change {
            self.sink.request_refresh(RefreshReason::DeviceEvent, d.settle_ms());
        }
    }
}
