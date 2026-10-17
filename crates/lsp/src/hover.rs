//! Hover information for `uses:` references.
//!
//! Shows action metadata (name, description, author), version status,
//! repository link, and deprecation warnings on hover over `uses:` lines.

use std::{
    fmt::Write as _,
    sync::Arc,
    time::{Duration, SystemTime},
};

use actup_core::{
    ActionRef, Clock, Config, SystemClock, check_action_with_provider, extract_uses_from_yaml,
    parse_uses, provider,
    resolution::{CheckResult, CheckStatus, RefKind},
    version,
};
use dashmap::DashMap;
use tower_lsp_server::ls_types::{
    Hover, HoverContents, MarkupContent, MarkupKind, Position, Range,
};

use crate::file_cache::FileCache;

/// Shared, read-only state threaded through a single hover request.
///
/// Introduced to collapse the 8-arg signature on `compute_hover`. All
/// fields are borrowed for the duration of the request — nothing is
/// cloned or stored.
pub struct HoverContext<'a> {
    pub config: &'a Config,
    pub registry: &'a provider::Registry,
    pub file_cache: &'a FileCache,
    pub meta_cache: &'a ActionMetaCache,
    pub encoding: crate::position::PositionEncoding,
}

/// Cached action.yml metadata for hover display.
///
/// Stored in an in-memory cache keyed by `ActionRepoId` + ref to avoid
/// re-fetching on every hover request. The provider's `get_action_yml`
/// is not cached by `CachingProvider`, so we cache results ourselves.
#[derive(Debug, Clone)]
pub struct CachedActionMeta {
    /// Action name from action.yml (e.g. "Checkout").
    pub name: Option<String>,
    /// Action description from action.yml.
    pub description: Option<String>,
    /// Action author from action.yml.
    pub author: Option<String>,
    /// Deprecated inputs: (`input_name`, `deprecation_message`).
    pub deprecated_inputs: Vec<(String, String)>,
    /// Runtime (e.g. "node20", "composite", "docker").
    pub runtime: Option<String>,
}

/// Key for the action metadata cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionMetaCacheKey {
    owner: String,
    repo: String,
    ref_str: String,
    host: Option<String>,
}

impl ActionMetaCacheKey {
    fn from_action_ref(action_ref: &ActionRef) -> Self {
        Self {
            owner: action_ref.owner().to_owned(),
            repo: action_ref.repo().to_owned(),
            ref_str: action_ref.ref_str().to_owned(),
            host: action_ref.host().map(str::to_owned),
        }
    }
}

/// In-memory cache for action.yml metadata used by hover.
///
/// This cache is unbounded, but in practice grows slowly: one entry per
/// unique (owner, repo, ref) triple the user hovers over in a session.
/// A typical session might see 10–50 distinct actions, so memory impact
/// is negligible. If this ever becomes a concern, an LRU wrapper could
/// be added with minimal API change.
pub struct ActionMetaCache {
    entries: DashMap<ActionMetaCacheKey, (Arc<CachedActionMeta>, SystemTime)>,
    clock: Arc<dyn Clock>,
}

impl ActionMetaCache {
    fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            entries: DashMap::new(),
            clock,
        }
    }

    /// The metadata stored under `key` at most `ttl` ago.
    pub fn get(&self, key: &ActionMetaCacheKey, ttl: Duration) -> Option<Arc<CachedActionMeta>> {
        let (meta, at) = self.entries.get(key)?.value().clone();
        self.clock
            .now()
            .duration_since(at)
            .is_ok_and(|age| age <= ttl)
            .then_some(meta)
    }

    /// Store `meta` under `key`, dated now.
    pub fn insert(&self, key: ActionMetaCacheKey, meta: Arc<CachedActionMeta>) {
        self.entries.insert(key, (meta, self.clock.now()));
    }
}

/// Create a new empty action metadata cache.
pub fn new_action_meta_cache() -> ActionMetaCache {
    ActionMetaCache::with_clock(Arc::new(SystemClock))
}

/// A parsed action located at a specific position in the document.
struct LocatedActionForHover {
    action_ref: ActionRef,
    /// 0-indexed line number.
    line: u32,
    /// Byte range of the full `uses:` value in the line.
    value_col_start: u32,
    value_col_end: u32,
}

