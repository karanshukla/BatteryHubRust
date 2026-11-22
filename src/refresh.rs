//! When to poll, and why. Free of Win32 so the coalescing rules can be unit tested.
//!
//! ```text
//!   emitters                              collector                    consumer
//!   main window (USB, resume, menu) --.
//!   HidEventListener (HID++ events) --+--> RefreshSink --> RefreshScheduler --> main window --> poll
//!   main window (after each poll)   --'                    (RefreshPlan inside)   (one timer)
//! ```
//!
//! Emitters only know [`RefreshSink`]. The plan is a conflating mailbox: each reason keeps one
//! slot, so twenty USB arrivals in a burst become one poll, and one poll answers every pending
//! reason. Requests that arrive while a poll runs stay pending and are served right after it.
//!
//! Ported from the event-driven refresh of the C++ BatteryHub (refresh.h, refresh_scheduler.cpp,
//! krisgfx9/Wireless-Device-Battery-Monitor PR #535 by Karan Shukla, GPL-3.0).
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshReason {
    Startup,
    Timer,
    Manual,
    DeviceChange,
    Resume,
    DeviceEvent,
}

impl RefreshReason {
    pub const ALL: [RefreshReason; 6] = [
        RefreshReason::Startup,
        RefreshReason::Timer,
        RefreshReason::Manual,
        RefreshReason::DeviceChange,
        RefreshReason::Resume,
        RefreshReason::DeviceEvent,
    ];

    pub fn name(self) -> &'static str {
        match self {
            RefreshReason::Startup => "startup",
            RefreshReason::Timer => "timer",
            RefreshReason::Manual => "manual",
            RefreshReason::DeviceChange => "device change",
            RefreshReason::Resume => "resume",
            RefreshReason::DeviceEvent => "device event",
        }
    }

    /// The coalescing rule of each reason lives here and nowhere else:
    /// `Some(max_defer_ms)` = trailing debounce (wait for the burst to end, but never past the
    /// first request + max_defer_ms); `None` = the earliest request wins.
    fn debounce(self) -> Option<u64> {
        match self {
            RefreshReason::Startup => None,                // as soon as possible
            RefreshReason::Timer => None,                  // re-armed after every poll
            RefreshReason::Manual => None,                 // the user is waiting
            RefreshReason::DeviceChange => Some(10_000),   // USB enumeration comes in bursts
            RefreshReason::Resume => Some(10_000),         // devices reappear one by one
            RefreshReason::DeviceEvent => Some(10_000),    // a receiver reports each of its devices
        }
    }

    fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// A set of reasons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshReasons(u32);

impl RefreshReasons {
    pub fn add(&mut self, r: RefreshReason) {
        self.0 |= r.bit();
    }
    pub fn has(self, r: RefreshReason) -> bool {
        self.0 & r.bit() != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn names(self) -> Vec<&'static str> {
        RefreshReason::ALL.iter().filter(|r| self.has(**r)).map(|r| r.name()).collect()
    }
}

/// Anything that wants the devices polled. Safe to call from any thread.
pub trait RefreshSink: Send + Sync {
    fn request_refresh(&self, why: RefreshReason, delay_ms: u32);
}

#[derive(Clone, Copy, Default)]
struct Slot {
    pending: bool,
    due: u64,
    first: u64,
}

/// The pending refreshes, on a millisecond clock the caller provides.
#[derive(Default)]
pub struct RefreshPlan {
    slots: [Slot; RefreshReason::ALL.len()],
}

impl RefreshPlan {
    pub fn add(&mut self, why: RefreshReason, now: u64, delay_ms: u32) {
        let s = &mut self.slots[why as usize];
        let want = now + delay_ms as u64;
        if !s.pending {
            *s = Slot { pending: true, due: want, first: now };
        } else if let Some(max_defer) = why.debounce() {
            // Trailing debounce, capped so a device that keeps flapping cannot postpone the poll forever.
            s.due = want.min(s.first + max_defer);
        } else {
            s.due = s.due.min(want);
        }
    }

    /// Milliseconds until the next poll is due, 0 if overdue, None if nothing is pending.
    pub fn wait_ms(&self, now: u64) -> Option<u64> {
        self.earliest_due().map(|due| due.saturating_sub(now))
    }

    /// When a poll is due, returns every pending reason and clears them all (one poll serves them
    /// all). Returns an empty set otherwise.
    pub fn take_due(&mut self, now: u64) -> RefreshReasons {
        let mut out = RefreshReasons::default();
        if self.earliest_due().is_none_or(|due| due > now) {
            return out;
        }
        for r in RefreshReason::ALL {
            let s = &mut self.slots[r as usize];
            if s.pending {
                out.add(r);
                *s = Slot::default();
            }
        }
        out
    }

