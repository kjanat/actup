//! Configuration types and loading
//!
//! Configuration is loaded from multiple sources with precedence:
//! 1. Environment variables (ACTUP_*) - highest
//! 2. Explicit --config path
//! 3. Repo configs (actup.toml, .actup.toml, .config/actup.toml, etc.)
//! 4. User/global config (~/.config/actup/config.toml) - fills gaps only
//! 5. Defaults (lowest)

pub mod discovery;
pub mod matching;
pub mod registration;
pub mod resolution;
pub mod validation;

use std::{
    collections::HashMap,
    convert::Infallible,
    io::Read,
    num::NonZeroU8,
    path::{Path, PathBuf},
    time::Duration,
};

#[allow(clippy::module_name_repetitions)]
pub use discovery::find_repo_config;
pub use discovery::{CONFIG_EXTENSIONS, iter_config_candidates, user_config_path};
use figment::{
    Figment,
    providers::{Env, Format, Json, Serialized, Toml, Yaml},
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

pub use matching::ActionPattern;

use crate::{ActionRef, Error, policy};

/// # Actup configuration
/// Configuration options for the GitHub Actions-like updater.
///
/// Main configuration struct.
///
/// # Example Configuration
///
/// ```toml
/// # .actup.toml - Conservative security-focused config
/// [policy]
/// bump   = "patch"
/// pin    = "sha"
/// branch = "warn"
///
/// # Result in workflow:
/// # Before:
/// # - uses: actions/checkout@v4
/// # After:
/// # - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # v4.1.1
/// ```
///
/// ```toml
/// # .actup.toml - Permissive major-update config
/// [policy]
/// bump       = "major"
/// pin        = "tag"
/// track      = "exact"
/// prerelease = true
///
/// # Result in workflow:
/// # Before:
/// # - uses: actions/checkout@v3
/// # After:
/// # - uses: actions/checkout@v4.2.0
/// ```
///
/// ```toml
/// # .actup.toml - Selective ignore patterns and overrides
/// [scan]
/// ignore = [
///   "actions/checkout",              # Ignore all refs of this action
///   "my-org/*",                      # Ignore all actions from my-org
///   "dtolnay/rust-toolchain@1.91.0", # Ignore specific MSRV version
/// ]
///
/// [[overrides]]
/// pattern = "ghe.corp.com/platform/*"
/// policy  = { pin = "sha", bump = "minor" }
///
/// # Result: dtolnay/rust-toolchain@stable is checked, @1.91.0 is ignored
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Config {
    /// Update policy applied to every action unless an override matches.
    ///
    /// ```toml
    /// [policy]
    /// bump  = "minor"
    /// pin   = "sha"
    /// track = "keep"
    /// ```
    pub policy: Policy,

    /// Which files and actions to scan.
    ///
    /// ```toml
    /// [scan]
    /// ignore      = ["my-org/*"]
    /// extra_paths = [".gitea/workflows"]
    /// ```
    pub scan: Scan,

    /// Ordered policy overrides; for each key, the last matching entry wins.
    ///
    /// ```toml
    /// [[overrides]]
    /// pattern = "my-org/*"
    /// policy  = { pin = "sha" }
    ///
    /// [[overrides]]
    /// pattern = "my-org/legacy"
    /// policy  = { bump = "patch" }
    /// ```
    pub overrides: Vec<Override>,

    /// Version-cache settings (TTL, location, on/off).
    ///
    /// ```toml
    /// [cache]
    /// ttl = 86400        # re-check a repo at most once a day
    /// enabled = true
    /// ```
    pub cache: Cache,

    /// Host used for bare `owner/repo` refs that carry no host prefix.
    ///
    /// Absent resolves to `github.com` via the built-in preset.
    ///
    /// ```toml
    /// default_host = "github.com"
    /// ```
    pub default_host: Option<String>,

    /// API fallback settings, used only when the git protocol can't list a repo.
    ///
    /// ```toml
    /// [fetch]
    /// api_mode = "graphql"       # or "rest"
    /// max_tags_per_repo = 10
    /// ```
    pub fetch: FetchSettings,

    /// Per-host provider config, keyed by lowercase hostname.
    ///
    /// Used for GitHub Enterprise, self-hosted GitLab/Gitea, and custom tokens.
    ///
    /// ```toml
    /// [hosts."ghe.corp.internal"]
    /// kind = "github"
    /// api_base = "https://ghe.corp.internal/api/v3"
    /// token_env = "GHE_TOKEN"
    /// ```
    pub hosts: HashMap<String, HostConfig>,

    /// Path this config was loaded from (if any)
    #[serde(skip)]
    pub loaded_from: Option<PathBuf>,

    /// Rejects the renamed `update_policy` key.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 0>")]
    #[schemars(skip)]
    pub update_policy: Option<Infallible>,

    /// Rejects the renamed `ref_style` key.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 1>")]
    #[schemars(skip)]
    pub ref_style: Option<Infallible>,

    /// Rejects the renamed `branch_policy` key.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 2>")]
    #[schemars(skip)]
    pub branch_policy: Option<Infallible>,

    /// Rejects the renamed `allow_prerelease` key.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 3>")]
    #[schemars(skip)]
    pub allow_prerelease: Option<Infallible>,

    /// Rejects the renamed top-level `ignore` key.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 4>")]
    #[schemars(skip)]
    pub ignore: Option<Infallible>,

    /// Rejects the renamed top-level `extra_paths` key.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 5>")]
    #[schemars(skip)]
    pub extra_paths: Option<Infallible>,

    /// Rejects the renamed `[actions]` table.
    #[serde(skip_serializing, deserialize_with = "renamed::<_, 6>")]
    #[schemars(skip)]
    pub actions: Option<Infallible>,
}

/// The `[policy]` table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Policy {
    /// How far to bump outdated version refs.
    ///
    /// `patch` stays on `major.minor`, `minor` stays on `major`, `major`
    /// allows a new major, `latest` takes the newest stable overall.
    ///
    /// ```toml
    /// bump = "minor"   # v1.2.3 -> v1.5.0, never v2
    /// ```
    pub bump: policy::Update,

    /// How a ref is written when updating.
    ///
    /// `preserve` keeps the current style (tag stays a tag, SHA stays a SHA),
    /// `tag` rewrites to the version tag, `sha` pins to the commit SHA with a
    /// `# vX.Y.Z` comment.
    ///
    /// ```toml
    /// pin = "sha"   # actions/checkout@v4 -> @<sha> # v4.2.0
    /// ```
    pub pin: policy::Style,

    /// Version granularity of an updated tag ref.
    ///
    /// `keep` follows the current ref (`v4` -> `v6`, `v4.1.0` -> `v6.0.2`),
    /// `major` writes the major-only tag when it points at the target commit,
    /// `exact` always writes the full version.
    ///
    /// ```toml
    /// track = "exact"   # actions/checkout@v4 -> @v6.0.2
    /// ```
    pub track: policy::Track,

    /// How to handle floating branch refs like `@main` or `@v2`.
    ///
    /// `ignore` leaves them untouched (default), `warn` flags them as
    /// floating, `update` pins the branch to its current HEAD SHA.
    ///
    /// ```toml
    /// branch = "warn"   # @main -> flagged, pin to a tag or SHA
    /// ```
    pub branch: policy::Branch,

    /// Consider prerelease tags (e.g. `v2.0.0-rc.1`) when picking a target.
    ///
    /// ```toml
    /// prerelease = false   # skip -rc/-beta tags unless already on one
    /// ```
    pub prerelease: bool,
}

