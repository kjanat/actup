//! Configuration types and loading
//!
//! Configuration is loaded from multiple sources with precedence:
//! 1. Environment variables (ACTUP_*) - highest
//! 2. Explicit --config path
//! 3. Repo configs (actup.toml, .actup.toml, .config/actup.toml, etc.)
//! 4. User/global config (~/.config/actup/config.toml) - fills gaps only
//! 5. Defaults (lowest)

pub mod discovery;

use crate::Error;
use crate::policy::{BranchPolicy, RefStyle, UpdatePolicy};
use figment::Figment;
use figment::providers::{Env, Format, Json, Serialized, Toml, Yaml};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use discovery::{
    CONFIG_EXTENSIONS, find_repo_config, iter_config_candidates, user_config_path,
};

/// Main configuration struct.
///
/// # Example Configuration
///
/// ```yaml
/// # .actup.toml - Conservative security-focused config
/// update_policy = "patch"
/// ref_style = "sha"
/// branch_policy = "warn"
///
/// # Result in workflow:
/// # Before:
/// # - uses: actions/checkout@v4
/// # After:
/// # - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # v4.1.1
/// ```
///
/// ```yaml
/// # .actup.toml - Permissive always-latest config
/// update_policy = "major"
/// ref_style = "tag"
/// allow_prerelease = true
///
/// # Result in workflow:
/// # Before:
/// # - uses: actions/checkout@v3
/// # After:
/// # - uses: actions/checkout@v4.2.0
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Config {
    /// Global update policy
    pub update_policy: UpdatePolicy,

    /// How to format refs when updating
    pub ref_style: RefStyle,

    /// How to handle branch references
    pub branch_policy: BranchPolicy,

    /// Whether to consider prerelease versions
    pub allow_prerelease: bool,

    /// Actions to ignore (owner/repo patterns)
    pub ignore: Vec<String>,

    /// Per-action policy overrides (keyed by "owner/repo")
    #[serde(default, rename = "actions")]
    pub action_overrides: HashMap<String, ActionOverride>,

    /// Cache configuration
    pub cache: CacheConfig,

    /// Provider configuration
    pub providers: ProvidersConfig,

    /// Additional paths to scan (beyond defaults)
    pub extra_paths: Vec<String>,

    /// Path this config was loaded from (if any)
    #[serde(skip)]
    pub loaded_from: Option<PathBuf>,
}

/// Per-action override configuration.
///
/// # Example
///
/// ```toml
/// # .actup.toml - Per-action overrides
/// [actions."actions/checkout"]
/// update_policy = "major"
///
/// [actions."actions/cache"]
/// ignore = true
///
/// [actions."internal/action"]
/// ref_style = "sha"
/// ```
///
/// ```yaml
/// # Result in workflow:
/// # actions/checkout@v3 -> actions/checkout@v4 (major allowed)
/// # actions/cache@v3 -> unchanged (ignored)
/// # internal/action@v1 -> internal/action@abc123... # v1.2.0 (pinned to SHA)
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ActionOverride {
    /// Override update policy for this action
    pub update_policy: Option<UpdatePolicy>,

    /// Override ref style for this action
    pub ref_style: Option<RefStyle>,

    /// Whether to completely ignore this action
    pub ignore: bool,
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CacheConfig {
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

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(24 * 60 * 60), // 24 hours
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

/// Provider configuration
///
/// NOTE: Default uses empty strings. The CLI layer sets actual defaults.
/// Core must remain provider-neutral per `AGENTS.md` spec.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Default provider kind
    pub default_kind: String,

    /// Default host (e.g., "github.com")
    pub default_host: String,

    /// Additional provider instances
    pub instances: Vec<ProviderInstance>,
}

/// A configured provider instance
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderInstance {
    /// Provider kind (github, gitlab, gitea, etc.)
    pub kind: String,

    /// Host this instance serves
    pub host: String,

    /// API base URL (for enterprise/self-hosted)
    pub api_base: Option<String>,

    /// Environment variable name for token
    pub token_env: Option<String>,
}

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
            path: loaded_from
                .as_ref()
                .map_or_else(|| "config".into(), |p| p.display().to_string()),
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

                figment.merge(Json::string(&stripped))
            }
            // Default to TOML for .toml or unknown extensions
            _ => figment.merge(Toml::file(path)),
        })
    }

    /// Discover and load config from standard locations (convenience method)
    ///
    /// Equivalent to `Config::load(None, start_dir)` but returns defaults on error.
    #[must_use]
    pub fn discover(start_dir: &Path) -> Self {
        Self::load(None, start_dir).unwrap_or_default()
    }

    /// Get the effective update policy for an action
    #[must_use]
    pub fn policy_for(&self, owner: &str, repo: &str) -> UpdatePolicy {
        let key = format!("{owner}/{repo}");
        self.action_overrides
            .get(&key)
            .and_then(|o| o.update_policy)
            .unwrap_or(self.update_policy)
    }

    /// Get the effective ref style for an action
    #[must_use]
    pub fn ref_style_for(&self, owner: &str, repo: &str) -> RefStyle {
        let key = format!("{owner}/{repo}");
        self.action_overrides
            .get(&key)
            .and_then(|o| o.ref_style)
            .unwrap_or(self.ref_style)
    }

    /// Check if an action should be ignored
    #[must_use]
    pub fn should_ignore(&self, owner: &str, repo: &str) -> bool {
        let full = format!("{owner}/{repo}");

        // Check explicit override
        if let Some(ovr) = self.action_overrides.get(&full)
            && ovr.ignore
        {
            return true;
        }

        // Check ignore list
        self.ignore.iter().any(|pattern| {
            // Simple matching: exact or owner/* glob
            if pattern == &full {
                return true;
            }
            if let Some(owner_pattern) = pattern.strip_suffix("/*") {
                return owner == owner_pattern;
            }
            false
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_ignore() {
        let config = Config {
            ignore: vec!["actions/checkout".to_string(), "my-org/*".to_string()],
            ..Default::default()
        };

        assert!(config.should_ignore("actions", "checkout"));
        assert!(config.should_ignore("my-org", "anything"));
        assert!(!config.should_ignore("other", "repo"));
    }

    #[test]
    fn test_policy_override() {
        let config = Config {
            update_policy: UpdatePolicy::Minor,
            action_overrides: std::collections::HashMap::from([(
                "special/action".to_string(),
                ActionOverride {
                    update_policy: Some(UpdatePolicy::Patch),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        assert_eq!(config.policy_for("normal", "action"), UpdatePolicy::Minor);
        assert_eq!(config.policy_for("special", "action"), UpdatePolicy::Patch);
    }
}
