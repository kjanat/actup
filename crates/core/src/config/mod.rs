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
    collections::{BTreeSet, HashMap},
    convert::Infallible,
    io::Read,
    num::NonZeroU8,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use discovery::{
    CONFIG_EXTENSIONS, Candidate, canonical_path, iter_config_candidates, repo_candidates,
    repo_search_dirs, user_config_path,
};
use discovery::{Format, find_repo_config};
#[cfg(feature = "clap")]
pub use env::ignore_empty_env;
pub use env::{
    CLI_FLAG_VARS, COLOR_VAR, CONFIG_VAR, EnvOverrides, FORMAT_VAR, STRICT_VAR, VERBOSE_VAR,
};
use figment::{
    Figment,
    providers::{Format as _, Json, Serialized, Toml},
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
    /// JSON schema URL for editors; actup ignores the value.
    ///
    /// ```json
    /// { "$schema": "./schemas/actup-config.schema.json" }
    /// ```
    #[serde(rename = "$schema", skip_serializing)]
    pub schema: Option<String>,

    /// Update policy applied to every action unless an override matches.
    ///
    /// ```toml
    /// [policy]
    /// bump  = "minor"
    /// pin   = "sha"
    /// track = "keep"
    /// ```
    pub policy: Policy,

    /// Formatting of SHA pins, shared by the CLI, editors, MCP and WASM.
    pub pin: Pin,

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
    /// Absent resolves to `github.com` via the built-in preset. An
    /// auto-discovered repo config cannot set it.
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
    pub hosts: HashMap<String, Host>,

    /// Every layer that contributed to this config, lowest precedence first.
    #[serde(skip)]
    pub sources: Vec<Source>,

    /// Hosts that only an auto-discovered repo config declares; a non-preset one gets no default token.
    #[serde(skip)]
    pub repo_only_hosts: BTreeSet<String>,

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
        /// Keys the file set that a repo config may not set and no later layer sets, which were ignored.
        ignored: Vec<String>,
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
            Self::User { path } | Self::Repo { path, .. } | Self::Explicit { path } => Some(path),
            Self::Env { .. } => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User { path } => write!(f, "user config {}", path.display()),
            Self::Repo { path, ignored } if ignored.is_empty() => {
                write!(f, "repo config {}", path.display())
            }
            Self::Repo { path, ignored } => write!(
                f,
                "repo config {} (ignored {})",
                path.display(),
                ignored.join(", ")
            ),
            Self::Explicit { path } => write!(f, "explicit config {}", path.display()),
            Self::Env { vars } => write!(f, "environment {}", vars.join(", ")),
        }
    }
}

/// The `[pin]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "tsify", derive(tsify::Tsify))]
#[serde(default, deny_unknown_fields)]
pub struct Pin {
    /// Add a version label when writing a SHA pin (default: true).
    ///
    /// When false, omit generated labels and remove an existing recognized
    /// label while retaining its note. Unrelated comments stay unchanged.
    pub comment: bool,
}

impl Default for Pin {
    fn default() -> Self {
        Self { comment: true }
    }
}

/// The `[policy]` table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "tsify", derive(tsify::Tsify))]
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
    /// version. `major_tag = "contains"` also permits verified descendants
    /// of the selected release when a major tag is eligible.
    ///
    /// ```toml
    /// track = "exact"   # actions/checkout@v4 -> @v6.0.2
    /// ```
    pub track: policy::Track,

    /// Relationship required between a major tag and the selected release.
    /// `exact` (default) requires equality; `contains` also accepts verified
    /// descendants. Ahead or divergent refs are never automatically downgraded.
    pub major_tag: policy::MajorTag,

    /// How to handle floating branch refs like `@main` or `@v2`.
    ///
    /// `ignore` leaves them untouched (default), `warn` flags them as
    /// floating, `update` pins the branch or non-semver tag to its current
    /// commit SHA.
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

    /// How the LSP reports an action whose repository is archived or disabled.
    ///
    /// `hint` (default) shows a hint diagnostic with code `archived` and a
    /// hover warning, `ignore` shows nothing.
    ///
    /// ```toml
    /// archived = "ignore"   # no report for archived or disabled repositories
    /// ```
    pub archived: policy::Archived,
}

impl Policy {
    /// Return this policy with every key that `partial` sets replaced.
    #[must_use]
    pub fn with(self, partial: &PolicyOverride) -> Self {
        Self {
            bump: partial.bump.unwrap_or(self.bump),
            pin: partial.pin.unwrap_or(self.pin),
            track: partial.track.unwrap_or(self.track),
            major_tag: partial.major_tag.unwrap_or(self.major_tag),
            branch: partial.branch.unwrap_or(self.branch),
            prerelease: partial.prerelease.unwrap_or(self.prerelease),
            archived: partial.archived.unwrap_or(self.archived),
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
    /// Replaces `policy.major_tag`.
    pub major_tag: Option<policy::MajorTag>,
    /// Replaces `policy.branch`.
    pub branch: Option<policy::Branch>,
    /// Replaces `policy.prerelease`.
    pub prerelease: Option<bool>,
    /// Replaces `policy.archived`.
    pub archived: Option<policy::Archived>,
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
    /// - `.github/workflows/*.{yml,yaml}` (workflows)
    /// - `action.{yml,yaml}` and `.github/actions/*/action.{yml,yaml}` (composite actions)
    ///
    /// Use `extra_paths` to scan additional locations. A directory that an
    /// entry matches expands to `*.{yml,yaml}` directly in it and
    /// `*/action.{yml,yaml}` below it.
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
    /// An auto-discovered repo config cannot set it.
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
    /// Seconds a fetched answer stays fresh, on disk and in the memory of `actup-lsp` and `actup-mcp`
    #[serde(
        serialize_with = "serialize_duration_secs",
        deserialize_with = "deserialize_duration_secs"
    )]
    #[schemars(with = "u64")]
    pub ttl: Duration,

    /// Custom cache directory (None = use OS default)
    ///
    /// An auto-discovered repo config cannot set it.
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
    /// Preferred API fallback mode: `"graphql"` (batched, requires auth) or `"rest"` (parallel).
    ///
    /// A provider that lacks the preferred mode, such as REST-only GitLab and
    /// Gitea, uses the mode it supports.
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "HostConfig")]
pub struct Host {
    /// Provider kind. Required for non-preset hosts; implied by preset for known hosts.
    pub kind: Option<HostKind>,

    /// API base URL. Required after merge (may be inferred by provider registration).
    /// Must include scheme (`http://` or `https://`), no query or fragment.
    /// An auto-discovered repo config cannot set it.
    pub api_base: Option<String>,

    /// Env var name for auth token. Required after merge (may be defaulted by preset).
    /// An auto-discovered repo config cannot set it.
    pub token_env: Option<String>,