impl Policy {
    /// Return this policy with every key that `partial` sets replaced.
    #[must_use]
    pub fn with(self, partial: &PolicyOverride) -> Self {
        Self {
            bump: partial.bump.unwrap_or(self.bump),
            pin: partial.pin.unwrap_or(self.pin),
            track: partial.track.unwrap_or(self.track),
            branch: partial.branch.unwrap_or(self.branch),
            prerelease: partial.prerelease.unwrap_or(self.prerelease),
        }
    }
}

/// A partial `[policy]` table used by `[[overrides]]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyOverride {
    /// Replaces `policy.bump`.
    pub bump: Option<policy::Update>,
    /// Replaces `policy.pin`.
    pub pin: Option<policy::Style>,
    /// Replaces `policy.track`.
    pub track: Option<policy::Track>,
    /// Replaces `policy.branch`.
    pub branch: Option<policy::Branch>,
    /// Replaces `policy.prerelease`.
    pub prerelease: Option<bool>,
}

/// The `[scan]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Scan {
    /// Actions to skip, as `[host/]owner/repo[/subpath][@ref]` globs.
    ///
    /// Examples:
    /// - `"actions/checkout"` - ignores all refs of this action
    /// - `"actions/checkout@v2"` - ignores only the v2 ref
    /// - `"my-org/*"` - ignores all actions from my-org
    /// - `"ghe.corp.com/*/*"` - ignores every action on one host
    /// - `"tool/action@1.91.0"` - ignores specific version (e.g., MSRV pin)
    #[schemars(with = "Vec<String>")]
    pub ignore: Vec<ActionPattern>,

    /// Additional paths to scan (beyond defaults)
    ///
    /// By default, actup scans:
    /// - `.github/workflows/**/*.{yml,yaml}` (workflows)
    /// - `.github/actions/**/action.{yml,yaml}` (composite actions)
    ///
    /// Use `extra_paths` to scan additional locations:
    ///
    /// Examples:
    /// - `".gitea/workflows"` - Gitea workflow directory
    /// - `".woodpecker/*.yml"` - Woodpecker CI pipelines
    /// - `"ci/pipelines/**/*.yaml"` - Custom CI directory
    /// - `".drone.yml"` - Specific Drone CI config file
    /// - `"infra/terraform/github-actions.tf"` - GitHub Actions in Terraform
    pub extra_paths: Vec<String>,
}

/// One `[[overrides]]` entry.
///
/// ## Example
/// ```toml
/// [[overrides]]
/// pattern = "actions/*"
/// policy  = { bump = "major" }
///
/// [[overrides]]
/// pattern = "ghe.corp.com/internal/*"
/// policy  = { pin = "sha", track = "exact" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// Actions this entry applies to, as a `[host/]owner/repo[/subpath][@ref]` glob.
    #[schemars(with = "String")]
    pub pattern: ActionPattern,

    /// Policy keys to replace for matching actions.
    #[serde(default)]
    pub policy: PolicyOverride,
}

