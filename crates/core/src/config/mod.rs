//! Configuration types and loading
//!
//! Configuration is loaded from multiple sources with precedence:
//! 1. Environment variables (ACTUP_*) - highest
//! 2. Explicit --config path
//! 3. Repo configs (actup.toml, .actup.toml, .config/actup.toml, etc.), up to the repository root
//! 4. User/global config ([`user_config_path`])
//! 5. Defaults (lowest)

pub mod discovery;
mod env;
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

use discovery::Format;
#[allow(clippy::module_name_repetitions)]
pub use discovery::find_repo_config;
pub use discovery::{
    CONFIG_EXTENSIONS, Candidate, iter_config_candidates, repo_candidates, repo_search_dirs,
    user_config_path,
};
pub use env::{CLI_FLAG_VARS, CONFIG_VAR, EnvOverrides, FORMAT_VAR, STRICT_VAR, VERBOSE_VAR};
use figment::{
    Figment,
    providers::{Format as _, Json, Serialized, Toml, Yaml},
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

pub use matching::ActionPattern;

use crate::{ActionRef, Error, discovery::ExtraPath, policy};

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
#[serde(default, deny_unknown_fields)]
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

    /// Every layer that contributed to this config, lowest precedence first.
    #[serde(skip)]
    pub sources: Vec<Source>,

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

/// One layer that contributed to a loaded [`Config`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// The user config file.
    User {
        /// Path of the file.
        path: PathBuf,
    },
    /// The repo config file found by discovery.
    Repo {
        /// Path of the file.
        path: PathBuf,
    },
    /// The file named by `--config`, `ACTUP_CONFIG` or the LSP `configPath`.
    Explicit {
        /// Path of the file.
        path: PathBuf,
    },
    /// `ACTUP_*` environment variables that set config keys.
    Env {
        /// Variable names, sorted.
        vars: Vec<String>,
    },
}

impl Source {
    /// Return the file path of a file source.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::User { path } | Self::Repo { path } | Self::Explicit { path } => Some(path),
            Self::Env { .. } => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User { path } => write!(f, "user config {}", path.display()),
            Self::Repo { path } => write!(f, "repo config {}", path.display()),
            Self::Explicit { path } => write!(f, "explicit config {}", path.display()),
            Self::Env { vars } => write!(f, "environment {}", vars.join(", ")),
        }
    }
}

