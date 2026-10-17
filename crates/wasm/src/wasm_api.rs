//! Public WASM-facing API. Compiled only on `wasm32` targets.

use std::{path::Path, sync::Arc};

use actup_core::{
    ActionRef, SourceFormat, Uses, UsesLocation,
    config::{Config, EnvOverrides, repo_candidates},
    error::chain,
    extract_uses_from_yaml, is_host_segment, parse_action_path, parse_uses,
    provider::{Kind, Registry},
    resolution::{CheckResult, CheckStatus, RefKind, check_action_remote},
    rewrite::{Rewrite, Trailing, closer},
    version,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use crate::bridge::{self, JsProvider, JsProviderAdapter, VersionTag};

#[wasm_bindgen(typescript_custom_section)]
const CONFIG_TYPES: &str = r#"
/**
 * The config tables most callers pass to `checkRefs` and `planUpdates`.
 *
 * Both functions take any table of a repo config, such as the `RepoConfig`
 * that `parseConfig` returns with `scan.ignore` and `[[overrides]]`, and parse
 * it like a repo config file: an unknown `[policy]` key or a renamed key is an
 * error.
 */
export interface ActupConfig {
    /** Default host for bare `owner/repo` refs (e.g. `"github.com"`). */
    default_host?: string | null;
    /** The `[policy]` table. */
    policy?: Policy;
}

/** Every table of a repo config, as `parseConfig` returns it. */
export type RepoConfig = ActupConfig & { readonly [table: string]: unknown };
"#;

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::new().serialize_missing_as_null(true))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

fn config_from_js(value: &JsValue) -> Result<Config, JsValue> {
    let not_object = || JsValue::from_str("invalid config: expected an object");
    let json = js_sys::JSON::stringify(value)?
        .as_string()
        .ok_or_else(not_object)?;
    match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(serde_json::Value::Object(_)) => {}
        Ok(_) => return Err(not_object()),
        Err(e) => return Err(JsValue::from_str(&format!("invalid config: {e}"))),
    }
    Config::from_contents(
        Some((Path::new("config.json"), &json)),
        EnvOverrides::default(),
    )
    .map_err(|e| JsValue::from_str(&chain(&e)))
}

/// Parse the repo config file at `path` with `contents`, as the CLI parses it.
///
/// The extension of `path` picks the format: `toml`, `json`, `jsonc`, `yaml`
/// or `yml`. No user config and no `ACTUP_` variables apply. Pass the result
/// to `checkRefs` and `planUpdates`.
///
/// # Errors
///
/// Returns `JsValue` error for an invalid config, naming `path`.
#[wasm_bindgen(js_name = "parseConfig", unchecked_return_type = "RepoConfig")]
pub fn parse_config(path: &str, contents: &str) -> Result<JsValue, JsValue> {
    let config = Config::from_contents(Some((Path::new(path), contents)), EnvOverrides::default())
        .map_err(|e| JsValue::from_str(&chain(&e)))?;
    to_js(&config)
}

/// The repo config paths to look for in a directory, highest priority first.
///
/// Each path is relative and `/`-separated, for example `.config/actup.toml`.
#[wasm_bindgen(js_name = "repoConfigCandidates")]
#[must_use]
pub fn repo_config_candidates() -> Vec<String> {
    repo_candidates()
        .map(|candidate| candidate.relative())
        .collect()
}

/// Parse `uses:` references from workflow YAML content.
///
/// Returns a JS array of serialized [`actup_core::UsesLocation`] objects
/// (`line`, `column`, `value`, `raw_line`, `value_range`, `ref_range`,
/// `scalar_range`).
///
/// This is a pure, synchronous function — no network or filesystem access.
///
/// # Errors
///
/// Returns `JsValue` error if serialization to JS fails (should not happen
/// for valid input).
#[wasm_bindgen(js_name = "parseUsesRefs", unchecked_return_type = "UsesLocation[]")]
pub fn parse_uses_refs(yaml: &str) -> Result<JsValue, JsValue> {
    to_js(&extract_uses_from_yaml(yaml))
}

