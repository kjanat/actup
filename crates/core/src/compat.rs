//! Platform compatibility aliases for `Send`/`Sync` bounds and boxed futures
//!
//! On native targets, [`MaybeSend`] and [`MaybeSync`] are equivalent to
//! `Send` and `Sync` respectively. On `wasm32` (which is single-threaded),
//! they are automatically implemented for all types.
//!
//! [`BoxFuture`] is a type alias for pinned, boxed futures that are `Send`
//! on native and `!Send` on `wasm32`. Use it in trait methods that return
//! `dyn Future` — Rust only allows auto traits (like `Send`) in trait
//! object position, so a `dyn Future + MaybeSend` bound would not compile.
//!
//! This follows the established pattern used by `reqwest-middleware`, `iced`,
//! `opendal`, and many other ecosystem crates.

use std::future::Future;
use std::pin::Pin;

/// Marker trait that is `Send` on native targets, unconditional on `wasm32`.
///
/// Use this instead of bare `Send` bounds on traits and generics that need
/// to compile for WASM. On native targets this is identical to `Send`;
/// on `wasm32` every type satisfies this bound.
///
/// **Important:** This cannot be used in trait-object position
/// (`dyn Future + MaybeSend` is illegal). For boxed futures, use
/// [`BoxFuture`] instead.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> MaybeSend for T {}

/// Marker trait: unconditional on `wasm32` (single-threaded).
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSend for T {}

/// Marker trait that is `Sync` on native targets, unconditional on `wasm32`.
///
/// See [`MaybeSend`] for usage notes.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync> MaybeSync for T {}

/// Marker trait: unconditional on `wasm32` (single-threaded).
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSync for T {}

/// A pinned, boxed future that is `Send` on native targets.
///
/// On native: `Pin<Box<dyn Future<Output = T> + Send + 'a>>`
/// On wasm32: `Pin<Box<dyn Future<Output = T> + 'a>>`
///
/// Use this as the return type for trait methods that return `dyn Future`.
/// Rust does not allow non-auto traits in trait-object position, so
/// `dyn Future + MaybeSend` is a compile error — this type alias is the
/// established workaround.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Boxed future without `Send` bound (wasm32 is single-threaded).
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