const RENAMED_KEYS: [(&str, &str); 7] = [
    ("update_policy", "policy.bump"),
    ("ref_style", "policy.pin"),
    ("branch_policy", "policy.branch"),
    ("allow_prerelease", "policy.prerelease"),
    ("ignore", "scan.ignore"),
    ("extra_paths", "scan.extra_paths"),
    ("actions", "[[overrides]]"),
];

fn renamed<'de, D, const KEY: usize>(_: D) -> Result<Option<Infallible>, D::Error>
where
    D: Deserializer<'de>,
{
    let (old, new) = RENAMED_KEYS[KEY];
    Err(de::Error::custom(format_args!(
        "`{old}` is no longer supported; use `{new}`"
    )))
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Cache {
    /// Cache time-to-live in seconds
    #[serde(
        serialize_with = "serialize_duration_secs",
        deserialize_with = "deserialize_duration_secs"
    )]
    #[schemars(with = "u64")]
    pub ttl: Duration,

    /// Custom cache directory (None = use OS default)
    pub path: Option<PathBuf>,

    /// Whether caching is enabled
    pub enabled: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            ttl: Duration::from_hours(24),
            path: None,
            enabled: true,
        }
    }
}

/// Serialize Duration as seconds (u64)
fn serialize_duration_secs<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_u64(duration.as_secs())
}

/// Deserialize Duration from seconds (u64)
fn deserialize_duration_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

/// API mode for fetching versions
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApiMode {
    /// One batched GraphQL query for all repos. Fast, but needs an auth token.
    ///
    /// ```toml
    /// [fetch]
    /// api_mode = "graphql"
    /// ```
    #[default]
    Graphql,
    /// Parallel REST requests, one per repo. Works without a token.
    ///
    /// ```toml
    /// [fetch]
    /// api_mode = "rest"
    /// ```
    Rest,
}

impl std::fmt::Display for ApiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graphql => write!(f, "graphql"),
            Self::Rest => write!(f, "rest"),
        }
    }
}

/// Provider kind identifiers.
///
/// Each variant corresponds to a provider crate that implements the [`Provider`](crate::Provider)
/// trait. Used for cache namespacing, registration lookup, and config validation.
///
/// Adding a new provider requires adding a variant here **and** a provider crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    /// GitHub / GitHub Enterprise
    GitHub,
    /// GitLab (gitlab.com or self-hosted)
    GitLab,
    /// Gitea / Forgejo
    Gitea,
}

impl std::fmt::Display for HostKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitHub => write!(f, "github"),
            Self::GitLab => write!(f, "gitlab"),
            Self::Gitea => write!(f, "gitea"),
        }
    }
}

/// Global fetch settings applied to all providers unless overridden per-host.
///
/// Versions come from the git wire protocol, which needs no API and lists every
/// tag. These settings govern the HTTP API fallback used when a host refuses a
/// git connection. Per-host overrides in `HostConfig` take precedence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
    /// API fallback mode: `"graphql"` (batched, requires auth) or `"rest"` (parallel)
    pub api_mode: ApiMode,

    /// Maximum tags the API fallback fetches per repository (1–255).
    pub max_tags_per_repo: NonZeroU8,
}

impl Default for FetchSettings {
    fn default() -> Self {
        Self {
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: DEFAULT_MAX_TAGS_PER_REPO,
        }
    }
}

/// Per-host configuration for a single provider host.
///
/// All fields are optional: they act as overrides on top of provider preset
/// defaults and global `[fetch]` settings. After resolution, `kind`, `api_base`,
/// and `token_env` must all be present (validated post-merge).
///
/// Used as values in the `[hosts."hostname"]` config map.
#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Provider kind. Required for non-preset hosts; implied by preset for known hosts.
    pub kind: Option<HostKind>,

    /// API base URL. Required after merge (may be inferred by provider registration).
    /// Must include scheme (`http://` or `https://`), no query or fragment.
    pub api_base: Option<String>,

    /// Env var name for auth token. Required after merge (may be defaulted by preset).
    pub token_env: Option<String>,

    /// Override global `[fetch].api_mode` for this host.
    pub api_mode: Option<ApiMode>,

    /// Override global `[fetch].max_tags_per_repo` for this host.
    pub max_tags_per_repo: Option<NonZeroU8>,
}

/// Default maximum tags to fetch per repository
pub const DEFAULT_MAX_TAGS_PER_REPO: NonZeroU8 = match NonZeroU8::new(10) {
    Some(v) => v,
    None => panic!("DEFAULT_MAX_TAGS_PER_REPO must be non-zero"),
};