    /// Override global `[fetch].api_mode` for this host.
    ///
    /// A mode the host's provider does not support is a config error.
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
    /// 3. Repo config, searched upward from `start_dir` by [`repo_search_dirs`] with `ceiling`,
    ///    unless it is the file at `config_path`
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
        let mut layers = Layers::default();

        if let Some(user_config) = user_config
            && user_config.exists()
        {
            layers = layers.trusted(
                Toml::file(user_config),
                Source::User {
                    path: user_config.to_path_buf(),
                },
            );
        }

        if let Some(repo_config) = find_repo_config(start_dir, ceiling)
            && !config_path.is_some_and(|path| same_file(path, &repo_config))
        {
            layers = layers.repo(read_config_file(&repo_config)?);
        }

        if let Some(path) = config_path {
            layers = layers.trusted(
                read_config_file(path)?,
                Source::Explicit {
                    path: path.to_path_buf(),
                },
            );
        }

        let env = EnvOverrides::from_vars(std::env::vars_os().map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        }));
        layers.extract(env)
    }

    /// Parse one auto-discovered repo config `file`, given as path and contents, over the defaults and apply `env`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`] for an invalid value.
    pub fn from_contents(file: Option<(&Path, &str)>, env: EnvOverrides) -> Result<Self, Error> {
        let mut layers = Layers::default();
        if let Some((path, contents)) = file {
            layers = layers.repo(FileContents::new(path, contents)?);
        }
        layers.extract(env)
    }

    /// Parse one explicit config file, given as path and contents, over the defaults and apply `env`.
    ///
    /// Every key applies, as for a `--config` file.
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`] for an invalid value.
    pub fn from_explicit_contents(
        path: &Path,
        contents: &str,
        env: EnvOverrides,
    ) -> Result<Self, Error> {
        Self::from_repo_and_explicit(None, Some((path, contents)), env)
    }

    /// Parse a discovered repo config `repo` and then an explicit config `explicit`, each given as
    /// path and contents, over the defaults and apply `env`.
    ///
    /// Every key of `explicit` applies, as for a `--config` file, and wins over `repo`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidConfig`] for an invalid value.
    pub fn from_repo_and_explicit(
        repo: Option<(&Path, &str)>,
        explicit: Option<(&Path, &str)>,
        env: EnvOverrides,
    ) -> Result<Self, Error> {
        let mut layers = Layers::default();
        if let Some((path, contents)) = repo {
            layers = layers.repo(FileContents::new(path, contents)?);
        }
        if let Some((path, contents)) = explicit {
            layers = layers.trusted(
                FileContents::new(path, contents)?,
                Source::Explicit {
                    path: path.to_path_buf(),
                },
            );
        }
        layers.extract(env)
    }

    /// Return the highest-precedence config file in [`Config::sources`].
    #[must_use]
    pub fn loaded_from(&self) -> Option<&Path> {
        self.sources.iter().rev().find_map(Source::path)
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

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn read_config_file(path: &Path) -> Result<FileContents, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::InvalidConfig {
        path: path.display().to_string(),
        reason: String::new(),
        hint: None,
        source: Some(Arc::new(e)),
    })?;
    FileContents::new(path, &contents)
}

type ProfileData = figment::value::Map<figment::Profile, figment::value::Dict>;

fn declared_hosts(provider: &impl figment::Provider) -> BTreeSet<String> {
    provider
        .data()
        .into_iter()
        .flat_map(ProfileData::into_values)
        .filter_map(|mut dict| match dict.remove("hosts") {
            Some(figment::value::Value::Dict(_, hosts)) => Some(hosts.into_keys()),
            _ => None,
        })
        .flatten()
        .collect()
}

struct Layer {
    source: Source,
    repo_only: BTreeSet<String>,
}

struct Layers {
    figment: Figment,
    stack: Vec<Layer>,
    repo_hosts: BTreeSet<String>,
    trusted_hosts: BTreeSet<String>,
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            figment: Figment::new().merge(Serialized::defaults(Config::default())),
            stack: Vec::new(),
            repo_hosts: BTreeSet::new(),
            trusted_hosts: BTreeSet::new(),
        }
    }
}

impl Layers {
    fn trusted(mut self, provider: impl figment::Provider, source: Source) -> Self {
        self.trusted_hosts.extend(declared_hosts(&provider));
        self.stack.push(Layer {
            source,
            repo_only: repo_only_keys(&provider),
        });
        self.figment = self.figment.merge(provider);
        self
    }

    fn repo(mut self, file: FileContents) -> Self {
        let repo = RepoFile::new(file);
        self.repo_hosts.extend(repo.hosts.iter().cloned());
        self.stack.push(Layer {
            source: Source::Repo {
                path: repo.file.path.clone(),
                ignored: repo.ignored.clone(),
            },
            repo_only: BTreeSet::new(),
        });
        self.figment = self.figment.merge(repo);
        self
    }

    fn extract(mut self, env: EnvOverrides) -> Result<Config, Error> {
        let vars = env.names();
        self.trusted_hosts.extend(declared_hosts(&env));
        let mut later = repo_only_keys(&env);
        let mut sources = Vec::with_capacity(self.stack.len() + 1);
        for layer in self.stack.into_iter().rev() {
            let source = match layer.source {
                Source::Repo { path, mut ignored } => {
                    ignored.retain(|key| !later.contains(key));
                    for key in &ignored {
                        tracing::warn!(
                            key = %key,
                            path = %path.display(),
                            "ignoring a key that an auto-discovered repo config may not set; set it in the user config or pass --config"
                        );
                    }
                    Source::Repo { path, ignored }
                }
                source => source,
            };
            later.extend(layer.repo_only);
            sources.push(source);
        }
        sources.reverse();
        let figment = self.figment.merge(env);
        let mut config: Config = figment.extract().map_err(|e| Error::InvalidConfig {
            path: error_origin(&e, sources.iter().rev().find_map(Source::path)),
            reason: String::new(),
            hint: unknown_key_hint(&e),
            source: Some(Arc::new(e)),
        })?;
        if !vars.is_empty() {
            sources.push(Source::Env { vars });
        }
        config.sources = sources;
        config.repo_only_hosts = self
            .repo_hosts
            .difference(&self.trusted_hosts)
            .cloned()
            .collect();
        Ok(config)
    }
}

fn repo_only_keys(provider: &impl figment::Provider) -> BTreeSet<String> {
    provider
        .data()
        .into_iter()
        .flat_map(ProfileData::into_values)
        .flat_map(|mut dict| strip_repo_only(&mut dict))
        .collect()
}