    fn earliest_due(&self) -> Option<u64> {
        self.slots.iter().filter(|s| s.pending).map(|s| s.due).min()
    }
}

/// The collector: a thread-safe [`RefreshSink`] in front of [`RefreshPlan`]. Every request calls
/// `wake` (the app posts a message to its window), and the consumer then asks [`Self::wait_ms`]
/// when to look again and [`Self::take_due`] what is due.
pub struct RefreshScheduler {
    plan: Mutex<RefreshPlan>,
    epoch: Instant,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl RefreshScheduler {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self { plan: Mutex::new(RefreshPlan::default()), epoch: Instant::now(), wake: Box::new(wake) }
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn plan(&self) -> std::sync::MutexGuard<'_, RefreshPlan> {
        self.plan.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn wait_ms(&self) -> Option<u64> {
        let now = self.now();
        self.plan().wait_ms(now)
    }

    pub fn take_due(&self) -> RefreshReasons {
        let now = self.now();
        self.plan().take_due(now)
    }
}

impl RefreshSink for RefreshScheduler {
    fn request_refresh(&self, why: RefreshReason, delay_ms: u32) {
        let now = self.now();
        self.plan().add(why, now, delay_ms);
        (self.wake)(); // a new request may have moved the deadline earlier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use RefreshReason::*;

    #[test]
    fn empty_plan_never_wakes() {
        let mut p = RefreshPlan::default();
        assert_eq!(p.wait_ms(0), None);
        assert!(p.take_due(1_000_000).is_empty());
    }

    #[test]
    fn earliest_wins_for_non_debounced_reasons() {
        let mut p = RefreshPlan::default();
        p.add(Timer, 0, 60_000);
        p.add(Timer, 1_000, 5_000); // sooner: moves the deadline earlier
        assert_eq!(p.wait_ms(1_000), Some(5_000));
        p.add(Timer, 2_000, 60_000); // later: ignored
        assert_eq!(p.wait_ms(2_000), Some(4_000));
    }

    #[test]
    fn debounce_waits_for_the_burst_to_end() {
        let mut p = RefreshPlan::default();
        p.add(DeviceChange, 0, 2_500);
        p.add(DeviceChange, 2_000, 2_500);
        assert!(p.take_due(2_600).is_empty(), "the second arrival pushed the poll back");
        assert_eq!(p.wait_ms(2_600), Some(1_900));
        assert!(p.take_due(4_500).has(DeviceChange));
    }

    #[test]
    fn debounce_is_capped_so_flapping_cannot_starve_the_poll() {
        let mut p = RefreshPlan::default();
        for t in (0..20_000).step_by(1_000) {
            p.add(DeviceEvent, t, 1_500);
            if !p.take_due(t).is_empty() {
                assert!(t >= 10_000, "polled before the cap");
                return;
            }
        }
        assert!(p.take_due(10_000).has(DeviceEvent));
    }

    #[test]
    fn one_poll_serves_every_pending_reason() {
        let mut p = RefreshPlan::default();
        p.add(Timer, 0, 60_000);
        p.add(DeviceEvent, 0, 1_500);
        p.add(Manual, 100, 0);
        let due = p.take_due(100);
        assert!(due.has(Manual) && due.has(DeviceEvent) && due.has(Timer));
        assert_eq!(p.wait_ms(100), None, "everything was answered");
    }

    #[test]
    fn nothing_due_takes_nothing() {
        let mut p = RefreshPlan::default();
        p.add(Startup, 0, 3_000);
        assert!(p.take_due(2_999).is_empty());
        assert_eq!(p.wait_ms(2_999), Some(1));
        assert_eq!(p.wait_ms(5_000), Some(0), "overdue");
    }

    #[test]
    fn names() {
        let mut r = RefreshReasons::default();
        r.add(DeviceEvent);
        r.add(Startup);
        assert_eq!(r.names(), ["startup", "device event"]);
    }

    #[test]
    fn scheduler_wakes_the_consumer_on_every_request() {
        let woken = Arc::new(AtomicU32::new(0));
        let w = woken.clone();
        let s = RefreshScheduler::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(s.wait_ms(), None);
        s.request_refresh(Manual, 0);
        s.request_refresh(Timer, 60_000);
        assert_eq!(woken.load(Ordering::Relaxed), 2);
        let due = s.take_due();
        assert!(due.has(Manual) && due.has(Timer));
        assert_eq!(s.wait_ms(), None);
    }

    #[test]
    fn scheduler_is_a_shareable_sink() {
        let s: Arc<dyn RefreshSink> = Arc::new(RefreshScheduler::new(|| {}));
        let t = {
            let s = s.clone();
            std::thread::spawn(move || s.request_refresh(DeviceEvent, 1_500))
        };
        t.join().unwrap();
    }
}
