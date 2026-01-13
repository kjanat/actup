//! Configuration types and discovery
//!
//! Configuration is loaded from multiple sources with precedence:
//! 1. Environment variables (ACTUP_*) - highest
//! 2. Explicit --config path
//! 3. Repo configs (actup.toml, .actup.toml, .config/actup.toml)
//! 4. User/global config (~/.config/actup/config.toml) - fills gaps only
//! 5. Defaults (lowest)

use crate::Error;
use crate::policy::{BranchPolicy, RefStyle, UpdatePolicy};
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml, Yaml};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Main configuration struct
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

impl Default for Config {
    fn default() -> Self {
        Self {
            update_policy: UpdatePolicy::default(),
            ref_style: RefStyle::default(),
            branch_policy: BranchPolicy::default(),
            allow_prerelease: false,
            ignore: Vec::new(),
            action_overrides: HashMap::new(),
            cache: CacheConfig::default(),
            providers: ProvidersConfig::default(),
            extra_paths: Vec::new(),
            loaded_from: None,
        }
    }
}

/// Per-action override configuration
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
    #[serde(deserialize_with = "deserialize_duration_secs")]
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

/// Deserialize Duration from seconds (u64)
fn deserialize_duration_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

/// Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Default provider kind
    pub default_kind: String,

    /// Default host (e.g., "github.com")
    pub default_host: String,

    /// Additional provider instances
    pub instances: Vec<ProviderInstance>,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            default_kind: "github".to_string(),
            default_host: "github.com".to_string(),
            instances: Vec::new(),
        }
    }
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

/// Config file names to search for (in order)
const CONFIG_FILE_NAMES: &[&str] = &[
    "actup.toml",
    ".actup.toml",
    ".config/actup.toml",
    "actup.yaml",
    "actup.yml",
];

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
    /// 2. Explicit config_path (if provided)
    /// 3. Repo config (searched upward from start_dir)
    /// 4. User config (~/.config/actup/config.toml) - fills gaps only
    /// 5. Defaults
    ///
    /// # Errors
    /// Returns error if config file exists but is invalid.
    pub fn load(config_path: Option<&Path>, start_dir: &Path) -> Result<Self, Error> {
        let mut figment = Figment::new().merge(Serialized::defaults(Config::default()));

        // User config fills gaps (lowest precedence file)
        if let Some(user_config) = Self::user_config_path() {
            if user_config.exists() {
                figment = figment.join(Toml::file(&user_config));
            }
        }

        // Repo config (searched upward from start_dir)
        let mut loaded_from = None;
        if let Some(repo_config) = Self::find_repo_config_path(start_dir) {
            figment = match repo_config.extension().and_then(|e| e.to_str()) {
                Some("toml") => figment.merge(Toml::file(&repo_config)),
                Some("yaml" | "yml") => figment.merge(Yaml::file(&repo_config)),
                _ => figment,
            };
            loaded_from = Some(repo_config);
        }

        // Explicit --config path (high precedence)
        if let Some(path) = config_path {
            figment = match path.extension().and_then(|e| e.to_str()) {
                Some("toml") => figment.merge(Toml::file(path)),
                Some("yaml" | "yml") => figment.merge(Yaml::file(path)),
                _ => figment.merge(Toml::file(path)), // Default to TOML
            };
            loaded_from = Some(path.to_path_buf());
        }

        // Environment variables (highest precedence)
        figment = figment.merge(Env::prefixed("ACTUP_").split("_"));

        let mut config: Config = figment.extract().map_err(|e| Error::InvalidConfig {
            path: loaded_from
                .as_ref()
                .map_or_else(|| "config".into(), |p| p.display().to_string()),
            reason: e.to_string(),
        })?;

        config.loaded_from = loaded_from;
        Ok(config)
    }

    /// Discover and load config from standard locations (convenience method)
    ///
    /// Equivalent to `Config::load(None, start_dir)` but returns defaults on error.
    #[must_use]
    pub fn discover(start_dir: &Path) -> Self {
        Self::load(None, start_dir).unwrap_or_default()
    }

    /// Find repo config path, searching upward from start_dir
    fn find_repo_config_path(start_dir: &Path) -> Option<PathBuf> {
        let mut dir = start_dir.to_path_buf();

        loop {
            for name in CONFIG_FILE_NAMES {
                let path = dir.join(name);
                if path.exists() {
                    return Some(path);
                }
            }

            // Move up one directory
            if !dir.pop() {
                break;
            }
        }

        None
    }

    /// Get user config path
    fn user_config_path() -> Option<PathBuf> {
        let config_dir = dirs::config_dir()?;
        Some(config_dir.join("actup").join("config.toml"))
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
        if let Some(ovr) = self.action_overrides.get(&full) {
            if ovr.ignore {
                return true;
            }
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
        let mut config = Config::default();
        config.ignore = vec!["actions/checkout".to_string(), "my-org/*".to_string()];

        assert!(config.should_ignore("actions", "checkout"));
        assert!(config.should_ignore("my-org", "anything"));
        assert!(!config.should_ignore("other", "repo"));
    }

    #[test]
    fn test_policy_override() {
        let mut config = Config::default();
        config.update_policy = UpdatePolicy::Minor;
        config.action_overrides.insert(
            "special/action".to_string(),
            ActionOverride {
                update_policy: Some(UpdatePolicy::Patch),
                ..Default::default()
            },
        );

        assert_eq!(config.policy_for("normal", "action"), UpdatePolicy::Minor);
        assert_eq!(config.policy_for("special", "action"), UpdatePolicy::Patch);
    }
}