/// Find the action reference at the given cursor position in the document.
///
/// Returns `None` if the cursor is not on a `uses:` value.
///
/// Column positions are converted from byte offsets to the negotiated
/// LSP position encoding for comparison with the incoming cursor position
/// and for use in the returned hover range.
fn find_action_at_position(
    text: &str,
    position: Position,
    encoding: crate::position::PositionEncoding,
) -> Option<LocatedActionForHover> {
    let locations = extract_uses_from_yaml(text);

    for loc in locations {
        let Some(line_0) = loc
            .line()
            .checked_sub(1)
            .and_then(|line| u32::try_from(line).ok())
        else {
            continue;
        };

        if line_0 != position.line {
            continue;
        }

        let raw_line = loc.raw_line();
        let Some(value_range) = loc.value_range().or_else(|| loc.scalar_range()) else {
            continue;
        };
        let col_start = crate::position::byte_to_col(raw_line, value_range.start, encoding);
        let col_end = crate::position::byte_to_col(raw_line, value_range.end, encoding);

        if position.character < col_start || position.character >= col_end {
            continue;
        }

        // Parse the uses value — skip local/docker refs.
        if let Ok(Some(action_ref)) = parse_uses(loc.value()) {
            return Some(LocatedActionForHover {
                action_ref,
                line: line_0,
                value_col_start: col_start,
                value_col_end: col_end,
            });
        }
    }

    None
}

/// Fetch or retrieve cached action.yml metadata.
async fn get_action_meta(
    action_ref: &ActionRef,
    registry: &provider::Registry,
    cache: &ActionMetaCache,
    ttl: Duration,
) -> Option<Arc<CachedActionMeta>> {
    let cache_key = ActionMetaCacheKey::from_action_ref(action_ref);

    // Check cache first.
    if let Some(cached) = cache.get(&cache_key, ttl) {
        return Some(cached);
    }

    // Fetch from provider.
    let repo_id = action_ref.to_repo_id();
    let provider = registry.find_for_repo(&repo_id)?;

    let action_yml = match provider
        .get_action_yml(&repo_id, action_ref.ref_str())
        .await
    {
        Ok(yml) => yml,
        Err(e) => {
            tracing::debug!(
                action = %action_ref,
                error = %actup_core::error::chain(&e),
                "failed to fetch action.yml for hover"
            );
            return None;
        }
    };

    let deprecated_inputs: Vec<(String, String)> = action_yml
        .inputs()
        .iter()
        .filter_map(|(name, input)| {
            input
                .deprecation_message()
                .map(|msg| (name.clone(), msg.to_owned()))
        })
        .collect();

    let runtime = action_yml.runs().map(|r| r.using().to_owned());

    let meta = Arc::new(CachedActionMeta {
        name: action_yml.name().map(str::to_owned),
        description: action_yml.description().map(str::to_owned),
        author: action_yml.author().map(str::to_owned),
        deprecated_inputs,
        runtime,
    });

    cache.insert(cache_key, Arc::clone(&meta));
    Some(meta)
}

/// Build the Markdown link to an action's repository on its host.
fn repo_link(action_ref: &ActionRef, default_host: Option<&str>) -> String {
    let host = action_ref.host().or(default_host).unwrap_or("github.com");
    format!(
        "[View on {host}](https://{host}/{}/{})",
        action_ref.owner(),
        action_ref.repo()
    )
}

/// Format version status information for hover display.
fn format_version_status(status: &CheckStatus, ref_kind: Option<RefKind>) -> String {
    let kind = RefKind::noun(ref_kind);
    match status {
        CheckStatus::UpToDate => "Up to date".to_owned(),
        CheckStatus::Outdated { current, target } => {
            format!(
                "**Outdated**: {} -> {} (`{}`)",
                current,
                target.version(),
                target.sha().get(..7).unwrap_or_else(|| target.sha()),
            )
        }
        CheckStatus::Pinned { sha, tag } => {
            let tag_info = tag
                .as_ref()
                .map_or_else(|| "unknown version".into(), String::clone);
            format!(
                "**Pinned** to `{}` ({tag_info})",
                sha.get(..12).unwrap_or(sha),
            )
        }
        CheckStatus::Ignored => "Ignored by config".to_owned(),
        CheckStatus::Floating { ref_name } => {
            format!("**Floating {kind}** `{ref_name}` — pin to a tag or SHA")
        }
        CheckStatus::PinBranch { ref_name, sha } => {
            format!(
                "**Pin {kind}** `{ref_name}` -> `{}` # {ref_name}",
                sha.get(..12).unwrap_or(sha)
            )
        }
        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => {
            if *is_environment_error {
                format!("Check failed: {reason}")
            } else {
                format!("**Unresolvable**: {reason}")
            }
        }
    }
}

