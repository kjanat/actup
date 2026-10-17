//! WASM bindings for actup-core
//!
//! Provides:
//! - `parseUsesRefs` — pure, synchronous extraction of `uses:` references
//! - `checkRefs` — async version checking via a host-provided JS provider
//! - `planUpdates` — compute text edits from check results
//! - `sortVersionTags`: sort version tags newest first
//!
//! Runs entirely in-browser or Node.js — no filesystem access.
//!
//! This crate is wasm32-only. On native targets it compiles as an empty
//! crate so that `cargo check` in the workspace succeeds without errors.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(target_arch = "wasm32")]
pub mod bridge;
#[cfg(target_arch = "wasm32")]
mod clock;
#[cfg(target_arch = "wasm32")]
mod wasm_api;