/// The `[policy]` table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// How far to bump outdated version refs.
    ///
    /// `patch` stays on `major.minor`, `minor` stays on `major`, `latest`
    /// takes the newest version overall, and `major` is an alias of `latest`.
    ///
    /// ```toml
    /// bump = "minor"   # v1.2.3 -> v1.5.0, never v2
    /// ```
    pub bump: policy::Update,

    /// How a ref is written when updating.
    ///
    /// `preserve` keeps the current style (tag stays a tag, SHA stays a SHA),
    /// `tag` rewrites to the version tag, `sha` pins to the commit SHA with a
    /// `# vX.Y.Z` comment. `update`, LSP quick fixes and fix-all apply it to
    /// outdated refs.
    ///
    /// **`tag` unpins:** an outdated SHA pin becomes its target's mutable tag.
    ///
    /// ```toml
    /// pin = "sha"   # actions/checkout@v4 -> @<sha> # v4.2.0
    /// ```
    pub pin: policy::Style,

    /// Version granularity of an updated tag ref.
    ///
    /// `keep` follows the current ref (`v4` -> `v6`, `v4.1.0` -> `v6.0.2`)
    /// and writes the full version when the major-only tag lags the target
    /// commit, `major` writes the major-only tag when it points at the target commit
    /// and `bump` allows that whole major, `exact` always writes the full
    /// version.
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
    /// prerelease = false   # skip -rc/-beta tags unless already on one of that version
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
#[serde(default, deny_unknown_fields)]
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
    ///
    /// A matched file is read by its extension. `.yml` and `.yaml` files are
    /// read as YAML. Markdown files (`.md`, `.markdown`) are read through
    /// their fenced `yaml` or `yml` blocks. Every other file is read line by
    /// line for `uses:` values that contain `@`, and actup never appends a
    /// version comment to such a line.
    ///
    /// Each entry is relative to the scan root. Absolute paths and `..`
    /// segments are rejected, and matches that resolve outside the scan root
    /// through a symlink are skipped.
    #[schemars(with = "Vec<String>")]
    pub extra_paths: Vec<ExtraPath>,
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
#[serde(default, deny_unknown_fields)]
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
    /// 2. Explicit `config_path` (if provided), relative to the process working directory
    /// 3. Repo config, searched upward from `start_dir` by [`repo_search_dirs`] with `ceiling`
    /// 4. User config at `user_config`, usually [`user_config_path`]
    /// 5. Defaults
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`] if a config file is invalid or `config_path` cannot be read.
    pub fn load(
        config_path: Option<&Path>,
        user_config: Option<&Path>,
        start_dir: &Path,
        ceiling: Option<&Path>,
    ) -> Result<Self, Error> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        let mut sources = Vec::new();

        if let Some(user_config) = user_config
            && user_config.exists()
        {
            figment = figment.merge(Toml::file(user_config));
            sources.push(Source::User {
                path: user_config.to_path_buf(),
            });
        }

        if let Some(repo_config) = find_repo_config(start_dir, ceiling) {
            figment = Self::merge_config_file(figment, &repo_config)?;
            sources.push(Source::Repo { path: repo_config });
        }

        if let Some(path) = config_path {
            figment = Self::merge_config_file(figment, path)?;
            sources.push(Source::Explicit {
                path: path.to_path_buf(),
            });
        }

        let env = EnvOverrides::from_vars(std::env::vars_os().map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        }));
        Self::extract(figment, sources, env)
    }

    /// Parse one repo config `file`, given as path and contents, over the defaults and apply `env`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`] for an invalid value.
    pub fn from_contents(file: Option<(&Path, &str)>, env: EnvOverrides) -> Result<Self, Error> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        let mut sources = Vec::new();
        if let Some((path, contents)) = file {
            figment = figment.merge(FileContents::new(path, contents)?);
            sources.push(Source::Repo {
                path: path.to_path_buf(),
            });
        }
        Self::extract(figment, sources, env)
    }

    fn extract(
        figment: Figment,
        mut sources: Vec<Source>,
        env: EnvOverrides,
    ) -> Result<Self, Error> {
        let vars = env.names();
        let figment = figment.merge(env);
        let mut config: Self = figment.extract().map_err(|e| Error::InvalidConfig {
            path: error_origin(&e, sources.iter().rev().find_map(Source::path)),
            reason: format!("{e}{}", unknown_key_hint(&e).unwrap_or_default()),
        })?;
        if !vars.is_empty() {
            sources.push(Source::Env { vars });
        }
        config.sources = sources;
        Ok(config)
    }

    /// Return the highest-precedence config file in [`Config::sources`].
    #[must_use]
    pub fn loaded_from(&self) -> Option<&Path> {
        self.sources.iter().rev().find_map(Source::path)
    }

    /// Merge a config file into figment based on extension
    fn merge_config_file(figment: Figment, path: &Path) -> Result<Figment, Error> {
        let contents = std::fs::read_to_string(path).map_err(|e| Error::InvalidConfig {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(figment.merge(FileContents::new(path, &contents)?))
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

struct FileContents {
    path: PathBuf,
    format: Format,
    contents: String,
}

impl FileContents {
    fn new(path: &Path, contents: &str) -> Result<Self, Error> {
        let format = Format::from_path(path);
        let contents = match format {
            Format::Jsonc => {
                let mut stripped = String::new();
                json_comments::StripComments::new(contents.as_bytes())
                    .read_to_string(&mut stripped)
                    .map_err(|e| Error::InvalidConfig {
                        path: path.display().to_string(),
                        reason: format!("failed to strip comments: {e}"),
                    })?;
                stripped
            }
            Format::Toml | Format::Json | Format::Yaml => contents.to_owned(),
        };
        Ok(Self {
            path: path.to_path_buf(),
            format,
            contents,
        })
    }
}

impl figment::Provider for FileContents {
    fn metadata(&self) -> figment::Metadata {
        let name = match self.format {
            Format::Toml => "TOML file",
            Format::Json => "JSON file",
            Format::Jsonc => "JSONC file",
            Format::Yaml => "YAML file",
        };
        figment::Metadata::from(name, self.path.as_path())
    }

    fn data(
        &self,
    ) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        match self.format {
            Format::Toml => figment::Provider::data(&Toml::string(&self.contents)),
            Format::Json | Format::Jsonc => figment::Provider::data(&Json::string(&self.contents)),
            Format::Yaml => figment::Provider::data(&Yaml::string(&self.contents)),
        }
    }
}

fn unknown_key_hint(error: &figment::Error) -> Option<String> {
    let figment::error::Kind::UnknownField(found, expected) = &error.kind else {
        return None;
    };
    let parent = error.path.get(..error.path.len().checked_sub(1)?)?;
    let threshold = (found.chars().count() / 3).max(1);
    let near = expected
        .iter()
        .map(|key| (strsim::levenshtein(found, key), *key))
        .filter(|(distance, _)| *distance <= threshold)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, key)| key);
    if let Some(key) = near
        && let Some((_, new)) = RENAMED_KEYS.iter().find(|(old, _)| *old == key)
    {
        return Some(format!("; did you mean `{new}`?"));
    }
    let leaf: Vec<&str> = if let Some(key) = near {
        vec![key]
    } else {
        found
            .char_indices()
            .filter(|(_, c)| *c == '_')
            .find_map(|(i, _)| {
                let (head, tail) = (&found[..i], &found[i + 1..]);
                (!tail.is_empty() && expected.contains(&head)).then(|| vec![head, tail])
            })?
    };
    let suggestion: Vec<&str> = parent.iter().map(String::as_str).chain(leaf).collect();
    let rendered = match (&error.metadata, &error.profile) {
        (Some(metadata), Some(profile)) if metadata.source.is_none() => {
            metadata.interpolate(profile, &suggestion)
        }
        _ => suggestion.join("."),
    };
    Some(format!("; did you mean `{rendered}`?"))
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
        assert_eq!(config.scan.extra_paths, Vec::<ExtraPath>::new());
        assert_eq!(config.overrides, Vec::<Override>::new());
        assert!(config.loaded_from().is_none());
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
        let extra: Vec<&str> = config
            .scan
            .extra_paths
            .iter()
            .map(ExtraPath::as_str)
            .collect();
        assert_eq!(extra, [".gitea/workflows"]);
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

    #[test]
    fn test_from_contents_reads_every_format() -> Result<(), Error> {
        let samples = [
            ("actup.toml", "[policy]\nbump = \"minor\"\n"),
            ("actup.json", r#"{"policy": {"bump": "minor"}}"#),
            (
                "actup.jsonc",
                "{\n  // keep majors\n  \"policy\": { \"bump\": \"minor\" } /* end */\n}",
            ),
            (".github/actup.yaml", "policy:\n  bump: minor\n"),
            (".config/actup.yml", "policy:\n  bump: minor\n"),
        ];
        for (name, contents) in samples {
            let config =
                Config::from_contents(Some((Path::new(name), contents)), EnvOverrides::default())?;
            assert_eq!(config.policy.bump, policy::Update::Minor, "{name}");
            assert_eq!(config.policy.pin, Policy::default().pin, "{name}");
            assert_eq!(config.loaded_from(), Some(Path::new(name)));
        }
        Ok(())
    }

    #[test]
    fn test_from_contents_rejects_renamed_key_naming_the_file() {
        let Err(Error::InvalidConfig { path, reason }) = Config::from_contents(
            Some((Path::new(".github/actup.yaml"), "update_policy: patch\n")),
            EnvOverrides::default(),
        ) else {
            panic!("renamed key must be rejected");
        };
        assert_eq!(path, ".github/actup.yaml");
        assert!(
            reason.contains("`update_policy` is no longer supported; use `policy.bump`"),
            "{reason}"
        );
    }

    #[test]
    fn test_from_contents_rejects_escaping_extra_paths() {
        for (entry, reason) in [
            ("../../../etc/*.yml", "contains `..`"),
            ("/etc/*.yml", "is absolute"),
        ] {
            let Err(Error::InvalidConfig { path, reason: got }) = Config::from_contents(
                Some((
                    Path::new("actup.toml"),
                    &format!("[scan]\nextra_paths = [\"{entry}\"]\n"),
                )),
                EnvOverrides::default(),
            ) else {
                panic!("`{entry}` must be rejected");
            };
            assert_eq!(path, "actup.toml");
            assert!(
                got.contains(&format!("`scan.extra_paths` entry `{entry}` {reason}")),
                "{got}"
            );
        }
    }

    #[test]
    fn test_from_contents_syntax_error_names_the_file() {
        let Err(Error::InvalidConfig { path, .. }) = Config::from_contents(
            Some((Path::new("actup.toml"), "[policy\n")),
            EnvOverrides::default(),
        ) else {
            panic!("invalid TOML must be rejected");
        };
        assert_eq!(path, "actup.toml");
    }

    #[test]
    fn test_from_contents_applies_env_over_the_file() -> Result<(), Error> {
        let env = EnvOverrides::from_vars([
            ("ACTUP_POLICY__BUMP", "patch"),
            ("ACTUP_VERBOSE", "true"),
            ("GITHUB_TOKEN", "t"),
        ]);
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                "[policy]\nbump = \"minor\"\npin = \"sha\"\n",
            )),
            env,
        )?;
        assert_eq!(config.policy.bump, policy::Update::Patch);
        assert_eq!(config.policy.pin, policy::Style::Sha);
        assert_eq!(config.loaded_from(), Some(Path::new("actup.toml")));
        Ok(())
    }

    #[test]
    fn test_from_contents_records_the_file_and_env_sources() -> Result<(), Error> {
        let env = || {
            EnvOverrides::from_vars([
                ("ACTUP_POLICY__PIN", "sha"),
                (" actup_cache__ttl ", "60"),
                ("ACTUP_VERBOSE", "1"),
                ("GITHUB_TOKEN", "t"),
            ])
        };
        let vars = vec![
            "ACTUP_POLICY__PIN".to_owned(),
            "actup_cache__ttl".to_owned(),
        ];

        let config = Config::from_contents(None, env())?;
        assert_eq!(config.sources, [Source::Env { vars: vars.clone() }]);
        assert_eq!(config.loaded_from(), None);

        let config = Config::from_contents(Some((Path::new("actup.toml"), "")), env())?;
        assert_eq!(
            config.sources,
            [
                Source::Repo {
                    path: "actup.toml".into()
                },
                Source::Env { vars },
            ]
        );
        assert_eq!(config.loaded_from(), Some(Path::new("actup.toml")));

        let config = Config::from_contents(None, EnvOverrides::default())?;
        assert_eq!(config.sources, []);
        Ok(())
    }

    fn rejected(file: Option<(&str, &str)>, env: &[(&str, &str)]) -> (String, String) {
        let Err(Error::InvalidConfig { path, reason }) = Config::from_contents(
            file.map(|(path, contents)| (Path::new(path), contents)),
            EnvOverrides::from_vars(env.iter().copied()),
        ) else {
            panic!("{file:?} {env:?} must be rejected");
        };
        (path, reason)
    }

    #[test]
    fn test_unknown_keys_are_rejected_in_every_table_and_format() {
        for (name, contents, found, hint) in [
            (
                "actup.toml",
                "update-policy = \"patch\"\n",
                "update-policy",
                Some("policy.bump"),
            ),
            (
                "actup.toml",
                "[policy]\nbumpp = \"minor\"\n",
                "bumpp",
                Some("policy.bump"),
            ),
            (
                "actup.toml",
                "[scan]\nignores = []\n",
                "ignores",
                Some("scan.ignore"),
            ),
            (
                "actup.toml",
                "[cache]\nttll = 1\n",
                "ttll",
                Some("cache.ttl"),
            ),
            ("actup.toml", "zzz = 1\n", "zzz", None),
            ("actup.json", r#"{"polcy": {}}"#, "polcy", Some("policy")),
            (
                "actup.jsonc",
                "{ /* c */ \"cache\": { \"enable\": true } }",
                "enable",
                Some("cache.enabled"),
            ),
            (
                ".github/actup.yaml",
                "scan:\n  extra_path: []\n",
                "extra_path",
                Some("scan.extra_paths"),
            ),
        ] {
            let (path, reason) = rejected(Some((name, contents)), &[]);
            assert_eq!(path, name);
            assert!(reason.contains(&format!("found `{found}`")), "{reason}");
            match hint {
                Some(hint) => assert!(
                    reason.ends_with(&format!("; did you mean `{hint}`?")),
                    "{reason}"
                ),
                None => assert!(!reason.contains("did you mean"), "{reason}"),
            }
        }
    }

    #[test]
    fn test_unknown_env_keys_name_the_variable() {
        for (var, hint) in [
            ("ACTUP_POLICY_PIN", Some("ACTUP_POLICY__PIN")),
            ("ACTUP_POLICY__PINN", Some("ACTUP_POLICY__PIN")),
            ("ACTUP_UPDATE_POLICYY", Some("policy.bump")),
            ("ACTUP_ZZZ", None),
        ] {
            let (path, reason) = rejected(None, &[(var, "x")]);
            assert_eq!(path, "`ACTUP_` environment variable(s)");
            assert!(reason.contains(&format!("for key \"{var}\"")), "{reason}");
            match hint {
                Some(hint) => assert!(
                    reason.ends_with(&format!("; did you mean `{hint}`?")),
                    "{reason}"
                ),
                None => assert!(!reason.contains("did you mean"), "{reason}"),
            }
        }
    }

    #[test]
    fn test_config_source_display_and_path() {
        let path = PathBuf::from("/x/actup.toml");
        let cases = [
            (
                Source::User { path: path.clone() },
                "user config /x/actup.toml",
            ),
            (
                Source::Repo { path: path.clone() },
                "repo config /x/actup.toml",
            ),
            (
                Source::Explicit { path: path.clone() },
                "explicit config /x/actup.toml",
            ),
        ];
        for (source, text) in cases {
            assert_eq!(source.to_string(), text);
            assert_eq!(source.path(), Some(path.as_path()));
        }
        let env = Source::Env {
            vars: vec!["ACTUP_A".into(), "ACTUP_B".into()],
        };
        assert_eq!(env.to_string(), "environment ACTUP_A, ACTUP_B");
        assert_eq!(env.path(), None);
    }

    #[test]
    fn test_from_contents_without_file_applies_env_over_defaults() -> Result<(), Error> {
        let config = Config::from_contents(
            None,
            EnvOverrides::from_vars([("ACTUP_POLICY__BRANCH", "warn")]),
        )?;
        assert_eq!(config.policy.branch, policy::Branch::Warn);
        assert_eq!(config.policy.bump, Policy::default().bump);
        assert!(config.loaded_from().is_none());
        Ok(())
    }

    const ENV_EVERY_KEY: &[(&str, &str)] = &[
        ("ACTUP_POLICY__BUMP", "patch"),
        ("ACTUP_POLICY__PIN", "sha"),
        ("ACTUP_POLICY__TRACK", "exact"),
        ("ACTUP_POLICY__BRANCH", "update"),
        ("ACTUP_POLICY__PRERELEASE", "true"),
        ("ACTUP_SCAN__IGNORE", "[\"a/b\"]"),
        ("ACTUP_SCAN__EXTRA_PATHS", "[\".gitea/workflows\"]"),
        (
            "ACTUP_OVERRIDES",
            "[{pattern = \"my-org/*\", policy = {pin = \"tag\"}}]",
        ),
        ("ACTUP_CACHE__TTL", "60"),
        ("ACTUP_CACHE__PATH", "cache-dir"),
        ("ACTUP_CACHE__ENABLED", "false"),
        ("ACTUP_DEFAULT_HOST", "ghe.corp.com"),
        ("ACTUP_FETCH__API_MODE", "rest"),
        ("ACTUP_FETCH__MAX_TAGS_PER_REPO", "7"),
        ("ACTUP_HOSTS__GHE.CORP.COM__KIND", "github"),
        (
            "ACTUP_HOSTS__GHE.CORP.COM__API_BASE",
            "https://ghe.corp.com/api/v3",
        ),
        ("ACTUP_HOSTS__GHE.CORP.COM__TOKEN_ENV", "GHE_TOKEN"),
        ("ACTUP_HOSTS__GHE.CORP.COM__API_MODE", "graphql"),
        ("ACTUP_HOSTS__GHE.CORP.COM__MAX_TAGS_PER_REPO", "3"),
    ];

    #[test]
    fn test_from_contents_env_sets_every_key() -> Result<(), Box<dyn std::error::Error>> {
        let config =
            Config::from_contents(None, EnvOverrides::from_vars(ENV_EVERY_KEY.iter().copied()))?;
        assert_eq!(
            config.policy,
            Policy {
                bump: policy::Update::Patch,
                pin: policy::Style::Sha,
                track: policy::Track::Exact,
                branch: policy::Branch::Update,
                prerelease: true,
            }
        );
        let ignore: Vec<String> = config.scan.ignore.iter().map(ToString::to_string).collect();
        assert_eq!(ignore, ["a/b"]);
        let extra: Vec<&str> = config
            .scan
            .extra_paths
            .iter()
            .map(ExtraPath::as_str)
            .collect();
        assert_eq!(extra, [".gitea/workflows"]);
        let [entry] = config.overrides.as_slice() else {
            panic!("expected one override: {:?}", config.overrides);
        };
        assert_eq!(entry.pattern.to_string(), "my-org/*");
        assert_eq!(entry.policy.pin, Some(policy::Style::Tag));
        assert_eq!(config.cache.ttl, Duration::from_secs(60));
        assert_eq!(config.cache.path.as_deref(), Some(Path::new("cache-dir")));
        assert!(!config.cache.enabled);
        assert_eq!(config.default_host.as_deref(), Some("ghe.corp.com"));
        assert_eq!(config.fetch.api_mode, ApiMode::Rest);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 7);
        let Some(host) = config.hosts.get("ghe.corp.com") else {
            panic!("missing host: {:?}", config.hosts);
        };
        assert_eq!(host.kind, Some(HostKind::GitHub));
        assert_eq!(
            host.api_base.as_deref(),
            Some("https://ghe.corp.com/api/v3")
        );
        assert_eq!(host.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(host.api_mode, Some(ApiMode::Graphql));
        assert_eq!(host.max_tags_per_repo.map(NonZeroU8::get), Some(3));
        Ok(())
    }

    fn leaf_paths(value: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        match value {
            serde_json::Value::Object(map) if !map.is_empty() => {
                for (key, child) in map {
                    path.push(key.clone());
                    leaf_paths(child, path, out);
                    path.pop();
                }
            }
            _ => out.push(path.clone()),
        }
    }

    fn lookup<'a>(value: &'a serde_json::Value, path: &[String]) -> Option<&'a serde_json::Value> {
        path.iter()
            .try_fold(value, |value, key| value.get(key))
            .filter(|value| !value.is_null())
    }

    #[test]
    fn test_env_every_key_sample_changes_every_leaf() -> Result<(), Box<dyn std::error::Error>> {
        let defaults = serde_json::to_value(Config::default())?;
        let from_env = serde_json::to_value(Config::from_contents(
            None,
            EnvOverrides::from_vars(ENV_EVERY_KEY.iter().copied()),
        )?)?;
        let mut leaves = Vec::new();
        leaf_paths(&from_env, &mut Vec::new(), &mut leaves);
        assert_eq!(leaves.len(), ENV_EVERY_KEY.len(), "{leaves:?}");
        leaf_paths(&defaults, &mut Vec::new(), &mut leaves);
        for leaf in &leaves {
            assert_ne!(
                lookup(&from_env, leaf),
                lookup(&defaults, leaf),
                "no sample sets {leaf:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_from_contents_env_error_names_the_environment() {
        let Err(Error::InvalidConfig { path, reason }) = Config::from_contents(
            Some((Path::new("actup.toml"), "[policy]\nbump = \"minor\"\n")),
            EnvOverrides::from_vars([("ACTUP_IGNORE", "a/b")]),
        ) else {
            panic!("renamed env key must be rejected");
        };
        assert_eq!(path, "`ACTUP_` environment variable(s)");
        assert!(
            reason.contains("`ignore` is no longer supported; use `scan.ignore`"),
            "{reason}"
        );
    }
}