fn format_check_result(result: &CheckResult) -> String {
    let Some(target) = result.same_commit() else {
        return format_version_status(result.status(), result.ref_kind());
    };
    let kind = result
        .ref_kind()
        .map(|k| format!("{k} "))
        .unwrap_or_default();
    let lead = if result.same_commit_pins() {
        "**Pin available**, same commit"
    } else {
        "**Same commit**"
    };
    format!(
        "{lead}: {kind}`{}` = {} (`{}`)",
        result.action_ref().ref_str(),
        target.version(),
        target.sha().get(..7).unwrap_or_else(|| target.sha()),
    )
}

/// Build the full hover markdown content.
fn build_hover_content(
    action_ref: &ActionRef,
    meta: Option<&CachedActionMeta>,
    result: &CheckResult,
    latest_version: Option<&str>,
    default_host: Option<&str>,
) -> String {
    let mut md = String::with_capacity(512);

    // Header: action name or owner/repo
    if let Some(meta) = meta
        && let Some(name) = &meta.name
    {
        let _ = writeln!(md, "### {name}");
    } else {
        let _ = writeln!(md, "### {}/{}", action_ref.owner(), action_ref.repo());
    }

    // Description
    if let Some(meta) = meta
        && let Some(desc) = &meta.description
    {
        let _ = writeln!(md, "\n{desc}");
    }

    // Author
    if let Some(meta) = meta
        && let Some(author) = &meta.author
    {
        let _ = writeln!(md, "\n*by {author}*");
    }

    let _ = writeln!(md);

    // Version info section
    let _ = writeln!(md, "---");
    let _ = writeln!(md, "| | |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| **Current** | `{}` |", action_ref.ref_str());

    if let Some(latest) = latest_version {
        let _ = writeln!(md, "| **Latest** | `{latest}` |");
    }

    let _ = writeln!(md, "| **Status** | {} |", format_check_result(result));

    // Runtime
    if let Some(meta) = meta
        && let Some(runtime) = &meta.runtime
    {
        let _ = writeln!(md, "| **Runtime** | {runtime} |");
    }

    let _ = writeln!(md, "\n{}", repo_link(action_ref, default_host));

    // Deprecation warnings
    if let Some(meta) = meta
        && !meta.deprecated_inputs.is_empty()
    {
        let _ = writeln!(md, "\n---");
        let _ = writeln!(md, "**Deprecated inputs:**");
        for (input_name, message) in &meta.deprecated_inputs {
            let _ = writeln!(md, "- `{input_name}`: {message}");
        }
    }

    md
}

/// Compute hover information for a position in a document.
///
/// Returns `None` if the cursor is not on a `uses:` reference or if the
/// reference is local/docker.
pub async fn compute_hover(
    text: &str,
    position: Position,
    document_versions: Option<&std::collections::HashMap<actup_core::ActionRepoId, version::Set>>,
    ctx: &HoverContext<'_>,
) -> Option<Hover> {
    let located = find_action_at_position(text, position, ctx.encoding)?;
    let repo_id = located.action_ref.to_repo_id().without_subpath();

    let versions = resolve_hover_versions(&repo_id, document_versions, ctx).await;
    let result = compute_hover_status(
        &located.action_ref,
        &repo_id,
        versions.as_ref().map(Option::as_ref),
        ctx,
    )
    .await;

    let latest_version = versions
        .as_ref()
        .ok()
        .and_then(|opt| opt.as_ref())
        .and_then(|v| v.latest())
        .map(|info| info.version().to_string());

    let meta = get_action_meta(
        &located.action_ref,
        ctx.registry,
        ctx.meta_cache,
        ctx.config.cache.ttl,
    )
    .await;
    let content = build_hover_content(
        &located.action_ref,
        meta.as_deref(),
        &result,
        latest_version.as_deref(),
        ctx.registry.default_host(),
    );

    let range = Range {
        start: Position {
            line: located.line,
            character: located.value_col_start,
        },
        end: Position {
            line: located.line,
            character: located.value_col_end,
        },
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: content,
        }),
        range: Some(range),
    })
}

