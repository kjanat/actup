//! Platform-agnostic time source
//!
//! Provides a [`Clock`] trait that abstracts `SystemTime::now()` for
//! WASM compatibility. Native code uses [`System`] (the default);
//! WASM hosts inject a `JsClock` backed by `Date.now()`.

use std::{fmt, time::SystemTime};

use crate::compat::{MaybeSend, MaybeSync};

/// Platform-agnostic time source.
///
/// On native targets this requires `Send + Sync` (via [`MaybeSend`] +
/// [`MaybeSync`]) so clocks can be shared across async tasks. On
/// `wasm32` (single-threaded) these bounds are automatically satisfied.
pub trait Clock: fmt::Debug + MaybeSend + MaybeSync {
    /// Returns the current system time.
    fn now(&self) -> SystemTime;
}

/// Default clock that delegates to [`SystemTime::now()`].
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Clock for System {
    #[inline]
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock(std::sync::Mutex<SystemTime>);

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(std::sync::Mutex::new(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
        )))
    }

    pub(crate) fn advance(&self, by: std::time::Duration) {
        self.set(|now| now.checked_add(by));
    }

    pub(crate) fn rewind(&self, by: std::time::Duration) {
        self.set(|now| now.checked_sub(by));
    }

    fn set(&self, to: impl FnOnce(SystemTime) -> Option<SystemTime>) {
        let mut now = match self.0.lock() {
            Ok(now) => now,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(moved) = to(*now) {
            *now = moved;
        }
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        match self.0.lock() {
            Ok(now) => *now,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn system_clock_returns_recent_time() {
        let clock = System;
        let now = clock.now();
        let elapsed = now.elapsed().unwrap();
        assert!(elapsed < Duration::from_secs(1));
    }

    /// Mock clock for testing — returns a fixed point in time.
    #[derive(Debug)]
    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    #[test]
    fn fixed_clock_returns_injected_time() {
        let epoch = SystemTime::UNIX_EPOCH;
        let clock = FixedClock(epoch);
        assert_eq!(clock.now(), epoch);
    }

    #[test]
    fn cache_entry_with_fixed_clock() {
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let clock = FixedClock(past);
        let entry = crate::cache::Entry::with_clock(crate::version::Set::default(), &clock);
        assert_eq!(entry.stored_at(), past);
    }

    #[test]
    fn version_set_build_with_fixed_clock() {
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let clock = FixedClock(past);
        let set = crate::version::Set::build_with_clock(
            Vec::new(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            None,
            &clock,
        );
        assert_eq!(set.fetched_at(), Some(past));
    }
}
