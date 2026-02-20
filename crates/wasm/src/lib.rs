//! WASM bindings for actup-core
//!
//! Provides:
//! - `parseUsesRefs` — pure, synchronous extraction of `uses:` references
//! - `checkRefs` — async version checking via a host-provided JS provider
//! - `planUpdates` — compute text edits from check results
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
pub mod clock;

#[cfg(target_arch = "wasm32")]
mod wasm_api {
    use actup_core::config::Config;
    use actup_core::provider::{Kind, Registry};
    use actup_core::resolution::{CheckResult, CheckStatus, check_action_remote};
    use actup_core::{extract_uses_from_yaml, parse_uses};
    use serde::Serialize;
    use wasm_bindgen::prelude::*;

    use crate::bridge::{JsProvider, JsProviderAdapter};

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

    /// Check `uses:` references in YAML against a host-provided JS provider.
    ///
    /// Parses the YAML, resolves each non-local `uses:` ref through the
    /// provider, and returns an array of [`CheckResult`] objects.
    ///
    /// # Arguments
    ///
    /// * `yaml` — workflow YAML content
    /// * `provider` — JS object implementing `ActupProvider` interface
    /// * `config_json` — optional JSON config (subset of actup config).
    ///   If omitted, defaults are used.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` error on serde failures or if the provider throws.
    #[wasm_bindgen(js_name = "checkRefs")]
    pub async fn check_refs(
        yaml: &str,
        provider: JsProvider,
        config_json: Option<String>,
    ) -> Result<JsValue, JsValue> {
        let config: Config = match config_json {
            Some(ref json) => serde_json::from_str(json)
                .map_err(|e| JsValue::from_str(&format!("invalid config JSON: {e}")))?,
            None => Config::default(),
        };

        let host = config
            .default_host
            .as_deref()
            .unwrap_or("github.com")
            .to_owned();
        let adapter = JsProviderAdapter::new(provider, host.clone(), Kind::GitHub);

        let mut registry = Registry::new();
        registry.set_default_host(&host);
        registry.register(Box::new(adapter));

        let locations = extract_uses_from_yaml(yaml);
        let mut results: Vec<CheckResult> = Vec::with_capacity(locations.len());

        for loc in &locations {
            let parsed = match parse_uses(loc.value()) {
                Ok(Some(action_ref)) => action_ref,
                // Local/docker refs or parse errors — skip
                Ok(None) => continue,
                Err(_) => continue,
            };

            match check_action_remote(&parsed, &config, &registry).await {
                Ok(result) => results.push(result),
                Err(e) => {
                    // Convert provider errors to Unresolvable status
                    results.push(CheckResult::new(
                        parsed,
                        CheckStatus::Unresolvable {
                            reason: e.to_string(),
                            is_environment_error: e.is_environment_error(),
                        },
                    ));
                }
            }
        }

        serde_wasm_bindgen::to_value(&results).map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// A text edit to apply to the YAML content.
    #[derive(Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct UpdateEdit {
        /// 1-indexed line number
        line: usize,
        /// 0-indexed byte offset of the ref portion start within the line
        ref_start: usize,
        /// 0-indexed byte offset of the ref portion end within the line
        ref_end: usize,
        /// The full original `uses:` value (e.g. `"actions/checkout@v3"`)
        original: String,
        /// The replacement ref string (e.g. `"v4.2.0"`)
        new_ref: String,
    }

    /// Plan text edits from check results.
    ///
    /// Takes the original YAML and a JSON array of [`CheckResult`]s (as returned
    /// by [`checkRefs`]), and returns an array of [`UpdateEdit`] objects for
    /// each outdated ref.
    ///
    /// This is a pure, synchronous function. The caller applies the edits.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` error on serde failures.
    #[wasm_bindgen(js_name = "planUpdates")]
    pub fn plan_updates(yaml: &str, results_json: &str) -> Result<JsValue, JsValue> {
        let results: Vec<CheckResult> = serde_json::from_str(results_json)
            .map_err(|e| JsValue::from_str(&format!("invalid results JSON: {e}")))?;

        let locations = extract_uses_from_yaml(yaml);
        let mut edits: Vec<UpdateEdit> = Vec::new();

        for result in &results {
            let status = result.status();
            let target_ref = match status {
                CheckStatus::Outdated { target, .. } => target.version().as_str().to_owned(),
                _ => continue,
            };

            // Find the matching location in the YAML
            let action_ref = result.action_ref();
            let uses_str = action_ref.to_uses_string();

            if let Some(loc) = locations.iter().find(|l| l.value() == uses_str) {
                if let Some(range) = loc.ref_range() {
                    edits.push(UpdateEdit {
                        line: loc.line(),
                        ref_start: range.start,
                        ref_end: range.end,
                        original: loc.value().to_owned(),
                        new_ref: target_ref,
                    });
                }
            }
        }

        serde_wasm_bindgen::to_value(&edits).map_err(|e| JsValue::from_str(&e.to_string()))
    }
}