fn strip_repo_only(dict: &mut figment::value::Dict) -> Vec<String> {
    use figment::value::Value;

    let mut stripped = Vec::new();
    if dict.remove("default_host").is_some() {
        stripped.push("default_host".to_owned());
    }
    if let Some(Value::Dict(_, scan)) = dict.get_mut("scan")
        && scan.remove("extra_paths").is_some()
    {
        stripped.push("scan.extra_paths".to_owned());
    }
    if let Some(Value::Dict(_, cache)) = dict.get_mut("cache")
        && cache.remove("path").is_some()
    {
        stripped.push("cache.path".to_owned());
    }
    if let Some(Value::Dict(_, table)) = dict.get_mut("hosts") {
        for (host, value) in table.iter_mut() {
            let Value::Dict(_, host_table) = value else {
                continue;
            };
            for key in ["api_base", "token_env"] {
                if host_table.remove(key).is_some() {
                    stripped.push(format!("hosts.\"{host}\".{key}"));
                }
            }
        }
    }
    stripped
}

struct RepoFile {
    file: FileContents,
    data: Result<ProfileData, figment::Error>,
    ignored: Vec<String>,
    hosts: BTreeSet<String>,
}

impl RepoFile {
    fn new(file: FileContents) -> Self {
        use figment::value::Value;

        let mut ignored = Vec::new();
        let mut hosts = BTreeSet::new();
        let data = figment::Provider::data(&file).map(|mut map| {
            for dict in map.values_mut() {
                ignored.extend(strip_repo_only(dict));
                if let Some(Value::Dict(_, table)) = dict.get("hosts") {
                    hosts.extend(table.keys().cloned());
                }
            }
            map
        });
        Self {
            file,
            data,
            ignored,
            hosts,
        }
    }
}

impl figment::Provider for RepoFile {
    fn metadata(&self) -> figment::Metadata {
        self.file.metadata()
    }

