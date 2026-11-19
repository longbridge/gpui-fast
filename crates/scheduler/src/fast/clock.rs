//! `Clock`'s wall-clock time is a `SystemTime`, not chrono's `DateTime<Utc>`.
//!
//! RUSTSEC-2026-0543 marks chrono as unmaintained, and the scheduler used it
//! only for this. `SystemTime` comes from `web_time`, as `Instant` does, so
//! it reads the browser's clock on wasm, which is what chrono's `wasmbind`
//! feature did. Upstream makes the same change in zed-industries/zed#67089;
//! the sync that brings it replaces the hooks naming this module, and this
//! file goes.

use std::time::Duration;

use web_time::SystemTime;

/// What `Clock::utc_now` returns.
pub(crate) type UtcTime = SystemTime;

/// Where a `TestClock`'s wall clock starts: 2025-07-01T23:59:58Z, as upstream.
pub(crate) fn test_clock_start() -> UtcTime {
    const START_TIME_UNIX_SECS: u64 = 1_751_414_398;
    SystemTime::UNIX_EPOCH + Duration::from_secs(START_TIME_UNIX_SECS)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use web_time::SystemTime;

    use crate::{Clock, TestClock};

    /// A test clock starts at upstream's fixed time, `advance` moves its wall
    /// clock with its monotonic one, and `set_utc_now` sets the wall clock.
    #[test]
    fn wall_clock_starts_at_a_fixed_time_advances_and_can_be_set() {
        let clock = TestClock::new();
        let start = clock.utc_now();
        assert_eq!(
            start.duration_since(SystemTime::UNIX_EPOCH).unwrap(),
            Duration::from_secs(1_751_414_398)
        );

        let instant_start = clock.now();
        clock.advance(Duration::from_millis(1500));
        assert_eq!(
            clock.utc_now().duration_since(start).unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(clock.now() - instant_start, Duration::from_millis(1500));

        let later = start + Duration::from_secs(3600);
        clock.set_utc_now(later);
        assert_eq!(clock.utc_now(), later);
    }
}
