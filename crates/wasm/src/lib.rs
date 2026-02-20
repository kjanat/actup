//! WASM bindings for actup-core
//!
//! Provides [`parseUsesRefs`] to extract `uses:` references from GitHub
//! Actions workflow YAML. Runs entirely in-browser or Node.js — no
//! filesystem access, no network.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clock;

use actup_core::extract_uses_from_yaml;
use wasm_bindgen::prelude::*;

/// Parse `uses:` references from workflow YAML content.
///
/// Returns a JS array of objects with `line`, `column`, `value`, and
/// `raw_line` fields (matching [`actup_core::UsesLocation`] serialization).
///
/// This is a pure, synchronous function — no network or filesystem access.
///
/// # Errors
///
/// Returns `JsValue` error if serialization to JS fails (should not happen
/// for valid input).
#[wasm_bindgen(js_name = "parseUsesRefs")]
pub fn parse_uses_refs(yaml: &str) -> Result<JsValue, JsValue> {
    let locations = extract_uses_from_yaml(yaml);
    serde_wasm_bindgen::to_value(&locations).map_err(|e| JsValue::from_str(&e.to_string()))
}
