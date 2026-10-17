//! Public WASM-facing API. Compiled only on `wasm32` targets.

use std::{path::Path, sync::Arc};

use actup_core::{
    ActionRef, SourceFormat, Uses, UsesLocation,
    config::{Config, EnvOverrides, repo_candidates},
    error::chain,
    extract_uses_from_yaml, is_host_segment, parse_action_path, parse_uses,
    provider::{Kind, Registry},
    resolution::{CheckResult, CheckStatus, RefKind, check_action_fetched},
    rewrite::{Gap, Labels, Rewrite, Trailing, after_value, closer, comment_label, labeled_head},
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

/** A config file's path and text. The extension of `path` picks the format. */
export interface ConfigFile {
    path: string;
    contents: string;
}
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
    Config::from_explicit_contents(Path::new("config.json"), &json, EnvOverrides::default())
        .map_err(|e| JsValue::from_str(&chain(&e)))
}

/// Parse the repo config file at `path` with `contents`, as the CLI parses a
/// file that discovery found.
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

/// Parse the config file at `path` with `contents` that the user named, as
/// the CLI parses a `--config` file.
///
/// Every key applies. The extension of `path` picks the format, and no user
/// config and no `ACTUP_` variables apply.
///
/// # Errors
///
/// Returns `JsValue` error for an invalid config, naming `path`.
#[wasm_bindgen(js_name = "parseExplicitConfig", unchecked_return_type = "RepoConfig")]
pub fn parse_explicit_config(path: &str, contents: &str) -> Result<JsValue, JsValue> {
    let config = Config::from_explicit_contents(Path::new(path), contents, EnvOverrides::default())
        .map_err(|e| JsValue::from_str(&chain(&e)))?;
    to_js(&config)
}

#[derive(Deserialize)]
struct ConfigFile {
    path: String,
    contents: String,
}

impl ConfigFile {
    fn from_js(value: Option<JsValue>) -> Result<Option<Self>, JsValue> {
        value
            .map(|value| {
                serde_wasm_bindgen::from_value(value)
                    .map_err(|e| JsValue::from_str(&format!("invalid config file: {e}")))
            })
            .transpose()
    }

    fn parts(&self) -> (&Path, &str) {
        (Path::new(&self.path), &self.contents)
    }
}

/// Parse the repo config file that discovery found, then the config file
/// that the user named over it, as the CLI layers a `--config` file over the
/// repo config.
///
/// Every key of `explicit` applies, and `explicit` wins over `repo`. Either
/// file may be absent. No user config and no `ACTUP_` variables apply.
///
/// # Errors
///
/// Returns `JsValue` error for an invalid config, naming the file.
#[wasm_bindgen(
    js_name = "parseRepoAndExplicitConfig",
    unchecked_return_type = "RepoConfig"
)]
pub fn parse_repo_and_explicit_config(
    #[wasm_bindgen(unchecked_param_type = "ConfigFile | null | undefined")] repo: Option<JsValue>,
    #[wasm_bindgen(unchecked_param_type = "ConfigFile | null | undefined")] explicit: Option<
        JsValue,
    >,
) -> Result<JsValue, JsValue> {
    let repo = ConfigFile::from_js(repo)?;
    let explicit = ConfigFile::from_js(explicit)?;
    let config = Config::from_repo_and_explicit(
        repo.as_ref().map(ConfigFile::parts),
        explicit.as_ref().map(ConfigFile::parts),
        EnvOverrides::default(),
    )
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
/// `scalar_range`, `action_key`). `action_key` is `false` for a `uses` key outside
/// `jobs.<id>.uses`, `jobs.<id>.steps[*].uses` and `runs.steps[*].uses`.
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
#[derive(Debug, Serialize, Deserialize, tsify::Tsify)]
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
    /// Whether the same-commit target names more version components than the ref
    #[serde(default)]
    same_commit_pins: bool,
    /// Whether the line may take a comment after the ref: it can hold a
    /// comment and holds no other `uses:` value
    #[serde(default)]
    takes_comment: bool,
    /// Whether the fetched tags and branches list the first word of the
    /// comment after the ref, like `main` in `# main keep`
    #[serde(default)]
    label_listed: bool,
}

fn read_line(location: &UsesLocation, takes_comment: bool, ref_end: usize) -> &str {
    if takes_comment {
        location.raw_line()
    } else {
        labeled_head(Gap::Spaces, location.raw_line(), ref_end)
    }
}

impl LocatedCheckResult {
    fn new(
        location: &UsesLocation,
        result: &CheckResult,
        takes_comment: bool,
        versions: Option<&version::Set>,
    ) -> Self {
        let label_listed = location.ref_range().is_some_and(|range| {
            comment_label(after_value(
                read_line(location, takes_comment, range.end),
                range.end,
            ))
            .is_some_and(|label| versions.is_some_and(|versions| versions.names_ref(label)))
        });
        Self {
            location: location.clone(),
            action_ref: result.action_ref().clone(),
            status: result.status().clone(),
            ref_kind: result.ref_kind(),
            same_commit: result.same_commit().cloned(),
            same_commit_pins: result.same_commit_pins(),
            takes_comment,
            label_listed,
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
                        None,
                    ));
                    continue;
                }
            };

            let (result, versions) = match check_action_fetched(&parsed, &config, &registry).await {
                Ok(fetched) => fetched,
                Err(e) => (
                    CheckResult::new(
                        parsed,
                        CheckStatus::Unresolvable {
                            reason: chain(&e),
                            is_environment_error: e.is_environment_error(),
                        },
                    ),
                    None,
                ),
            };
            results.push(LocatedCheckResult::new(
                loc,
                &result,
                comment_lines.contains(&loc.line()),
                versions.as_ref(),
            ));
        }

        to_js(&results)
    })
}

/// A text edit to apply to the YAML content.
#[derive(Debug, Serialize, tsify::Tsify)]
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
/// comment is added only for a result whose `takes_comment` is set. On other
/// lines, a label comment right after the ref is rewritten and the rest of
/// the line stays.
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
            let read = read_line(location, result.takes_comment, range.end);
            let labels = Labels::checked(result.action_ref.ref_str(), result.label_listed);
            let trailing = match Trailing::after(read, range.end, &labels) {
                Trailing::None if !result.takes_comment => Trailing::Foreign,
                trailing => trailing,
            };
            let closing = closer(read, range.end);
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
                replace_end: if to_line_end { read.len() } else { range.end },
                original: location.value().to_owned(),
                new_text,
                new_ref: rewrite.new_ref,
            })
        })
        .collect();

    to_js(&edits)
}