impl Config {
    /// Create a new config with defaults
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load config with full layering support
    ///
    /// Precedence (highest to lowest):
    /// 1. Environment variables (ACTUP_*)
    /// 2. Explicit `config_path` (if provided)
    /// 3. Repo config (searched upward from `start_dir`)
    /// 4. User config (~/.config/actup/config.toml) - fills gaps only
    /// 5. Defaults
    ///
    /// # Errors
    /// Returns error if config file exists but is invalid.
    #[allow(clippy::option_if_let_else)] // Readability: mutating figment in if-let is clearer
    pub fn load(config_path: Option<&Path>, start_dir: &Path) -> Result<Self, Error> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));

        // User config fills gaps (lowest precedence file)
        if let Some(user_config) = user_config_path()
            && user_config.exists()
        {
            figment = figment.join(Toml::file(&user_config));
        }

        // Repo config (searched upward from `start_dir`)
        let loaded_from = if let Some(repo_config) = find_repo_config(start_dir) {
            figment = Self::merge_config_file(figment, &repo_config)?;
            Some(repo_config)
        } else {
            None
        };

        // Explicit --config path (high precedence)
        let loaded_from = if let Some(path) = config_path {
            figment = Self::merge_config_file(figment, path)?;
            Some(path.to_path_buf())
        } else {
            loaded_from
        };

        // Environment variables (highest precedence)
        figment = figment.merge(Env::prefixed("ACTUP_").split("_"));

        let mut config: Self = figment.extract().map_err(|e| Error::InvalidConfig {
            path: error_origin(&e, loaded_from.as_deref()),
            reason: e.to_string(),
        })?;

        config.loaded_from = loaded_from;
        Ok(config)
    }

    /// Merge a config file into figment based on extension
    fn merge_config_file(figment: Figment, path: &Path) -> Result<Figment, Error> {
        let ext = path.extension().and_then(|e| e.to_str());

        Ok(match ext {
            Some("yaml" | "yml") => figment.merge(Yaml::file(path)),
            Some("json") => figment.merge(Json::file(path)),
            Some("jsonc") => {
                // Read file, strip comments, parse as JSON
                let content = std::fs::read_to_string(path).map_err(|e| Error::InvalidConfig {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;

                let mut stripped = String::new();
                json_comments::StripComments::new(content.as_bytes())
                    .read_to_string(&mut stripped)
                    .map_err(|e| Error::InvalidConfig {
                        path: path.display().to_string(),
                        reason: format!("failed to strip comments: {e}"),
                    })?;

                figment.merge(JsoncFile {
                    path: path.to_path_buf(),
                    json: stripped,
                })
            }
            // Default to TOML for .toml or unknown extensions
            _ => figment.merge(Toml::file(path)),
        })
    }

    /// Return the policy for `action`: `[policy]` with every matching override applied in order.
    #[must_use]
    pub fn policy_for(&self, action: &ActionRef) -> Policy {
        let default_host = self.pattern_default_host();
        self.overrides
            .iter()
            .filter(|o| o.pattern.matches(action, default_host))
            .fold(self.policy, |policy, o| policy.with(&o.policy))
    }

    /// Return whether `action` matches a `scan.ignore` pattern.
    #[must_use]
    pub fn should_ignore(&self, action: &ActionRef) -> bool {
        let default_host = self.pattern_default_host();
        self.scan
            .ignore
            .iter()
            .any(|p| p.matches(action, default_host))
    }

    fn pattern_default_host(&self) -> &str {
        self.default_host
            .as_deref()
            .unwrap_or(resolution::FALLBACK_DEFAULT_HOST)
    }
}

struct JsoncFile {
    path: PathBuf,
    json: String,
}

impl figment::Provider for JsoncFile {
    fn metadata(&self) -> figment::Metadata {
        figment::Metadata::from("JSONC file", self.path.as_path())
    }

    fn data(
        &self,
    ) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        figment::Provider::data(&Json::string(&self.json))
    }
}