    fn data(&self) -> Result<ProfileData, figment::Error> {
        self.data.clone()
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
                        reason: "failed to strip comments".into(),
                        hint: None,
                        source: Some(Arc::new(e)),
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
            Format::Yaml => crate::yaml::from_str(&self.contents, crate::yaml::Dialect::Config)
                .map(|dict| figment::Profile::Default.collect(dict))
                .map_err(|e| figment::Error::from(e.to_string())),
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
        return Some(format!("did you mean `{new}`?"));
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
    Some(format!("did you mean `{rendered}`?"))
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
    use actup_test_support::logged;

    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

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
        assert!(config.pin.comment);
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

    #[test]
    fn pin_comments_default_true_and_follow_config_precedence() -> TestResult {
        for text in ["", "[pin]\n", "[pin]\ncomment = true\n"] {
            let config: Config = toml::from_str(text)?;
            assert!(config.pin.comment);
        }
        let file = Some((Path::new("actup.toml"), "[pin]\ncomment = false\n"));
        let config = Config::from_contents(file, EnvOverrides::default())?;
        assert!(!config.pin.comment);
        assert_eq!(config.policy.pin, policy::Style::Preserve);
        let config = Config::from_contents(
            file,
            EnvOverrides::from_vars([("ACTUP_PIN__COMMENT", "true")]),
        )?;
        assert!(config.pin.comment);
        assert!(toml::from_str::<Config>("[pin]\ncomments = false\n").is_err());
        Ok(())
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
    fn test_hostkind_serde_roundtrip() -> TestResult {
        for (variant, expected_str) in [
            (HostKind::GitHub, "\"github\""),
            (HostKind::GitLab, "\"gitlab\""),
            (HostKind::Gitea, "\"gitea\""),
        ] {
            let json = serde_json::to_string(&variant)?;
            assert_eq!(json, expected_str, "serialize {variant:?}");

            let back: HostKind = serde_json::from_str(&json)?;
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
        Ok(())
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

    #[test]
    fn test_fetch_settings_default() {
        let fs = FetchSettings::default();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[test]
    fn test_fetch_settings_serde_roundtrip_defaults() -> TestResult {
        let fs = FetchSettings::default();
        let json = serde_json::to_string(&fs)?;
        let back: FetchSettings = serde_json::from_str(&json)?;
        assert_eq!(back.api_mode, ApiMode::Graphql);
        assert_eq!(back.max_tags_per_repo.get(), 10);
        Ok(())
    }

    #[test]
    fn test_fetch_settings_serde_custom_values() -> TestResult {
        let json = r#"{"api_mode":"rest","max_tags_per_repo":50}"#;
        let fs: FetchSettings = serde_json::from_str(json)?;
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 50);
        Ok(())
    }

    #[test]
    fn test_fetch_settings_serde_partial_uses_defaults() -> TestResult {
        // Only api_mode specified — max_tags_per_repo should default
        let json = r#"{"api_mode":"rest"}"#;
        let fs: FetchSettings = serde_json::from_str(json)?;
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 10);

        // Only max_tags_per_repo — api_mode should default
        let json = r#"{"max_tags_per_repo":25}"#;
        let fs: FetchSettings = serde_json::from_str(json)?;
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 25);
        Ok(())
    }

    #[test]
    fn test_fetch_settings_serde_empty_uses_defaults() -> TestResult {
        let json = "{}";
        let fs: FetchSettings = serde_json::from_str(json)?;
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
        Ok(())
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

    #[test]
    fn test_fetch_settings_max_tags_boundary() -> TestResult {
        // NonZeroU8 max = 255
        let json = r#"{"max_tags_per_repo":255}"#;
        let fs: FetchSettings = serde_json::from_str(json)?;
        assert_eq!(fs.max_tags_per_repo.get(), 255);

        // 256 exceeds u8 range
        let json = r#"{"max_tags_per_repo":256}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
        Ok(())
    }

    // Host tests

    #[test]
    fn test_hostconfig_all_fields() -> TestResult {
        let json = r#"{
            "kind": "github",
            "api_base": "https://ghe.corp.com/api/v3",
            "token_env": "GHE_TOKEN",
            "api_mode": "rest",
            "max_tags_per_repo": 50
        }"#;
        let hc: Host = serde_json::from_str(json)?;
        assert_eq!(hc.kind, Some(HostKind::GitHub));
        assert_eq!(hc.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(hc.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(50));
        Ok(())
    }

    #[test]
    fn test_hostconfig_no_fields() -> TestResult {
        let json = "{}";
        let hc: Host = serde_json::from_str(json)?;
        assert_eq!(hc.kind, None);
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        assert_eq!(hc.api_mode, None);
        assert_eq!(hc.max_tags_per_repo, None);
        Ok(())
    }

    #[test]
    fn test_hostconfig_partial_kind_only() -> TestResult {
        let json = r#"{"kind": "gitlab"}"#;
        let hc: Host = serde_json::from_str(json)?;
        assert_eq!(hc.kind, Some(HostKind::GitLab));
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        assert_eq!(hc.api_mode, None);
        assert_eq!(hc.max_tags_per_repo, None);
        Ok(())
    }

    #[test]
    fn test_hostconfig_partial_fetch_overrides_only() -> TestResult {
        let json = r#"{"api_mode": "rest", "max_tags_per_repo": 25}"#;
        let hc: Host = serde_json::from_str(json)?;
        assert_eq!(hc.kind, None);
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(25));
        Ok(())
    }

    #[test]
    fn test_hostconfig_deny_unknown_fields() {
        let json = r#"{"kind": "github", "bogus": true}"#;
        let result = serde_json::from_str::<Host>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_rejects_invalid_kind() {
        let json = r#"{"kind": "bitbucket"}"#;
        let result = serde_json::from_str::<Host>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_rejects_zero_max_tags() {
        let json = r#"{"max_tags_per_repo": 0}"#;
        let result = serde_json::from_str::<Host>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_serde_roundtrip() -> TestResult {
        let hc = Host {
            kind: Some(HostKind::Gitea),
            api_base: Some("https://gitea.local/api/v1".into()),
            token_env: Some("GITEA_TOKEN".into()),
            api_mode: Some(ApiMode::Rest),
            max_tags_per_repo: NonZeroU8::new(100),
        };
        let json = serde_json::to_string(&hc)?;
        let back: Host = serde_json::from_str(&json)?;
        assert_eq!(back.kind, hc.kind);
        assert_eq!(back.api_base, hc.api_base);
        assert_eq!(back.token_env, hc.token_env);
        assert_eq!(back.api_mode, hc.api_mode);
        assert_eq!(back.max_tags_per_repo, hc.max_tags_per_repo);
        Ok(())
    }

    #[test]
    fn test_hostconfig_each_kind_variant() -> TestResult {
        for (variant, name) in [
            (HostKind::GitHub, "github"),
            (HostKind::GitLab, "gitlab"),
            (HostKind::Gitea, "gitea"),
        ] {
            let json = format!(r#"{{"kind": "{name}"}}"#);
            let hc: Host = serde_json::from_str(&json)?;
            assert_eq!(hc.kind, Some(variant), "kind={name}");
        }
        Ok(())
    }

    // TOML deserialization tests (realistic config fragments)

    #[test]
    fn test_hostkind_toml_deserialization() -> TestResult {
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
            let w: Wrapper = toml::from_str(&toml_str)?;
            assert_eq!(w.kind, variant, "TOML kind={name}");
        }
        Ok(())
    }

    #[test]
    fn test_fetch_settings_toml_deserialization() -> TestResult {
        let toml_str = r#"
            api_mode = "rest"
            max_tags_per_repo = 42
        "#;
        let fs: FetchSettings = toml::from_str(toml_str)?;
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 42);
        Ok(())
    }

    #[test]
    fn test_fetch_settings_toml_empty_uses_defaults() -> TestResult {
        let fs: FetchSettings = toml::from_str("")?;
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
        Ok(())
    }

    #[test]
    fn test_hostconfig_toml_full() -> TestResult {
        let toml_str = r#"
            kind              = "github"
            api_base          = "https://ghe.corp.com/api/v3"
            token_env         = "GHE_TOKEN"
            api_mode          = "rest"
            max_tags_per_repo = 50
        "#;
        let hc: Host = toml::from_str(toml_str)?;
        assert_eq!(hc.kind, Some(HostKind::GitHub));
        assert_eq!(hc.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(hc.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(50));
        Ok(())
    }

    #[test]
    fn test_hostconfig_toml_minimal() -> TestResult {
        let toml_str = r#"kind = "gitea""#;
        let hc: Host = toml::from_str(toml_str)?;
        assert_eq!(hc.kind, Some(HostKind::Gitea));
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        Ok(())
    }

    #[test]
    fn test_hostconfig_toml_deny_unknown_fields() {
        let toml_str = r#"
            kind = "github"
            typo_field = "oops"
        "#;
        let result = toml::from_str::<Host>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_toml_in_hosts_map() -> TestResult {
        // Test as it would appear in actual config: [hosts."hostname"]
        #[derive(Deserialize)]
        struct Outer {
            hosts: HashMap<String, Host>,
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
        let outer: Outer = toml::from_str(toml_str)?;
        assert_eq!(outer.hosts.len(), 2);

        let ghe = &outer.hosts["ghe.corp.com"];
        assert_eq!(ghe.kind, Some(HostKind::GitHub));
        assert_eq!(ghe.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(ghe.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(ghe.api_mode, None);

        let gl = &outer.hosts["gitlab.internal"];
        assert_eq!(gl.kind, Some(HostKind::GitLab));
        assert_eq!(gl.api_mode, Some(ApiMode::Rest));
        Ok(())
    }

    // Config-level TOML tests (new fields)

    #[test]
    fn test_config_toml_new_fields() -> TestResult {
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
        let config: Config = toml::from_str(toml_str)?;
        assert_eq!(config.default_host.as_deref(), Some("github.com"));
        assert_eq!(config.fetch.api_mode, ApiMode::Rest);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 42);
        assert_eq!(config.hosts.len(), 1);
        let ghe = &config.hosts["ghe.corp.com"];
        assert_eq!(ghe.kind, Some(HostKind::GitHub));
        assert_eq!(ghe.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        Ok(())
    }

    #[test]
    fn test_config_toml_empty_uses_defaults() -> TestResult {
        let config: Config = toml::from_str("")?;
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 10);
        assert!(config.hosts.is_empty());
        Ok(())
    }

    #[test]
    fn test_config_toml_only_fetch() -> TestResult {
        let toml_str = r#"
            [fetch]
            api_mode = "graphql"
            max_tags_per_repo = 100
        "#;
        let config: Config = toml::from_str(toml_str)?;
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 100);
        assert!(config.hosts.is_empty());
        Ok(())
    }

    #[test]
    fn test_config_toml_multiple_hosts() -> TestResult {
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
        let config: Config = toml::from_str(toml_str)?;
        assert_eq!(config.default_host.as_deref(), Some("ghe.corp.com"));
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts["ghe.corp.com"].api_mode, Some(ApiMode::Rest));
        assert_eq!(config.hosts["gitlab.internal"].kind, Some(HostKind::GitLab));
        Ok(())
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
            archived   = "ignore"

            [scan]
            ignore      = ["my-org/*", "owner/repo@v1.91.0"]
            extra_paths = [".gitea/workflows"]

            [[overrides]]
            pattern = "actions/*"
            policy  = { bump = "patch", track = "exact" }

            [[overrides]]
            pattern = "ghe.corp.com/platform/ci"
            policy  = { pin = "tag", branch = "update", prerelease = false, archived = "hint" }
        "#;
        let config: Config = toml::from_str(toml_str)?;
        assert_eq!(
            config.policy,
            Policy {
                bump: policy::Update::Minor,
                pin: policy::Style::Sha,
                track: policy::Track::Major,
                major_tag: policy::MajorTag::Exact,
                branch: policy::Branch::Warn,
                prerelease: true,
                archived: policy::Archived::Ignore,
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
                archived: Some(policy::Archived::Hint),
                ..Default::default()
            }
        );
        let overridden = ActionRef::new(Some("ghe.corp.com".into()), "platform", "ci", None, "v1");
        assert_eq!(
            config.policy_for(&overridden).archived,
            policy::Archived::Hint
        );
        let plain = ActionRef::simple("other", "repo", "v1");
        assert_eq!(config.policy_for(&plain).archived, policy::Archived::Ignore);
        Ok(())
    }

    #[test]
    fn test_config_toml_rejects_bad_override() {
        for toml_str in [
            "[[overrides]]\npattern = \"checkout\"",
            "[[overrides]]\npattern = \"a/b\"\npolicy = { update_policy = \"patch\" }",
            "[[overrides]]\npattern = \"a/b\"\nignore = true",
            "[[overrides]]\npattern = \"a/b\"\npolicy = { archived = \"warn\" }",
            "[[overrides]]\npattern = \"a/b\"\npolicy = { archive = \"ignore\" }",
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
    fn test_from_explicit_contents_keeps_every_key_and_applies_env() -> Result<(), Error> {
        let config = Config::from_explicit_contents(
            Path::new("ci/actup.toml"),
            "default_host = \"ghe.corp.example\"\n\n[scan]\nextra_paths = [\".gitea/workflows\"]\n\n[cache]\npath = \"/tmp/actup\"\n\n[hosts.\"ghe.corp.example\"]\nkind = \"github\"\napi_base = \"https://ghe.corp.example/api/v3\"\ntoken_env = \"GHE_TOKEN\"\n",
            EnvOverrides::from_vars([("ACTUP_POLICY__BUMP", "patch")]),
        )?;

        assert_eq!(config.default_host.as_deref(), Some("ghe.corp.example"));
        assert_eq!(
            config
                .scan
                .extra_paths
                .iter()
                .map(ExtraPath::as_str)
                .collect::<Vec<_>>(),
            [".gitea/workflows"]
        );
        assert_eq!(config.cache.path.as_deref(), Some(Path::new("/tmp/actup")));
        let host = config.hosts.get("ghe.corp.example");
        assert_eq!(
            host.and_then(|h| h.api_base.as_deref()),
            Some("https://ghe.corp.example/api/v3")
        );
        assert_eq!(host.and_then(|h| h.token_env.as_deref()), Some("GHE_TOKEN"));
        assert_eq!(config.policy.bump, crate::policy::Update::Patch);
        assert_eq!(config.loaded_from(), Some(Path::new("ci/actup.toml")));
        assert_eq!(
            config.sources,
            [
                Source::Explicit {
                    path: PathBuf::from("ci/actup.toml"),
                },
                Source::Env {
                    vars: vec!["ACTUP_POLICY__BUMP".to_string()],
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn test_from_repo_and_explicit_layers_the_explicit_file_last() -> Result<(), Error> {
        let config = Config::from_repo_and_explicit(
            Some((
                Path::new("actup.toml"),
                "[policy]\nbump = \"minor\"\npin = \"tag\"\n",
            )),
            Some((Path::new("ci/actup.toml"), "[policy]\npin = \"sha\"\n")),
            EnvOverrides::default(),
        )?;

        assert_eq!(config.policy.bump, crate::policy::Update::Minor);
        assert_eq!(config.policy.pin, crate::policy::Style::Sha);
        assert_eq!(config.loaded_from(), Some(Path::new("ci/actup.toml")));

        let repo_only = Config::from_repo_and_explicit(
            Some((Path::new("actup.toml"), "[policy]\nbump = \"minor\"\n")),
            None,
            EnvOverrides::default(),
        )?;
        assert_eq!(repo_only.loaded_from(), Some(Path::new("actup.toml")));
        Ok(())
    }

    #[test]
    fn test_from_repo_and_explicit_drops_a_candidate_default_host() -> Result<(), Error> {
        let config = Config::from_repo_and_explicit(
            Some((
                Path::new("actup.toml"),
                "default_host = \"ghe.corp.example\"\n",
            )),
            Some((Path::new("ci/actup.toml"), "[policy]\npin = \"sha\"\n")),
            EnvOverrides::default(),
        )?;

        assert_eq!(config.default_host, None);
        assert_eq!(config.policy.pin, crate::policy::Style::Sha);
        assert_eq!(
            config.sources,
            [
                Source::Repo {
                    path: PathBuf::from("actup.toml"),
                    ignored: vec!["default_host".to_string()],
                },
                Source::Explicit {
                    path: PathBuf::from("ci/actup.toml"),
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn test_from_explicit_contents_rejects_a_renamed_key_naming_the_file() {
        let Err(err) = Config::from_explicit_contents(
            Path::new("ci/actup.toml"),
            "update_policy = \"patch\"\n",
            EnvOverrides::default(),
        ) else {
            panic!("a renamed key must fail");
        };
        let Error::InvalidConfig { path, .. } = &err else {
            panic!("{err:?} is not an invalid config");
        };
        assert_eq!(path, "ci/actup.toml");
        let rendered = crate::error::chain(&err);
        assert!(rendered.contains("update_policy"), "{rendered}");
    }

    #[test]
    fn test_from_contents_rejects_renamed_key_naming_the_file() {
        let (path, reason) = rejected(Some((".github/actup.yaml", "update_policy: patch\n")), &[]);
        assert_eq!(path, ".github/actup.yaml");
        assert!(
            reason.contains("`update_policy` is no longer supported; use `policy.bump`"),
            "{reason}"
        );
    }

    fn explicit_yaml(contents: &str) -> Result<Config, Error> {
        let path = Path::new("/x/actup.yaml");
        Layers::default()
            .trusted(
                FileContents::new(path, contents)?,
                Source::Explicit {
                    path: path.to_path_buf(),
                },
            )
            .extract(EnvOverrides::default())
    }

    fn repo_yaml(contents: &str) -> Result<Config, Error> {
        Config::from_contents(
            Some((Path::new(".github/actup.yaml"), contents)),
            EnvOverrides::default(),
        )
    }

    #[test]
    fn test_yaml_config_reads_core_schema_scalars() -> Result<(), Box<dyn std::error::Error>> {
        let config = explicit_yaml("hosts:\n  ghe.corp:\n    kind: github\n    token_env: ON\n")?;
        let Some(host) = config.hosts.get("ghe.corp") else {
            panic!("missing ghe.corp");
        };
        assert_eq!(host.token_env.as_deref(), Some("ON"));

        for (text, enabled) in [
            ("true", true),
            ("True", true),
            ("TRUE", true),
            ("FALSE", false),
        ] {
            let config = repo_yaml(&format!("cache:\n  enabled: {text}\n"))?;
            assert_eq!(config.cache.enabled, enabled, "{text}");
        }
        for (text, tags) in [
            ("12", 12),
            ("0x1F", 31),
            ("0o17", 15),
            ("010", 10),
            ("+7", 7),
        ] {
            let config = repo_yaml(&format!("fetch:\n  max_tags_per_repo: {text}\n"))?;
            assert_eq!(config.fetch.max_tags_per_repo.get(), tags, "{text}");
        }
        for text in ["~", "null", "Null", "NULL", ""] {
            let config = explicit_yaml(&format!("default_host: {text}\n"))?;
            assert_eq!(config.default_host, None, "{text:?}");
        }
        for (text, host) in [
            ("'true'", "true"),
            ("\"NULL\"", "NULL"),
            ("|\n  12", "12\n"),
            ("yes", "yes"),
            ("off", "off"),
            ("nULL", "nULL"),
            ("!!str 12", "12"),
        ] {
            let config = explicit_yaml(&format!("default_host: {text}\n"))?;
            assert_eq!(config.default_host.as_deref(), Some(host), "{text}");
        }
        Ok(())
    }

    #[test]
    fn test_yaml_config_rejects_non_core_scalars_for_typed_keys() {
        for (text, found) in [
            ("tRUE", "found string \"tRUE\""),
            ("yes", "found string \"yes\""),
            ("on", "found string \"on\""),
            ("'true'", "found string \"true\""),
            ("1", "found unsigned int"),
        ] {
            let (path, reason) = rejected(
                Some((
                    ".github/actup.yaml",
                    &format!("cache:\n  enabled: {text}\n"),
                )),
                &[],
            );
            assert_eq!(path, ".github/actup.yaml");
            assert!(reason.contains(found), "{text}: {reason}");
        }
        for (text, found) in [
            ("1.5", "found float `1.5`"),
            (".inf", "found float `inf`"),
            ("-.INF", "found float `-inf`"),
            (".NaN", "found float `NaN`"),
            ("1e3", "found float `1000`"),
            ("'10'", "found string \"10\""),
        ] {
            let (_, reason) = rejected(
                Some((".github/actup.yaml", &format!("cache:\n  ttl: {text}\n"))),
                &[],
            );
            assert!(reason.contains(found), "{text}: {reason}");
        }
    }

    #[test]
    fn test_yaml_config_errors_name_file_line_and_column() {
        let cases = [
            ("policy:\n  bump: minor\n bad\n", "at line 3, column 2"),
            (
                "base: &b\n  bump: minor\npolicy:\n  <<: *b\n",
                "merge keys are not supported at line 4, column 3",
            ),
            (
                "policy:\n  bump: minor\n  bump: patch\n",
                "duplicate key `bump` at line 3, column 3",
            ),
            (
                "policy:\n  bump: !custom minor\n",
                "tag `!custom` is not a core schema tag at line 2, column 17",
            ),
            ("policy:\n  bump: !!int minor\n", "does not fit tag"),
            (
                "policy:\n  bump: minor\n---\npolicy:\n  bump: patch\n",
                "more than one YAML document at line 3, column 1",
            ),
            (
                "? [a]\n: b\n",
                "a mapping key must be a scalar at line 1, column 3",
            ),
            (
                "fetch:\n  max_tags_per_repo: 0x100000000\n",
                "`0x100000000` does not fit in 32 bits at line 2, column 22",
            ),
            (
                "fetch:\n  max_tags_per_repo: !!int \"12\"\n",
                "tag `!!int` needs a plain scalar at line 2, column 28",
            ),
            (
                "hosts:\n  Ghe.Corp: {}\n  ghe.corp: {}\n  ghe.corp: {}\n",
                "duplicate key `ghe.corp` at line 4, column 3",
            ),
        ];
        for (contents, needle) in cases {
            let (path, reason) = rejected(Some((".github/actup.yaml", contents)), &[]);
            assert_eq!(path, ".github/actup.yaml");
            assert!(reason.contains(needle), "{contents:?}: {reason}");
            assert!(
                reason.contains("in .github/actup.yaml YAML file"),
                "{reason}"
            );
        }
    }

    #[test]
    fn test_yaml_config_expands_anchors_up_to_the_cap() -> Result<(), Box<dyn std::error::Error>> {
        let config = repo_yaml(
            "overrides:\n  - pattern: a/*\n    policy: &p\n      bump: patch\n  - pattern: b/*\n    policy: *p\n",
        )?;
        let bumps: Vec<_> = config.overrides.iter().map(|o| o.policy.bump).collect();
        assert_eq!(
            bumps,
            [Some(policy::Update::Patch), Some(policy::Update::Patch)]
        );

        let ten = |alias: &str| [alias; 10].join(", ");
        let bomb = format!(
            "a: &a [{}]\nb: &b [{}]\nc: &c [{}]\nd: [{}]\n",
            ten("1"),
            ten("*a"),
            ten("*b"),
            ten("*c"),
        );
        let (path, reason) = rejected(Some((".github/actup.yaml", &bomb)), &[]);
        assert_eq!(path, ".github/actup.yaml");
        assert!(
            reason.contains("aliases expand to more than 10000 nodes at line 4"),
            "{reason}"
        );
        Ok(())
    }

    #[test]
    fn test_yaml_config_skips_a_byte_order_mark() -> Result<(), Box<dyn std::error::Error>> {
        let config = repo_yaml("\u{feff}policy:\n  bump: minor\n")?;
        assert_eq!(config.policy.bump, policy::Update::Minor);
        let config = repo_yaml("")?;
        assert_eq!(config.policy.bump, Policy::default().bump);
        Ok(())
    }

    #[test]
    fn test_env_rejects_escaping_extra_paths() {
        for (entry, reason) in [
            ("../../../etc/*.yml", "contains `..`"),
            ("/etc/*.yml", "is absolute"),
        ] {
            let paths = format!("[\"{entry}\"]");
            let (path, got) = rejected(None, &[("ACTUP_SCAN__EXTRA_PATHS", &paths)]);
            assert_eq!(path, "`ACTUP_` environment variable(s)");
            assert!(
                got.contains(&format!("`scan.extra_paths` entry `{entry}` {reason}")),
                "{got}"
            );
        }
    }

    #[test]
    fn test_from_contents_rejects_patterns_that_never_match() {
        for (key, pattern) in [("ignore", "ghe:*/o/r"), ("ignore", "ghe.corp:*/o/r")] {
            let (path, reason) = rejected(
                Some(("actup.toml", &format!("[scan]\n{key} = [\"{pattern}\"]\n"))),
                &[],
            );
            assert_eq!(path, "actup.toml");
            assert!(
                reason.contains(&format!("invalid action pattern `{pattern}`")),
                "{reason}"
            );
        }
        let (_, reason) = rejected(
            Some((
                "actup.toml",
                "[[overrides]]\npattern = \"o/r~x\"\npolicy = { pin = \"sha\" }\n",
            )),
            &[],
        );
        assert!(
            reason.contains("invalid action pattern `o/r~x`"),
            "{reason}"
        );
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
                    path: "actup.toml".into(),
                    ignored: Vec::new(),
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
        let (path, rendered, _) = rejected_with_hint(file, env);
        (path, rendered)
    }

    fn rejected_with_hint(
        file: Option<(&str, &str)>,
        env: &[(&str, &str)],
    ) -> (String, String, Option<String>) {
        let result = Config::from_contents(
            file.map(|(path, contents)| (Path::new(path), contents)),
            EnvOverrides::from_vars(env.iter().copied()),
        );
        let Err(err) = result else {
            panic!("{file:?} {env:?} must be rejected");
        };
        let rendered = crate::error::chain(&err);
        let Error::InvalidConfig { path, hint, .. } = err else {
            panic!("expected InvalidConfig, got {err:?}");
        };
        (path, rendered, hint)
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
            let (path, reason, got) = rejected_with_hint(Some((name, contents)), &[]);
            assert_eq!(path, name);
            assert!(reason.contains(&format!("found `{found}`")), "{reason}");
            assert_eq!(
                got,
                hint.map(|hint| format!("did you mean `{hint}`?")),
                "{reason}"
            );
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
            let (path, reason, got) = rejected_with_hint(None, &[(var, "x")]);
            assert_eq!(path, "`ACTUP_` environment variable(s)");
            assert!(reason.contains(&format!("for key \"{var}\"")), "{reason}");
            assert_eq!(
                got,
                hint.map(|hint| format!("did you mean `{hint}`?")),
                "{reason}"
            );
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
    fn test_top_level_schema_key_is_ignored_in_every_format()
    -> Result<(), Box<dyn std::error::Error>> {
        for (name, contents) in [
            (
                "actup.json",
                r#"{"$schema": "./schemas/actup-config.schema.json", "policy": {"bump": "minor"}}"#,
            ),
            (
                "actup.jsonc",
                "{ // editor schema\n\"$schema\": \"x\", \"policy\": { \"bump\": \"minor\" } }",
            ),
            ("actup.yaml", "$schema: x\npolicy:\n  bump: minor\n"),
            (
                "actup.toml",
                "\"$schema\" = \"x\"\n[policy]\nbump = \"minor\"\n",
            ),
        ] {
            let config =
                Config::from_contents(Some((Path::new(name), contents)), EnvOverrides::default())?;
            assert_eq!(config.policy.bump, policy::Update::Minor, "{name}");
            assert!(
                serde_json::to_value(&config)?.get("$schema").is_none(),
                "{name}"
            );
        }
        for (name, contents) in [
            ("actup.toml", "[policy]\n\"$schema\" = \"x\"\n"),
            ("actup.json", r#"{"cache": {"$schema": "x"}}"#),
            ("actup.yaml", "scan:\n  $schema: x\n"),
        ] {
            let (path, reason) = rejected(Some((name, contents)), &[]);
            assert_eq!(path, name);
            assert!(reason.contains("found `$schema`"), "{reason}");
        }
        Ok(())
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
                Source::Repo {
                    path: path.clone(),
                    ignored: Vec::new(),
                },
                "repo config /x/actup.toml",
            ),
            (
                Source::Repo {
                    path: path.clone(),
                    ignored: vec!["default_host".into(), "scan.extra_paths".into()],
                },
                "repo config /x/actup.toml (ignored default_host, scan.extra_paths)",
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
        ("ACTUP_POLICY__MAJOR_TAG", "contains"),
        ("ACTUP_POLICY__BRANCH", "update"),
        ("ACTUP_POLICY__PRERELEASE", "true"),
        ("ACTUP_POLICY__ARCHIVED", "ignore"),
        ("ACTUP_PIN__COMMENT", "false"),
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
        assert!(!config.pin.comment);
        assert_eq!(
            config.policy,
            Policy {
                bump: policy::Update::Patch,
                pin: policy::Style::Sha,
                track: policy::Track::Exact,
                major_tag: policy::MajorTag::Contains,
                branch: policy::Branch::Update,
                prerelease: true,
                archived: policy::Archived::Ignore,
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
    fn test_from_contents_rejects_unknown_policy_key_naming_the_file() {
        let Err(ref err @ Error::InvalidConfig { ref path, .. }) = Config::from_contents(
            Some((Path::new("actup.toml"), "[policy]\ntracking = \"exact\"\n")),
            EnvOverrides::default(),
        ) else {
            panic!("unknown policy key must be rejected");
        };
        let reason = crate::error::chain(err);
        assert_eq!(path, "actup.toml");
        assert!(
            reason.contains("unknown field: found `tracking`"),
            "{reason}"
        );
    }

    #[test]
    fn test_from_contents_rejects_unknown_policy_env_key_naming_the_variable() {
        let Err(ref err @ Error::InvalidConfig { ref path, .. }) = Config::from_contents(
            None,
            EnvOverrides::from_vars([("ACTUP_POLICY__TRACKING", "exact")]),
        ) else {
            panic!("unknown policy env key must be rejected");
        };
        let reason = crate::error::chain(err);
        assert_eq!(path, "`ACTUP_` environment variable(s)");
        assert!(
            reason.contains("unknown field: found `tracking`"),
            "{reason}"
        );
        assert!(reason.contains("ACTUP_POLICY__TRACKING"), "{reason}");
    }

    #[test]
    fn test_from_contents_env_error_names_the_environment() {
        let (path, reason) = rejected(
            Some(("actup.toml", "[policy]\nbump = \"minor\"\n")),
            &[("ACTUP_IGNORE", "a/b")],
        );
        assert_eq!(path, "`ACTUP_` environment variable(s)");
        assert!(
            reason.contains("`ignore` is no longer supported; use `scan.ignore`"),
            "{reason}"
        );
    }

    const REPO_ONLY_KEYS: [&str; 5] = [
        "default_host",
        "scan.extra_paths",
        "cache.path",
        "hosts.\"h.example\".api_base",
        "hosts.\"h.example\".token_env",
    ];

    const SETS_REPO_ONLY_KEYS: &str = "default_host = \"r.example\"\n\
        [scan]\nextra_paths = [\"docs/*.yml\"]\n\
        [cache]\npath = \"/cache\"\n\
        [hosts.\"h.example\"]\nkind = \"github\"\napi_base = \"https://h.example/api/v3\"\ntoken_env = \"H_TOKEN\"\n";

    struct Layered {
        config: Config,
        ignored: Vec<String>,
        warned: Vec<String>,
    }

    fn layered(
        user: Option<&str>,
        explicit: Option<&str>,
        env: &[(&str, &str)],
    ) -> Result<Layered, Box<dyn std::error::Error>> {
        let repo = FileContents::new(Path::new("repo/actup.toml"), SETS_REPO_ONLY_KEYS)?;
        let (config, log) = logged(|| {
            let mut layers = Layers::default();
            if let Some(user) = user {
                layers = layers.trusted(
                    Toml::string(user),
                    Source::User {
                        path: "user.toml".into(),
                    },
                );
            }
            layers = layers.repo(repo);
            if let Some(explicit) = explicit {
                layers = layers.trusted(
                    Toml::string(explicit),
                    Source::Explicit {
                        path: "explicit.toml".into(),
                    },
                );
            }
            layers.extract(EnvOverrides::from_vars(env.iter().copied()))
        })?;
        let config = config?;
        let warned = log
            .lines()
            .filter(|line| line.contains("ignoring a key"))
            .map(|line| {
                assert!(line.contains("path=repo/actup.toml"), "{line}");
                line.split_once(" key=")
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("no key in {line}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ignored = config
            .sources
            .iter()
            .find_map(|source| match source {
                Source::Repo { ignored, .. } => Some(ignored.clone()),
                _ => None,
            })
            .ok_or("no repo source")?;
        Ok(Layered {
            config,
            ignored,
            warned,
        })
    }

    #[test]
    fn test_repo_keys_an_explicit_file_sets_are_not_reported_as_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let explicit = "default_host = \"e.example\"\n\
            [scan]\nextra_paths = [\"ci/*.yml\"]\n\
            [cache]\npath = \"/explicit\"\n\
            [hosts.\"h.example\"]\napi_base = \"https://e.example/api/v3\"\ntoken_env = \"E_TOKEN\"\n";
        let run = layered(None, Some(explicit), &[])?;
        assert_eq!(run.ignored, Vec::<String>::new());
        assert_eq!(run.warned, Vec::<String>::new());
        assert_eq!(run.config.default_host.as_deref(), Some("e.example"));
        assert_eq!(
            run.config
                .hosts
                .get("h.example")
                .and_then(|h| h.token_env.as_deref()),
            Some("E_TOKEN")
        );
        Ok(())
    }

    #[test]
    fn test_repo_keys_an_explicit_file_lacks_stay_ignored() -> Result<(), Box<dyn std::error::Error>>
    {
        let run = layered(None, Some("default_host = \"e.example\"\n"), &[])?;
        let rest = REPO_ONLY_KEYS[1..]
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(run.ignored, rest);
        assert_eq!(run.warned, rest);

        let run = layered(None, Some("[policy]\nbump = \"patch\"\n"), &[])?;
        assert_eq!(run.ignored, REPO_ONLY_KEYS.map(str::to_owned));
        assert_eq!(run.warned, REPO_ONLY_KEYS.map(str::to_owned));
        assert_eq!(run.config.default_host, None);
        Ok(())
    }

    #[test]
    fn test_repo_keys_env_sets_are_not_reported_as_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let run = layered(
            None,
            None,
            &[
                ("ACTUP_DEFAULT_HOST", "v.example"),
                ("ACTUP_CACHE__PATH", "/env"),
                ("ACTUP_HOSTS__H.EXAMPLE__TOKEN_ENV", "V_TOKEN"),
            ],
        )?;
        let rest = ["scan.extra_paths", "hosts.\"h.example\".api_base"].map(str::to_owned);
        assert_eq!(run.ignored, rest);
        assert_eq!(run.warned, rest);
        assert_eq!(run.config.default_host.as_deref(), Some("v.example"));
        Ok(())
    }

    #[test]
    fn test_repo_host_keys_are_compared_per_key_path() -> Result<(), Box<dyn std::error::Error>> {
        let explicit = "[hosts.\"h.example\"]\nkind = \"gitea\"\n\
            [hosts.\"other.example\"]\nkind = \"github\"\ntoken_env = \"O_TOKEN\"\n";
        let run = layered(
            None,
            Some(explicit),
            &[("ACTUP_HOSTS__H.EXAMPLE__API_MODE", "rest")],
        )?;
        assert_eq!(run.ignored, REPO_ONLY_KEYS.map(str::to_owned));
        assert_eq!(run.warned, REPO_ONLY_KEYS.map(str::to_owned));

        let run = layered(
            None,
            Some("[hosts.\"h.example\"]\ntoken_env = \"E_TOKEN\"\n"),
            &[],
        )?;
        let rest = REPO_ONLY_KEYS[..4]
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(run.ignored, rest);
        assert_eq!(run.warned, rest);
        Ok(())
    }

    #[test]
    fn test_repo_keys_the_user_config_sets_stay_ignored() -> Result<(), Box<dyn std::error::Error>>
    {
        let run = layered(Some(SETS_REPO_ONLY_KEYS), None, &[])?;
        assert_eq!(run.ignored, REPO_ONLY_KEYS.map(str::to_owned));
        assert_eq!(run.warned, REPO_ONLY_KEYS.map(str::to_owned));
        assert_eq!(run.config.default_host.as_deref(), Some("r.example"));
        Ok(())
    }
    #[test]
    fn major_tag_defaults_overrides_and_environment() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Policy::default().major_tag, policy::MajorTag::Exact);
        let source = r#"
[policy]
major_tag = "contains"
[[overrides]]
pattern = "o/*"
policy = { major_tag = "exact" }
[[overrides]]
pattern = "o/r"
policy = { major_tag = "contains" }
"#;
        let config = Config::from_contents(
            Some((Path::new("actup.toml"), source)),
            EnvOverrides::default(),
        )?;
        assert_eq!(config.policy.major_tag, policy::MajorTag::Contains);
        assert_eq!(
            config
                .policy_for(&ActionRef::simple("o", "other", "v1"))
                .major_tag,
            policy::MajorTag::Exact
        );
        assert_eq!(
            config
                .policy_for(&ActionRef::simple("o", "r", "v1"))
                .major_tag,
            policy::MajorTag::Contains
        );
        let config = Config::from_contents(
            Some((Path::new("actup.toml"), source)),
            EnvOverrides::from_vars([("ACTUP_POLICY__MAJOR_TAG", "exact")]),
        )?;
        assert_eq!(config.policy.major_tag, policy::MajorTag::Exact);
        assert!(
            Config::from_contents(
                Some((Path::new("actup.toml"), "[policy]\nmajor_tag = \"lenient\"")),
                EnvOverrides::default()
            )
            .is_err()
        );
        Ok(())
    }
}