/// Parse one `uses:` value into an [`ActionRef`].
///
/// Returns `null` for local (`./path`) and `docker://` references.
///
/// # Errors
///
/// Returns `JsValue` error when `value` is not a valid action reference.
#[wasm_bindgen(js_name = "parseUses", unchecked_return_type = "ActionRef | null")]
pub fn parse_uses_value(value: &str) -> Result<JsValue, JsValue> {
    let parsed = parse_uses(value).map_err(|e| JsValue::from_str(&chain(&e)))?;
    to_js(&parsed)
}

/// Parse the `[host/]owner/repo[/subpath]` part of a `uses:` value.
///
/// # Errors
///
/// Returns `JsValue` error when `path` does not name `owner/repo`.
#[wasm_bindgen(js_name = "parseActionPath", unchecked_return_type = "ActionRepoId")]
pub fn parse_action_path_value(path: &str) -> Result<JsValue, JsValue> {
    let id = parse_action_path(path).map_err(|e| JsValue::from_str(&chain(&e)))?;
    to_js(&id)
}

/// Whether `segment` names a host when it starts a path of three or more segments.
#[wasm_bindgen(js_name = "isHostSegment")]
#[must_use]
pub fn is_host_segment_value(segment: &str) -> bool {
    is_host_segment(segment)
}

/// Sort version tags newest first by semver precedence.
///
/// # Errors
///
/// Returns `JsValue` error if `tags` is not an array of `VersionTag` objects.
#[wasm_bindgen(js_name = "sortVersionTags", unchecked_return_type = "VersionTag[]")]
pub fn sort_version_tags(
    #[wasm_bindgen(unchecked_param_type = "VersionTag[]")] tags: JsValue,
) -> Result<JsValue, JsValue> {
    let mut tags: Vec<VersionTag> =
        serde_wasm_bindgen::from_value(tags).map_err(|e| JsValue::from_str(&e.to_string()))?;
    bridge::sort_newest_first(&mut tags);
    to_js(&tags)
}

/// A [`CheckResult`] with the `uses:` location it was checked from.
#[derive(Debug, Serialize, Deserialize, tsify_next::Tsify)]
struct LocatedCheckResult {
    /// The `uses:` value in the YAML that produced this result
    location: UsesLocation,
    /// The action reference that was checked
    action_ref: ActionRef,
    /// The check status
    status: CheckStatus,
    /// What the ref names, when the version set lists it
    ref_kind: Option<RefKind>,
    /// The suggested target, when it sits on the commit the ref already names
    same_commit: Option<Arc<version::Info>>,
    /// Whether the line may take a comment after the ref: it can hold a
    /// comment and holds no other `uses:` value
    #[serde(default)]
    takes_comment: bool,
}

impl LocatedCheckResult {
    fn new(location: &UsesLocation, result: &CheckResult, takes_comment: bool) -> Self {
        Self {
            location: location.clone(),
            action_ref: result.action_ref().clone(),
            status: result.status().clone(),
            ref_kind: result.ref_kind(),
            same_commit: result.same_commit().cloned(),
            takes_comment,
        }
    }
}