fn error_origin(error: &figment::Error, loaded_from: Option<&Path>) -> String {
    match error.metadata.as_ref().map(|md| (&md.source, &md.name)) {
        Some((Some(figment::Source::File(path)), _)) => path.display().to_string(),
        Some((None, name)) => name.to_string(),
        _ => loaded_from.map_or_else(|| "config".into(), |p| p.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(uses: &str) -> ActionRef {
        let Ok(Some(action)) = crate::parse_uses(uses) else {
            panic!("bad uses: {uses}");
        };
        action
    }

    fn patterns(raw: &[&str]) -> Result<Vec<ActionPattern>, String> {
        raw.iter().map(|p| p.parse()).collect()
    }

    // Config defaults tests

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.policy.bump, policy::Update::Latest);
        assert_eq!(config.policy.pin, policy::Style::Preserve);
        assert_eq!(config.policy.track, policy::Track::Keep);
        assert_eq!(config.policy.branch, policy::Branch::Ignore);
        assert!(!config.policy.prerelease);
        assert_eq!(config.scan.ignore, Vec::<ActionPattern>::new());
        assert_eq!(config.scan.extra_paths, Vec::<String>::new());
        assert_eq!(config.overrides, Vec::<Override>::new());
        assert!(config.loaded_from.is_none());
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 10);
        assert!(config.hosts.is_empty());
    }

    // should_ignore tests

    #[test]
    fn test_should_ignore() -> Result<(), String> {
        let config = Config {
            scan: Scan {
                ignore: patterns(&["actions/checkout", "my-org/*"])?,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(config.should_ignore(&action("actions/checkout@v1")));
        assert!(config.should_ignore(&action("my-org/anything@v2")));
        assert!(!config.should_ignore(&action("other/repo@v1")));
        Ok(())
    }

    #[test]
    fn test_should_ignore_empty() {
        let config = Config::default();
        assert!(!config.should_ignore(&action("any/repo@v1")));
    }

    #[test]
    fn test_should_ignore_specific_ref() -> Result<(), String> {
        let config = Config {
            scan: Scan {
                ignore: patterns(&["owner/repo@v1.91.0", "another/action@main"])?,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(config.should_ignore(&action("owner/repo@v1.91.0")));
        assert!(config.should_ignore(&action("another/action@main")));
        assert!(!config.should_ignore(&action("owner/repo@v2")));
        assert!(!config.should_ignore(&action("owner/repo@stable")));
        assert!(!config.should_ignore(&action("another/action@v1")));
        Ok(())
    }

    #[test]
    fn test_should_ignore_host_qualified_uses_default_host() -> Result<(), String> {
        let mut config = Config {
            scan: Scan {
                ignore: patterns(&["ghe.corp.com/platform/*"])?,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(config.should_ignore(&action("ghe.corp.com/platform/ci@v1")));
        assert!(!config.should_ignore(&action("platform/ci@v1")));
        config.default_host = Some("ghe.corp.com".into());
        assert!(config.should_ignore(&action("platform/ci@v1")));
        Ok(())
    }

    // policy_for tests

    fn override_entry(pattern: &str, policy: PolicyOverride) -> Result<Override, String> {
        Ok(Override {
            pattern: pattern.parse()?,
            policy,
        })
    }

    #[test]
    fn test_policy_for_uses_default_when_no_override_matches() -> Result<(), String> {
        let config = Config {
            policy: Policy {
                bump: policy::Update::Major,
                ..Default::default()
            },
            overrides: vec![override_entry(
                "special/*",
                PolicyOverride {
                    bump: Some(policy::Update::Patch),
                    ..Default::default()
                },
            )?],
            ..Default::default()
        };

        assert_eq!(config.policy_for(&action("any/action@v1")), config.policy);
        Ok(())
    }

    #[test]
    fn test_policy_for_last_match_wins_per_key() -> Result<(), String> {
        let config = Config {
            policy: Policy {
                bump: policy::Update::Minor,
                ..Default::default()
            },
            overrides: vec![
                override_entry(
                    "my-org/*",
                    PolicyOverride {
                        bump: Some(policy::Update::Major),
                        pin: Some(policy::Style::Sha),
                        ..Default::default()
                    },
                )?,
                override_entry(
                    "my-org/legacy",
                    PolicyOverride {
                        bump: Some(policy::Update::Patch),
                        track: Some(policy::Track::Exact),
                        ..Default::default()
                    },
                )?,
                override_entry(
                    "other/*",
                    PolicyOverride {
                        prerelease: Some(true),
                        ..Default::default()
                    },
                )?,
            ],
            ..Default::default()
        };

        let legacy = config.policy_for(&action("my-org/legacy@v1"));
        assert_eq!(legacy.bump, policy::Update::Patch);
        assert_eq!(legacy.pin, policy::Style::Sha);
        assert_eq!(legacy.track, policy::Track::Exact);
        assert!(!legacy.prerelease);

        let tool = config.policy_for(&action("my-org/tool@v1"));
        assert_eq!(tool.bump, policy::Update::Major);
        assert_eq!(tool.track, policy::Track::Keep);

        let reordered = Config {
            overrides: config.overrides.iter().rev().cloned().collect(),
            ..config.clone()
        };
        assert_eq!(
            reordered.policy_for(&action("my-org/legacy@v1")).bump,
            policy::Update::Major
        );
        Ok(())
    }

    #[test]
    fn test_policy_for_host_qualified_override() -> Result<(), String> {
        let config = Config {
            overrides: vec![override_entry(
                "ghe.corp.com/platform/*",
                PolicyOverride {
                    branch: Some(policy::Branch::Warn),
                    ..Default::default()
                },
            )?],
            ..Default::default()
        };

        assert_eq!(
            config
                .policy_for(&action("ghe.corp.com/platform/ci@main"))
                .branch,
            policy::Branch::Warn
        );
        assert_eq!(
            config.policy_for(&action("platform/ci@main")).branch,
            policy::Branch::Ignore
        );
        Ok(())
    }

    // Cache tests

    #[test]
    fn test_cache_config_default() {
        let cache = Cache::default();
        assert!(cache.enabled);
        assert_eq!(cache.ttl, Duration::from_hours(24));
        assert!(cache.path.is_none());
    }

    #[test]
    fn test_cache_config_custom_ttl() {
        let cache = Cache {
            ttl: Duration::from_hours(12),
            ..Default::default()
        };
        assert_eq!(cache.ttl, Duration::from_hours(12));
    }

    // HostKind tests

    #[test]
    fn test_hostkind_serde_roundtrip() {
        #[allow(clippy::unwrap_used)]
        for (variant, expected_str) in [
            (HostKind::GitHub, "\"github\""),
            (HostKind::GitLab, "\"gitlab\""),
            (HostKind::Gitea, "\"gitea\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");

            let back: HostKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn test_hostkind_display() {
        assert_eq!(HostKind::GitHub.to_string(), "github");
        assert_eq!(HostKind::GitLab.to_string(), "gitlab");
        assert_eq!(HostKind::Gitea.to_string(), "gitea");
    }

    #[test]
    fn test_hostkind_rejects_unknown() {
        let result = serde_json::from_str::<HostKind>("\"bitbucket\"");
        assert!(result.is_err());
    }

    #[test]
    fn test_hostkind_case_sensitive() {
        // serde rename_all = "lowercase" means uppercase should fail
        let result = serde_json::from_str::<HostKind>("\"Github\"");
        assert!(result.is_err());

        let result = serde_json::from_str::<HostKind>("\"GITHUB\"");
        assert!(result.is_err());
    }

    #[test]
    fn test_hostkind_hashable() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(HostKind::GitHub);
        set.insert(HostKind::GitLab);
        set.insert(HostKind::Gitea);
        set.insert(HostKind::GitHub); // duplicate
        assert_eq!(set.len(), 3);
    }

    // FetchSettings tests

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_default() {
        let fs = FetchSettings::default();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_roundtrip_defaults() {
        let fs = FetchSettings::default();
        let json = serde_json::to_string(&fs).unwrap();
        let back: FetchSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_mode, ApiMode::Graphql);
        assert_eq!(back.max_tags_per_repo.get(), 10);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_custom_values() {
        let json = r#"{"api_mode":"rest","max_tags_per_repo":50}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 50);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_partial_uses_defaults() {
        // Only api_mode specified — max_tags_per_repo should default
        let json = r#"{"api_mode":"rest"}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 10);

        // Only max_tags_per_repo — api_mode should default
        let json = r#"{"max_tags_per_repo":25}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 25);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_empty_uses_defaults() {
        let json = "{}";
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[test]
    fn test_fetch_settings_rejects_zero_max_tags() {
        let json = r#"{"max_tags_per_repo":0}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_fetch_settings_deny_unknown_fields() {
        let json = r#"{"api_mode":"graphql","unknown_field":true}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_max_tags_boundary() {
        // NonZeroU8 max = 255
        let json = r#"{"max_tags_per_repo":255}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.max_tags_per_repo.get(), 255);

        // 256 exceeds u8 range
        let json = r#"{"max_tags_per_repo":256}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
    }

    // HostConfig tests

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_all_fields() {
        let json = r#"{
            "kind": "github",
            "api_base": "https://ghe.corp.com/api/v3",
            "token_env": "GHE_TOKEN",
            "api_mode": "rest",
            "max_tags_per_repo": 50
        }"#;
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, Some(HostKind::GitHub));
        assert_eq!(hc.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(hc.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(50));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_no_fields() {
        let json = "{}";
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, None);
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        assert_eq!(hc.api_mode, None);
        assert_eq!(hc.max_tags_per_repo, None);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_partial_kind_only() {
        let json = r#"{"kind": "gitlab"}"#;
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, Some(HostKind::GitLab));
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        assert_eq!(hc.api_mode, None);
        assert_eq!(hc.max_tags_per_repo, None);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_partial_fetch_overrides_only() {
        let json = r#"{"api_mode": "rest", "max_tags_per_repo": 25}"#;
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, None);
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(25));
    }

    #[test]
    fn test_hostconfig_deny_unknown_fields() {
        let json = r#"{"kind": "github", "bogus": true}"#;
        let result = serde_json::from_str::<HostConfig>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_rejects_invalid_kind() {
        let json = r#"{"kind": "bitbucket"}"#;
        let result = serde_json::from_str::<HostConfig>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_rejects_zero_max_tags() {
        let json = r#"{"max_tags_per_repo": 0}"#;
        let result = serde_json::from_str::<HostConfig>(json);
        assert!(result.is_err());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_serde_roundtrip() {
        let hc = HostConfig {
            kind: Some(HostKind::Gitea),
            api_base: Some("https://gitea.local/api/v1".into()),
            token_env: Some("GITEA_TOKEN".into()),
            api_mode: Some(ApiMode::Rest),
            max_tags_per_repo: NonZeroU8::new(100),
        };
        let json = serde_json::to_string(&hc).unwrap();
        let back: HostConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, hc.kind);
        assert_eq!(back.api_base, hc.api_base);
        assert_eq!(back.token_env, hc.token_env);
        assert_eq!(back.api_mode, hc.api_mode);
        assert_eq!(back.max_tags_per_repo, hc.max_tags_per_repo);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_each_kind_variant() {
        for (variant, name) in [
            (HostKind::GitHub, "github"),
            (HostKind::GitLab, "gitlab"),
            (HostKind::Gitea, "gitea"),
        ] {
            let json = format!(r#"{{"kind": "{name}"}}"#);
            let hc: HostConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(hc.kind, Some(variant), "kind={name}");
        }
    }

    // TOML deserialization tests (realistic config fragments)

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostkind_toml_deserialization() {
        #[derive(Deserialize)]
        struct Wrapper {
            kind: HostKind,
        }
        for (variant, name) in [
            (HostKind::GitHub, "github"),
            (HostKind::GitLab, "gitlab"),
            (HostKind::Gitea, "gitea"),
        ] {
            let toml_str = format!("kind = \"{name}\"");
            let w: Wrapper = toml::from_str(&toml_str).unwrap();
            assert_eq!(w.kind, variant, "TOML kind={name}");
        }
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_toml_deserialization() {
        let toml_str = r#"
            api_mode = "rest"
            max_tags_per_repo = 42
        "#;
        let fs: FetchSettings = toml::from_str(toml_str).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 42);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_toml_empty_uses_defaults() {
        let fs: FetchSettings = toml::from_str("").unwrap();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_toml_full() {
        let toml_str = r#"
            kind              = "github"
            api_base          = "https://ghe.corp.com/api/v3"
            token_env         = "GHE_TOKEN"
            api_mode          = "rest"
            max_tags_per_repo = 50
        "#;
        let hc: HostConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(hc.kind, Some(HostKind::GitHub));
        assert_eq!(hc.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(hc.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(50));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_toml_minimal() {
        let toml_str = r#"kind = "gitea""#;
        let hc: HostConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(hc.kind, Some(HostKind::Gitea));
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
    }

    #[test]
    fn test_hostconfig_toml_deny_unknown_fields() {
        let toml_str = r#"
            kind = "github"
            typo_field = "oops"
        "#;
        let result = toml::from_str::<HostConfig>(toml_str);
        assert!(result.is_err());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_toml_in_hosts_map() {
        // Test as it would appear in actual config: [hosts."hostname"]
        #[derive(Deserialize)]
        struct Outer {
            hosts: HashMap<String, HostConfig>,
        }
        let toml_str = r#"
            [hosts."ghe.corp.com"]
            kind      = "github"
            api_base  = "https://ghe.corp.com/api/v3"
            token_env = "GHE_TOKEN"

            [hosts."gitlab.internal"]
            kind      = "gitlab"
            api_base  = "https://gitlab.internal/api/v4"
            token_env = "GITLAB_TOKEN"
            api_mode  = "rest"
        "#;
        let outer: Outer = toml::from_str(toml_str).unwrap();
        assert_eq!(outer.hosts.len(), 2);

        let ghe = &outer.hosts["ghe.corp.com"];
        assert_eq!(ghe.kind, Some(HostKind::GitHub));
        assert_eq!(ghe.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(ghe.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(ghe.api_mode, None);

        let gl = &outer.hosts["gitlab.internal"];
        assert_eq!(gl.kind, Some(HostKind::GitLab));
        assert_eq!(gl.api_mode, Some(ApiMode::Rest));
    }

    // Config-level TOML tests (new fields)

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_new_fields() {
        let toml_str = r#"
            default_host = "github.com"

            [fetch]
            api_mode          = "rest"
            max_tags_per_repo = 42

            [hosts."ghe.corp.com"]
            kind      = "github"
            api_base  = "https://ghe.corp.com/api/v3"
            token_env = "GHE_TOKEN"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_host.as_deref(), Some("github.com"));
        assert_eq!(config.fetch.api_mode, ApiMode::Rest);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 42);
        assert_eq!(config.hosts.len(), 1);
        let ghe = &config.hosts["ghe.corp.com"];
        assert_eq!(ghe.kind, Some(HostKind::GitHub));
        assert_eq!(ghe.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_empty_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 10);
        assert!(config.hosts.is_empty());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_only_fetch() {
        let toml_str = r#"
            [fetch]
            api_mode = "graphql"
            max_tags_per_repo = 100
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 100);
        assert!(config.hosts.is_empty());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_multiple_hosts() {
        let toml_str = r#"
            default_host = "ghe.corp.com"

            [hosts."ghe.corp.com"]
            kind      = "github"
            api_base  = "https://ghe.corp.com/api/v3"
            token_env = "GHE_TOKEN"
            api_mode  = "rest"

            [hosts."gitlab.internal"]
            kind      = "gitlab"
            api_base  = "https://gitlab.internal/api/v4"
            token_env = "GITLAB_TOKEN"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_host.as_deref(), Some("ghe.corp.com"));
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts["ghe.corp.com"].api_mode, Some(ApiMode::Rest));
        assert_eq!(config.hosts["gitlab.internal"].kind, Some(HostKind::GitLab));
    }

    #[test]
    fn test_config_toml_policy_scan_and_overrides() -> Result<(), Box<dyn std::error::Error>> {
        let toml_str = r#"
            [policy]
            bump       = "minor"
            pin        = "sha"
            track      = "major"
            branch     = "warn"
            prerelease = true

            [scan]
            ignore      = ["my-org/*", "owner/repo@v1.91.0"]
            extra_paths = [".gitea/workflows"]

            [[overrides]]
            pattern = "actions/*"
            policy  = { bump = "patch", track = "exact" }

            [[overrides]]
            pattern = "ghe.corp.com/platform/ci"
            policy  = { pin = "tag", branch = "update", prerelease = false }
        "#;
        let config: Config = toml::from_str(toml_str)?;
        assert_eq!(
            config.policy,
            Policy {
                bump: policy::Update::Minor,
                pin: policy::Style::Sha,
                track: policy::Track::Major,
                branch: policy::Branch::Warn,
                prerelease: true,
            }
        );
        let ignore: Vec<&str> = config
            .scan
            .ignore
            .iter()
            .map(ActionPattern::as_str)
            .collect();
        assert_eq!(ignore, ["my-org/*", "owner/repo@v1.91.0"]);
        assert_eq!(config.scan.extra_paths, [".gitea/workflows"]);
        assert_eq!(config.overrides.len(), 2);
        assert_eq!(config.overrides[0].pattern.as_str(), "actions/*");
        assert_eq!(
            config.overrides[0].policy,
            PolicyOverride {
                bump: Some(policy::Update::Patch),
                track: Some(policy::Track::Exact),
                ..Default::default()
            }
        );
        assert_eq!(
            config.overrides[1].policy,
            PolicyOverride {
                pin: Some(policy::Style::Tag),
                branch: Some(policy::Branch::Update),
                prerelease: Some(false),
                ..Default::default()
            }
        );
        Ok(())
    }

    #[test]
    fn test_config_toml_rejects_bad_override() {
        for toml_str in [
            "[[overrides]]\npattern = \"checkout\"",
            "[[overrides]]\npattern = \"a/b\"\npolicy = { update_policy = \"patch\" }",
            "[[overrides]]\npattern = \"a/b\"\nignore = true",
            "[[overrides]]\npolicy = { bump = \"patch\" }",
            "[scan]\nignore = [\"checkout\"]",
        ] {
            assert!(toml::from_str::<Config>(toml_str).is_err(), "{toml_str}");
        }
    }

    #[test]
    fn test_config_serializes_new_shape() -> Result<(), Box<dyn std::error::Error>> {
        let config: Config = toml::from_str(
            "[scan]\nignore = [\"a/*\"]\n[[overrides]]\npattern = \"a/b\"\npolicy = { pin = \"sha\" }",
        )?;
        let back: Config = toml::from_str(&toml::to_string(&config)?)?;
        assert_eq!(back.scan, config.scan);
        assert_eq!(back.overrides, config.overrides);
        assert_eq!(back.policy, config.policy);
        Ok(())
    }

    const RENAMED_KEY_SAMPLES: [(&str, &str, &str); 7] = [
        ("update_policy", "policy.bump", "update_policy = \"minor\""),
        ("ref_style", "policy.pin", "ref_style = \"sha\""),
        ("branch_policy", "policy.branch", "branch_policy = \"warn\""),
        (
            "allow_prerelease",
            "policy.prerelease",
            "allow_prerelease = true",
        ),
        ("ignore", "scan.ignore", "ignore = [\"a/b\"]"),
        ("extra_paths", "scan.extra_paths", "extra_paths = []"),
        (
            "actions",
            "[[overrides]]",
            "[actions.\"a/b\"]\nignore = true",
        ),
    ];

    #[test]
    fn test_deserialize_rejects_renamed_keys_naming_the_new_key()
    -> Result<(), Box<dyn std::error::Error>> {
        for (old, new, toml_str) in RENAMED_KEY_SAMPLES {
            let message = format!("`{old}` is no longer supported; use `{new}`");

            let Err(error) = toml::from_str::<Config>(toml_str) else {
                panic!("{old}: toml must reject it");
            };
            assert!(error.to_string().contains(&message), "{old}: {error}");

            let json = serde_json::to_string(&toml::from_str::<toml::Table>(toml_str)?)?;
            let Err(error) = serde_json::from_str::<Config>(&json) else {
                panic!("{old}: json must reject it");
            };
            assert!(error.to_string().contains(&message), "{old}: {error}");

            let figment = Figment::new()
                .merge(Serialized::defaults(Config::default()))
                .merge(Toml::string(toml_str));
            let Err(error) = figment.extract::<Config>() else {
                panic!("{old}: figment must reject it");
            };
            assert_eq!(error.kind.to_string(), message);
            assert_eq!(error.path, [old]);
        }
        assert_eq!(
            RENAMED_KEYS,
            RENAMED_KEY_SAMPLES.map(|(old, new, _)| (old, new))
        );
        Ok(())
    }

    #[test]
    fn test_error_origin_falls_back_for_sources_without_a_file() {
        let Err(error) = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Serialized::default("policy.bump", "sideways"))
            .extract::<Config>()
        else {
            panic!("invalid bump must be rejected");
        };
        assert_eq!(
            error_origin(&error, Some(Path::new("/repo/actup.toml"))),
            "/repo/actup.toml"
        );
        assert_eq!(error_origin(&error, None), "config");
    }

    #[test]
    fn test_deserialize_accepts_new_shape_without_renamed_keys() -> Result<(), toml::de::Error> {
        let config: Config = toml::from_str(
            "[policy]\nbump = \"minor\"\ntrack = \"MAJOR\"\n[scan]\nignore = [\"a/b\"]\nextra_paths = []",
        )?;
        assert_eq!(config.policy.bump, policy::Update::Minor);
        assert_eq!(config.policy.track, policy::Track::Major);
        assert!(config.update_policy.is_none());
        assert!(config.ignore.is_none());
        assert!(config.actions.is_none());
        Ok(())
    }
}
