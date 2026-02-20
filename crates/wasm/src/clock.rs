//! WASM clock implementation backed by `Date.now()`.

use actup_core::Clock;
use std::time::{Duration, SystemTime};

/// Clock implementation for WASM environments.
///
/// Uses [`js_sys::Date::now()`] which returns milliseconds since the
/// Unix epoch, then converts to [`SystemTime`] for compatibility with
/// `actup-core`'s `Clock` trait.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::module_name_repetitions)]
pub struct JsClock;

impl Clock for JsClock {
    #[inline]
    fn now(&self) -> SystemTime {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let millis = js_sys::Date::now() as u64;
        SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
    }
}
