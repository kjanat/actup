//! WASM clock implementation backed by `Date.now()`.

use std::time::{Duration, SystemTime};

use actup_core::Clock;

/// Clock implementation for WASM environments.
///
/// Uses [`js_sys::Date::now()`] (ms since Unix epoch as `f64`) and converts
/// via [`Duration::try_from_secs_f64`] — avoiding lossy `as` casts while
/// gracefully handling NaN, negatives, and out-of-range values by falling
/// back to the Unix epoch.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsClock;

impl Clock for JsClock {
    #[inline]
    fn now(&self) -> SystemTime {
        let secs = js_sys::Date::now() / 1000.0;
        Duration::try_from_secs_f64(secs)
            .map_or(SystemTime::UNIX_EPOCH, |d| SystemTime::UNIX_EPOCH + d)
    }
}