/// Resolve version data for a hover request.
///
/// Prefers the document-level cache populated by the most recent diagnostic
/// pass (avoids re-fetching on every hover). Falls back to the file cache
/// + provider if the document cache has no entry for this repo.
///
/// Returns:
/// - `Ok(Some(set))` when versions were obtained (cache hit or fresh fetch).
/// - `Ok(None)` when no provider is registered for the repo's host.
/// - `Err(e)` when the provider returned an error (network, auth, rate limit).
async fn resolve_hover_versions(
    repo_id: &actup_core::ActionRepoId,
    document_versions: Option<&std::collections::HashMap<actup_core::ActionRepoId, version::Set>>,
    ctx: &HoverContext<'_>,
) -> Result<Option<version::Set>, actup_core::Error> {
    if let Some(v) = document_versions.and_then(|vs| vs.get(repo_id).cloned()) {
        return Ok(Some(v));
    }
    match crate::file_cache::fetch_versions_cached(repo_id, ctx.registry, ctx.file_cache).await {
        Some(Ok(v)) => Ok(Some(v)),
        Some(Err(e)) => {
            tracing::debug!(error = %actup_core::error::chain(&e), "failed to fetch versions for hover");
            Err(e)
        }
        None => Ok(None),
    }
}

/// Run the policy check against the resolved versions, or produce an
/// `Unresolvable` status if versions / provider lookup failed.
async fn compute_hover_status(
    action_ref: &ActionRef,
    repo_id: &actup_core::ActionRepoId,
    versions: Result<Option<&version::Set>, &actup_core::Error>,
    ctx: &HoverContext<'_>,
) -> CheckResult {
    let unresolvable = |reason: String| {
        CheckResult::new(
            action_ref.clone(),
            CheckStatus::Unresolvable {
                reason,
                is_environment_error: true,
            },
        )
    };
    let v = match versions {
        Ok(Some(v)) => v,
        Ok(None) => return unresolvable("no provider available".into()),
        Err(e) => return unresolvable(actup_core::error::chain(e)),
    };
    let Some(provider) = ctx.registry.find_for_repo(repo_id) else {
        return unresolvable("no provider available".into());
    };
    check_action_with_provider(action_ref, v, ctx.config, provider).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::Ordering;

    use actup_core::{resolution::CheckStatus, version};

    use super::*;

    #[test]
    fn find_action_at_cursor_on_uses_line() {
        let yaml = "      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 20,
        };
        let located =
            find_action_at_position(yaml, pos, crate::position::PositionEncoding::Utf16).unwrap();
        assert_eq!(located.action_ref.owner(), "actions");
        assert_eq!(located.action_ref.repo(), "checkout");
        assert_eq!(located.action_ref.ref_str(), "v4");
    }

    #[test]
    fn find_action_matches_an_escaped_value_on_its_scalar() {
        let line = r#"    steps: [{ uses: "actions/checkout@v1.0.\x30" }, { uses: "actions/setup-node@v4.\x30" }]"#;
        let yaml = format!("jobs:\n  build:\n{line}\n");
        let at = |needle: &str| {
            let Some(byte) = line.find(needle) else {
                panic!("{needle} is not on the line");
            };
            let position = Position {
                line: 2,
                character: crate::position::byte_to_col(
                    line,
                    byte,
                    crate::position::PositionEncoding::Utf16,
                ),
            };
            find_action_at_position(&yaml, position, crate::position::PositionEncoding::Utf16)
        };
        let span = |needle: &str| {
            let Some(start) = line.find(needle) else {
                panic!("{needle} is not on the line");
            };
            let encoding = crate::position::PositionEncoding::Utf16;
            (
                crate::position::byte_to_col(line, start, encoding),
                crate::position::byte_to_col(line, start + needle.len(), encoding),
            )
        };

        let Some(first) = at("checkout") else {
            panic!("no hover on the first escaped value");
        };
        assert_eq!(first.action_ref.repo(), "checkout");
        assert_eq!(first.action_ref.ref_str(), "v1.0.0");
        assert_eq!(
            (first.value_col_start, first.value_col_end),
            span(r#""actions/checkout@v1.0.\x30""#)
        );
        let Some(second) = at("setup-node") else {
            panic!("no hover on the second escaped value");
        };
        assert_eq!(second.action_ref.repo(), "setup-node");
        assert_eq!(
            (second.value_col_start, second.value_col_end),
            span(r#""actions/setup-node@v4.\x30""#)
        );
        assert!(at("}, {").is_none());
        assert!(at("steps").is_none());
    }

    #[test]
    fn find_action_returns_none_off_uses_value() {
        let yaml = "      - uses: actions/checkout@v4\n";
        // Position before the value.
        let pos = Position {
            line: 0,
            character: 5,
        };
        assert!(
            find_action_at_position(yaml, pos, crate::position::PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn find_action_returns_none_wrong_line() {
        let yaml = "name: CI\n      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 20,
        };
        assert!(
            find_action_at_position(yaml, pos, crate::position::PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn find_action_returns_none_for_local_ref() {
        let yaml = "      - uses: ./local-action\n";
        let pos = Position {
            line: 0,
            character: 18,
        };
        assert!(
            find_action_at_position(yaml, pos, crate::position::PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn find_action_at_end_of_value() {
        let yaml = "      - uses: actions/checkout@v4\n";
        // Character at the end boundary of the value.
        let pos = Position {
            line: 0,
            character: 33, // right at end
        };
        // Should still be within bounds (inclusive end).
        // value is "actions/checkout@v4" = 19 chars, starts at col 14, ends at 33
        let located = find_action_at_position(yaml, pos, crate::position::PositionEncoding::Utf16);
        // 33 == value_col_end, we use `>` not `>=` for end check
        assert!(located.is_none());
    }

    #[test]
    fn find_action_at_start_of_value() {
        let yaml = "      - uses: actions/checkout@v4\n";
        let pos = Position {
            line: 0,
            character: 14, // start of value
        };
        let located =
            find_action_at_position(yaml, pos, crate::position::PositionEncoding::Utf16).unwrap();
        assert_eq!(located.action_ref.repo(), "checkout");
    }

    #[test]
    fn repo_link_names_the_host_of_the_ref() {
        let cases = [
            (
                ActionRef::simple("actions", "checkout", "v4"),
                Some("github.com"),
                "[View on github.com](https://github.com/actions/checkout)",
            ),
            (
                ActionRef::simple("actions", "checkout", "v4"),
                Some("ghe.mycorp.internal"),
                "[View on ghe.mycorp.internal](https://ghe.mycorp.internal/actions/checkout)",
            ),
            (
                ActionRef::simple("actions", "checkout", "v4"),
                None,
                "[View on github.com](https://github.com/actions/checkout)",
            ),
            (
                ActionRef::new(
                    Some("ghe.mycorp.internal".to_owned()),
                    "platform",
                    "ci-actions",
                    None,
                    "v1",
                ),
                Some("github.com"),
                "[View on ghe.mycorp.internal](https://ghe.mycorp.internal/platform/ci-actions)",
            ),
            (
                ActionRef::new(
                    Some("gitlab.com".to_owned()),
                    "group",
                    "project",
                    None,
                    "v1",
                ),
                Some("github.com"),
                "[View on gitlab.com](https://gitlab.com/group/project)",
            ),
        ];
        for (action_ref, default_host, link) in cases {
            assert_eq!(repo_link(&action_ref, default_host), link, "{action_ref}");
        }
    }

    #[test]
    fn build_hover_content_links_a_gitlab_ref_to_gitlab() {
        let action_ref = ActionRef::new(
            Some("gitlab.com".to_owned()),
            "group",
            "project",
            None,
            "v1",
        );
        let result = CheckResult::new(action_ref.clone(), CheckStatus::UpToDate);

        let content = build_hover_content(&action_ref, None, &result, None, Some("github.com"));

        assert!(
            content.contains("\n[View on gitlab.com](https://gitlab.com/group/project)\n"),
            "{content}"
        );
        assert!(!content.contains("GitHub"), "{content}");
    }

    #[test]
    fn format_version_status_up_to_date() {
        assert_eq!(
            format_version_status(&CheckStatus::UpToDate, None),
            "Up to date"
        );
    }

    #[test]
    fn format_version_status_outdated() {
        let target = Arc::new(version::Info::new(
            version::Version::parse("v2.0.0"),
            "abc1234567890abcdef1234567890abcdef123456",
        ));
        let status = CheckStatus::Outdated {
            current: "v1.0.0".into(),
            target,
        };
        let result = format_version_status(&status, None);
        assert!(result.contains("Outdated"));
        assert!(result.contains("v1.0.0"));
        assert!(result.contains("v2.0.0"));
        assert!(result.contains("abc1234"));
    }

    #[test]
    fn format_version_status_pinned_with_tag() {
        let status = CheckStatus::Pinned {
            sha: "abc1234567890abcdef1234567890abcdef123456".into(),
            tag: Some("v3.1.0".into()),
        };
        let result = format_version_status(&status, None);
        assert!(result.contains("Pinned"));
        assert!(result.contains("v3.1.0"));
        assert!(result.contains("abc123456789"));
    }

    #[test]
    fn format_version_status_pinned_unknown_tag() {
        let status = CheckStatus::Pinned {
            sha: "abc1234567890abcdef1234567890abcdef123456".into(),
            tag: None,
        };
        let result = format_version_status(&status, None);
        assert!(result.contains("unknown version"));
    }

    #[test]
    fn format_version_status_pin_names_the_ref_kind() {
        let status = CheckStatus::PinBranch {
            ref_name: "nightly".into(),
            sha: "c".repeat(40),
        };
        let pin = |kind| format_version_status(&status, kind);
        assert_eq!(
            pin(Some(RefKind::Tag)),
            format!("**Pin tag** `nightly` -> `{}` # nightly", "c".repeat(12))
        );
        assert!(pin(Some(RefKind::Branch)).starts_with("**Pin branch** `nightly`"));
        assert!(pin(None).starts_with("**Pin ref** `nightly`"));
    }

    #[test]
    fn format_version_status_floating_names_the_ref_kind() {
        let status = CheckStatus::Floating {
            ref_name: "nightly".into(),
        };
        let floating = |kind| format_version_status(&status, kind);
        assert_eq!(
            floating(Some(RefKind::Tag)),
            "**Floating tag** `nightly` — pin to a tag or SHA"
        );
        assert!(floating(Some(RefKind::Branch)).starts_with("**Floating branch** `nightly`"));
        assert!(floating(None).starts_with("**Floating ref** `nightly`"));
    }

    #[test]
    fn format_version_status_ignored() {
        assert_eq!(
            format_version_status(&CheckStatus::Ignored, None),
            "Ignored by config"
        );
    }

    #[test]
    fn format_version_status_unresolvable_user_error() {
        let status = CheckStatus::Unresolvable {
            reason: "repository not found".into(),
            is_environment_error: false,
        };
        let result = format_version_status(&status, None);
        assert!(result.contains("Unresolvable"));
        assert!(result.contains("repository not found"));
    }

    #[test]
    fn format_version_status_unresolvable_env_error() {
        let status = CheckStatus::Unresolvable {
            reason: "rate limited".into(),
            is_environment_error: true,
        };
        let result = format_version_status(&status, None);
        assert!(result.contains("Check failed"));
        assert!(result.contains("rate limited"));
    }

    #[test]
    fn build_hover_content_with_full_meta() {
        let action_ref = ActionRef::simple("actions", "checkout", "v3");
        let meta = CachedActionMeta {
            name: Some("Checkout".into()),
            description: Some("Checks out your repository.".into()),
            author: Some("GitHub".into()),
            deprecated_inputs: vec![("path".into(), "Use working-directory instead".into())],
            runtime: Some("node20".into()),
        };
        let result = CheckResult::new(action_ref.clone(), CheckStatus::UpToDate);

        let content = build_hover_content(
            &action_ref,
            Some(&meta),
            &result,
            Some("v4.2.0"),
            Some("github.com"),
        );

        assert!(content.contains("### Checkout"));
        assert!(content.contains("Checks out your repository."));
        assert!(content.contains("*by GitHub*"));
        assert!(content.contains("`v3`"));
        assert!(content.contains("`v4.2.0`"));
        assert!(content.contains("Up to date"));
        assert!(content.contains("node20"));
        assert!(content.contains("https://github.com/actions/checkout"));
        assert!(content.contains("Deprecated inputs"));
        assert!(content.contains("`path`"));
        assert!(content.contains("Use working-directory instead"));
    }

    #[test]
    fn build_hover_content_without_meta() {
        let action_ref = ActionRef::simple("actions", "setup-node", "v4");
        let result = CheckResult::new(action_ref.clone(), CheckStatus::UpToDate);

        let content = build_hover_content(&action_ref, None, &result, None, Some("github.com"));

        // Falls back to owner/repo as header.
        assert!(content.contains("### actions/setup-node"));
        assert!(content.contains("`v4`"));
        assert!(content.contains("Up to date"));
        assert!(content.contains("https://github.com/actions/setup-node"));
        // No deprecated inputs section.
        assert!(!content.contains("Deprecated inputs"));
    }

    #[test]
    fn build_hover_content_outdated_status() {
        let action_ref = ActionRef::simple("actions", "checkout", "v3");
        let target = Arc::new(version::Info::new(
            version::Version::parse("v4.2.0"),
            "abc1234567890abcdef1234567890abcdef123456",
        ));
        let result = CheckResult::new(
            action_ref.clone(),
            CheckStatus::Outdated {
                current: "v3".into(),
                target,
            },
        );

        let content = build_hover_content(
            &action_ref,
            None,
            &result,
            Some("v4.2.0"),
            Some("github.com"),
        );

        assert!(content.contains("Outdated"));
        assert!(content.contains("v3"));
        assert!(content.contains("v4.2.0"));
    }

    #[test]
    fn build_hover_content_same_commit_branch() {
        use crate::test_utils::DUMMY_SHA;

        let action_ref = ActionRef::simple("rust-lang", "crates-io-auth-action", "v1");
        let mut versions = version::Set::build(
            vec![crate::test_utils::make_version_info("v1.0.5", DUMMY_SHA)],
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            None,
        );
        versions.set_branches(std::collections::HashMap::from([(
            "v1".to_string(),
            DUMMY_SHA.to_string(),
        )]));
        let config = actup_core::Config {
            policy: actup_core::config::Policy {
                branch: actup_core::policy::Branch::Update,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = actup_core::check_action(&action_ref, &versions, &config);

        let content = build_hover_content(&action_ref, None, &result, None, Some("github.com"));

        assert!(
            content.contains("**Pin available**, same commit: branch `v1` = v1.0.5 (`abc1234`)"),
            "{content}"
        );
    }

    #[test]
    fn build_hover_content_same_commit_alias() {
        use crate::test_utils::DUMMY_SHA;

        let action_ref = ActionRef::simple("rust-lang", "crates-io-auth-action", "v1.0.5");
        let major = crate::test_utils::make_version_info("v1", DUMMY_SHA);
        let versions = version::Set::build(
            vec![
                std::sync::Arc::clone(&major),
                crate::test_utils::make_version_info("v1.0.5", DUMMY_SHA),
            ],
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(1, major)]),
            None,
        );
        let config = actup_core::Config {
            policy: actup_core::config::Policy {
                track: actup_core::policy::Track::Major,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = actup_core::check_action(&action_ref, &versions, &config);

        let content = build_hover_content(&action_ref, None, &result, None, Some("github.com"));

        assert!(
            content.contains("**Same commit**: tag `v1.0.5` = v1 (`abc1234`)"),
            "{content}"
        );
    }

    // --- compute_hover async tests ---

    #[tokio::test]
    async fn compute_hover_returns_hover_for_uses_line() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();

        let text = "      - uses: actions/checkout@v3\n";
        let position = Position {
            line: 0,
            character: 20,
        };

        let ctx = HoverContext {
            config: &config,
            registry: &registry,
            file_cache: &file_cache,
            meta_cache: &meta_cache,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let hover = compute_hover(text, position, None, &ctx).await;
        let hover = hover.unwrap();

        match &hover.contents {
            HoverContents::Markup(mc) => {
                assert!(mc.value.contains("actions/checkout"));
                assert!(mc.value.contains("v3")); // current ref
            }
            other => panic!("expected Markup hover contents, got: {other:?}"),
        }

        // Range should cover the uses value.
        let range = hover.range.unwrap();
        assert_eq!(range.start.line, 0);
    }

    #[tokio::test]
    async fn compute_hover_returns_none_for_non_uses_line() {
        let provider = crate::test_utils::MockProvider::new("github.com");
        let registry = crate::test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();

        let text = "      - run: echo hello\n";
        let position = Position {
            line: 0,
            character: 10,
        };

        let ctx = HoverContext {
            config: &config,
            registry: &registry,
            file_cache: &file_cache,
            meta_cache: &meta_cache,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let hover = compute_hover(text, position, None, &ctx).await;
        assert!(hover.is_none(), "expected no hover for non-uses line");
    }

    #[tokio::test]
    async fn compute_hover_shows_outdated_status() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();

        let text = "      - uses: actions/checkout@v3\n";
        let position = Position {
            line: 0,
            character: 20,
        };

        let ctx = HoverContext {
            config: &config,
            registry: &registry,
            file_cache: &file_cache,
            meta_cache: &meta_cache,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let hover = compute_hover(text, position, None, &ctx).await.unwrap();

        match &hover.contents {
            HoverContents::Markup(mc) => {
                assert!(
                    mc.value.contains("Outdated"),
                    "expected 'Outdated' in hover, got: {}",
                    mc.value
                );
                assert!(mc.value.contains("v4.2.0"));
            }
            other => panic!("expected Markup, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compute_hover_reuses_diagnostics_version_data() {
        use crate::test_utils::{self, DUMMY_SHA};

        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let calls = provider.call_count();
        let registry = test_utils::mock_registry(provider);
        let file_cache = crate::file_cache::FileCache::disabled();
        let meta_cache = new_action_meta_cache();
        let config = Config::default();
        let uri = tower_lsp_server::ls_types::Uri::from_file_path("/test/ci.yml").unwrap();
        let text = "      - uses: actions/checkout@v3\n";

        let diagnostics = crate::diagnostics::compute_diagnostics(
            text,
            &uri,
            &config,
            &registry,
            &file_cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let position = Position {
            line: 0,
            character: 20,
        };
        let ctx = HoverContext {
            config: &config,
            registry: &registry,
            file_cache: &file_cache,
            meta_cache: &meta_cache,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let hover = compute_hover(text, position, Some(&diagnostics.version_sets), &ctx).await;

        assert!(hover.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn action_meta_cache_stores_and_retrieves() {
        let cache = new_action_meta_cache();
        let key = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v4".into(),
            host: None,
        };
        let meta = Arc::new(CachedActionMeta {
            name: Some("Checkout".into()),
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        });
        cache.insert(key.clone(), Arc::clone(&meta));
        let ttl = Duration::from_secs(60);
        assert!(cache.get(&key, ttl).is_some());
        assert_eq!(
            cache.get(&key, ttl).unwrap().name.as_deref(),
            Some("Checkout")
        );
    }

    #[test]
    fn action_meta_cache_expires_entries_after_the_ttl() {
        let clock = Arc::new(crate::test_utils::StepClock::new());
        let dyn_clock: Arc<dyn Clock> = clock.clone();
        let cache = ActionMetaCache::with_clock(dyn_clock);
        let key = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "main".into(),
            host: None,
        };
        let ttl = Duration::from_secs(60);
        cache.insert(key.clone(), cache_meta("Checkout"));

        clock.advance(ttl);
        assert!(cache.get(&key, ttl).is_some());
        clock.advance(Duration::from_secs(1));
        assert!(cache.get(&key, ttl).is_none());

        cache.insert(key.clone(), cache_meta("Again"));
        clock.rewind(Duration::from_secs(1));
        assert!(cache.get(&key, ttl).is_none());
    }

    fn cache_meta(name: &str) -> Arc<CachedActionMeta> {
        Arc::new(CachedActionMeta {
            name: Some(name.into()),
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        })
    }

    #[test]
    fn action_meta_cache_key_distinguishes_refs() {
        let cache = new_action_meta_cache();
        let key_v3 = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v3".into(),
            host: None,
        };
        let key_v4 = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v4".into(),
            host: None,
        };
        let meta = Arc::new(CachedActionMeta {
            name: Some("V3".into()),
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        });
        cache.insert(key_v3.clone(), meta);
        let ttl = Duration::from_secs(60);
        assert!(cache.get(&key_v3, ttl).is_some());
        assert!(cache.get(&key_v4, ttl).is_none());
    }

    #[test]
    fn find_action_at_position_returns_none_for_empty_doc() {
        let pos = Position {
            line: 0,
            character: 0,
        };
        assert!(
            find_action_at_position("", pos, crate::position::PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn build_hover_content_handles_minimal_meta() {
        let action_ref = ActionRef::simple("test", "action", "v1");
        let meta = CachedActionMeta {
            name: None,
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        };
        let content = build_hover_content(
            &action_ref,
            Some(&meta),
            &CheckResult::new(action_ref.clone(), CheckStatus::UpToDate),
            None,
            Some("github.com"),
        );
        assert!(content.contains("### test/action"));
        assert!(content.contains("Up to date"));
    }

    #[test]
    fn action_meta_cache_key_distinguishes_hosts() {
        let cache = new_action_meta_cache();
        let key_github = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v4".into(),
            host: None,
        };
        let key_ghe = ActionMetaCacheKey {
            owner: "actions".into(),
            repo: "checkout".into(),
            ref_str: "v4".into(),
            host: Some("ghe.corp.com".into()),
        };

        let meta = Arc::new(CachedActionMeta {
            name: Some("Test".into()),
            description: None,
            author: None,
            deprecated_inputs: vec![],
            runtime: None,
        });

        cache.insert(key_github.clone(), Arc::clone(&meta));

        let ttl = Duration::from_secs(60);
        assert!(cache.get(&key_github, ttl).is_some());
        assert!(cache.get(&key_ghe, ttl).is_none());
    }
}
