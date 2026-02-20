//! Platform-agnostic time source
//!
//! Provides a [`Clock`] trait that abstracts `SystemTime::now()` for
//! WASM compatibility. Native code uses [`SystemClock`] (the default);
//! WASM hosts inject a `JsClock` backed by `Date.now()`.

use std::fmt;
use std::time::SystemTime;

/// Platform-agnostic time source.
///
/// Implementors must be `Send + Sync` so they can be shared across
/// async tasks on native targets. On `wasm32` this will be relaxed
/// to `MaybeSend + MaybeSync` in a future change.
pub trait Clock: fmt::Debug + Send + Sync {
    /// Returns the current system time.
    fn now(&self) -> SystemTime;
}

/// Default clock that delegates to [`SystemTime::now()`].
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::module_name_repetitions)]
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn system_clock_returns_recent_time() {
        let clock = SystemClock;
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
