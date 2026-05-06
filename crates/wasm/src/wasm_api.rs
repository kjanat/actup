//! Public WASM-facing API. Compiled only on `wasm32` targets.

use actup_core::{
    config::Config,
    extract_uses_from_yaml, parse_uses, policy,
    provider::{Kind, Registry},
    resolution::{CheckResult, CheckStatus, check_action_remote},
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use crate::bridge::{JsProvider, JsProviderAdapter};

/// Subset of [`Config`] passable from JS callers.
///
/// JS code constructs an object matching this shape; `serde_wasm_bindgen`
/// deserializes it directly. Wider config knobs ([`Cache`], [`HostConfig`],
/// per-action overrides) aren't part of the WASM surface — those are
/// repo-config concerns, not runtime arguments.
///
/// [`Cache`]: actup_core::config::Cache
/// [`HostConfig`]: actup_core::config::HostConfig
#[derive(Debug, Default, Deserialize, tsify_next::Tsify)]
#[tsify(from_wasm_abi)]
pub struct ActupConfig {
    /// Default host for bare `owner/repo` refs (e.g. `"github.com"`).
    #[serde(default)]
    pub default_host: Option<String>,
    /// Global update policy.
    #[serde(default)]
    pub update_policy: Option<policy::Update>,
}

impl From<ActupConfig> for Config {
    fn from(value: ActupConfig) -> Self {
        let mut config = Self::default();
        if let Some(host) = value.default_host {
            config.default_host = Some(host);
        }
        if let Some(policy) = value.update_policy {
            config.update_policy = policy;
        }
        config
    }
}

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
#[wasm_bindgen(js_name = "parseUsesRefs", unchecked_return_type = "UsesLocation[]")]
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
/// * `config` — optional [`ActupConfig`]. If omitted, defaults are used.
///
/// # Errors
///
/// Returns `JsValue` error on serde failures or if the provider throws.
#[wasm_bindgen(js_name = "checkRefs", unchecked_return_type = "CheckResult[]")]
pub async fn check_refs(
    yaml: &str,
    provider: JsProvider,
    config: Option<ActupConfig>,
) -> Result<JsValue, JsValue> {
    let config: Config = config.map(Config::from).unwrap_or_default();

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
#[derive(Debug, Serialize, tsify_next::Tsify)]
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
#[wasm_bindgen(js_name = "planUpdates", unchecked_return_type = "UpdateEdit[]")]
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

        // Emit an edit for every matching location — the same action can
        // appear in multiple jobs/steps of a single workflow.
        let action_ref = result.action_ref();
        let uses_str = action_ref.to_uses_string();

        for loc in locations.iter().filter(|l| l.value() == uses_str) {
            if let Some(range) = loc.ref_range() {
                edits.push(UpdateEdit {
                    line: loc.line(),
                    ref_start: range.start,
                    ref_end: range.end,
                    original: loc.value().to_owned(),
                    new_ref: target_ref.clone(),
                });
            }
        }
    }

    serde_wasm_bindgen::to_value(&edits).map_err(|e| JsValue::from_str(&e.to_string()))
}
