use parking_lot::Mutex;
use std::time::Duration;

pub use web_time::{Instant, SystemTime};

pub trait Clock {
    fn utc_now(&self) -> crate::fast::clock::UtcTime;
    fn now(&self) -> Instant;
}

pub struct TestClock(Mutex<TestClockState>);

struct TestClockState {
    now: Instant,
    utc_now: crate::fast::clock::UtcTime,
}

impl TestClock {
    pub fn new() -> Self {
        let utc_now = crate::fast::clock::test_clock_start();
        Self(Mutex::new(TestClockState {
            now: Instant::now(),
            utc_now,
        }))
    }

    pub fn set_utc_now(&self, now: crate::fast::clock::UtcTime) {
        let mut state = self.0.lock();
        state.utc_now = now;
    }

    pub fn advance(&self, duration: Duration) {
        let mut state = self.0.lock();
        state.now += duration;
        state.utc_now += duration;
    }
}

impl Clock for TestClock {
    fn utc_now(&self) -> crate::fast::clock::UtcTime {
        self.0.lock().utc_now
    }

    fn now(&self) -> Instant {
        self.0.lock().now
    }
}