/// Check `uses:` references in YAML against a host-provided JS provider.
///
/// Parses the YAML, resolves each non-local `uses:` ref through the
/// provider, and returns an array of [`LocatedCheckResult`] objects. Each
/// carries the [`UsesLocation`] of the value it checked. A value that is not
/// a valid reference reaches no provider and is unresolvable as a user error.
///
/// # Arguments
///
/// * `yaml` — workflow YAML content
/// * `provider` — JS object implementing `ActupProvider` interface
/// * `config` — optional config tables, such as a `RepoConfig`. If omitted,
///   defaults are used.
///
/// # Errors
///
/// The returned promise rejects for an invalid `config`, on serde failures,
/// or if the provider throws.
#[wasm_bindgen(
    js_name = "checkRefs",
    unchecked_return_type = "Promise<LocatedCheckResult[]>"
)]
pub fn check_refs(
    yaml: String,
    provider: JsProvider,
    #[wasm_bindgen(unchecked_param_type = "ActupConfig | null | undefined")] config: Option<
        JsValue,
    >,
) -> js_sys::Promise {
    wasm_bindgen_futures::future_to_promise(async move {
        let config = match config {
            Some(value) => config_from_js(&value)?,
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

        let locations = extract_uses_from_yaml(&yaml);
        let comment_lines = SourceFormat::Yaml.comment_lines(&yaml);
        let mut results: Vec<LocatedCheckResult> = Vec::with_capacity(locations.len());

        for loc in &locations {
            let parsed = match loc.classify() {
                None | Some(Uses::Local) => continue,
                Some(Uses::Action(parsed)) => parsed,
                Some(Uses::Invalid { action, error }) => {
                    let result = CheckResult::new(
                        action,
                        CheckStatus::Unresolvable {
                            reason: error.to_string(),
                            is_environment_error: false,
                        },
                    );
                    results.push(LocatedCheckResult::new(
                        loc,
                        &result,
                        comment_lines.contains(&loc.line()),
                    ));
                    continue;
                }
            };

            let result = match check_action_remote(&parsed, &config, &registry).await {
                Ok(result) => result,
                Err(e) => CheckResult::new(
                    parsed,
                    CheckStatus::Unresolvable {
                        reason: chain(&e),
                        is_environment_error: e.is_environment_error(),
                    },
                ),
            };
            results.push(LocatedCheckResult::new(
                loc,
                &result,
                comment_lines.contains(&loc.line()),
            ));
        }

        to_js(&results)
    })
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
    /// 0-indexed byte offset within the line where the replacement ends:
    /// `refEnd`, or the line end when the edit replaces or drops a version
    /// comment or writes a comment after a closing quote or flow bracket
    replace_end: usize,
    /// The full original `uses:` value (e.g. `"actions/checkout@v3"`)
    original: String,
    /// The replacement ref string (e.g. `"v4.2.0"` or a commit SHA)
    new_ref: String,
    /// The text that replaces `refStart..replaceEnd`: `newRef` plus any
    /// closing quote or flow bracket and version comment (e.g. `"<sha> # v4.2.0"`)
    new_text: String,
}

/// Plan text edits from located check results.
///
/// Takes a JSON array of [`LocatedCheckResult`]s (as returned by
/// [`checkRefs`]) and returns one [`UpdateEdit`] for each outdated or
/// pin-branch result whose `location` has a ref range. `config.policy.pin`
/// picks tag or SHA output, as in `actup update`. A pin-branch result pins
/// the branch or non-semver tag to its commit with a `# <ref>` comment. A
/// comment is written only for a result whose `takes_comment` is set.
///
/// This is a pure, synchronous function. The caller applies the edits.
///
/// # Errors
///
/// Returns `JsValue` error on serde failures or for an invalid `config`.
#[wasm_bindgen(js_name = "planUpdates", unchecked_return_type = "UpdateEdit[]")]
pub fn plan_updates(
    results_json: &str,
    #[wasm_bindgen(unchecked_param_type = "ActupConfig | null | undefined")] config: Option<
        JsValue,
    >,
) -> Result<JsValue, JsValue> {
    let config = match config {
        Some(value) => config_from_js(&value)?,
        None => Config::default(),
    };
    let results: Vec<LocatedCheckResult> = serde_json::from_str(results_json)
        .map_err(|e| JsValue::from_str(&format!("invalid results JSON: {e}")))?;
    let edits: Vec<UpdateEdit> = results
        .iter()
        .filter_map(|result| {
            let location = &result.location;
            let range = location.ref_range()?;
            let raw_line = location.raw_line();
            let (trailing, closing) = if result.takes_comment {
                (
                    Trailing::after(raw_line, range.end),
                    closer(raw_line, range.end),
                )
            } else {
                (Trailing::Foreign, "")
            };
            let rewrite = match &result.status {
                CheckStatus::Outdated { target, .. } => Rewrite::outdated(
                    config.policy_for(&result.action_ref).pin,
                    result.action_ref.is_sha(),
                    target,
                    trailing,
                ),
                CheckStatus::PinBranch { ref_name, sha } => Rewrite::pin(sha, ref_name, trailing),
                _ => return None,
            };
            let (new_text, to_line_end) = rewrite.splice(closing);
            Some(UpdateEdit {
                line: location.line(),
                ref_start: range.start,
                ref_end: range.end,
                replace_end: if to_line_end {
                    raw_line.len()
                } else {
                    range.end
                },
                original: location.value().to_owned(),
                new_text,
                new_ref: rewrite.new_ref,
            })
        })
        .collect();

    to_js(&edits)
}
