//! Provider registration types.
//!
//! Provider crates export a static [`Registration`] that declares presets,
//! capabilities, and a factory function. Core uses these during config
//! resolution to fill defaults and validate host entries — without depending
//! on any provider crate directly.

use std::num::NonZeroU8;

use super::{ApiMode, HostKind};
use crate::provider::Provider;

/// Host-specific defaults a provider may register (zero-config ergonomics).
///
/// Each preset declares the canonical host, API base URL, and default
/// token environment variable. When a user's config omits these fields
/// for a preset host, the preset values are used as defaults.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    /// Canonical hostname (e.g. `"github.com"`).
    pub host: &'static str,
    /// Default API base URL (e.g. `"https://api.github.com"`).
    pub api_base: &'static str,
    /// Default env var name for auth token (e.g. `"GITHUB_TOKEN"`).
    pub token_env: &'static str,
}

/// Provider capabilities — which API modes it supports.
///
/// Used during config validation to reject `api_mode` values the
/// provider cannot handle (e.g. `graphql` for a REST-only provider).
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Whether the provider supports GraphQL batched queries.
    pub supports_graphql: bool,
    /// Whether the provider supports REST API requests.
    pub supports_rest: bool,
}

/// Resolved host configuration passed to [`Registration::create`].
///
/// This is the fully-merged, validated result of combining preset
/// defaults, user overrides, and provider inference for a single host.
/// All required fields are guaranteed present.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// The hostname this provider instance serves.
    pub host: String,
    /// Resolved provider kind.
    pub kind: HostKind,
    /// Validated API base URL (scheme required, no query/fragment).
    pub api_base: String,
    /// Env var name for auth token.
    pub token_env: String,
    /// Effective API mode (from per-host override, `[fetch]`, or default).
    pub api_mode: ApiMode,
    /// Effective max tags per repo.
    pub max_tags_per_repo: NonZeroU8,
}

/// Provider registration exported by provider crates.
///
/// Each provider crate defines a single `pub static REGISTRATION: Registration`
/// that core's resolution logic uses to:
/// 1. Look up presets by hostname
/// 2. Infer `api_base` for non-preset hosts
/// 3. Validate `api_mode` against capabilities
/// 4. Construct the provider implementation
///
/// # Invariants
///
/// - `presets` hostnames must be unique across *all* registrations.
///   Duplicates are detected at startup and cause a fatal error.
/// - `create` must not panic; return `Err` for unrecoverable failures.
pub struct Registration {
    /// Which provider kind this registration is for.
    pub kind: HostKind,

    /// Well-known host presets (zero or more).
    pub presets: &'static [Preset],

    /// Default token env for non-preset hosts of this kind.
    /// If provided, used as a non-user-specified default (missing env var = warn, not error).
    pub default_token_env: Option<&'static str>,

    /// Infer an `api_base` URL for a non-preset host of this kind.
    /// Return `None` to require `api_base` in user config.
    pub infer_api_base: fn(hostname: &str) -> Option<String>,

    /// Supported API modes.
    pub capabilities: Capabilities,

    /// Construct a provider from fully-resolved config.
    pub create: fn(ProviderConfig) -> crate::Result<Box<dyn Provider>>,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("kind", &self.kind)
            .field("presets", &self.presets)
            .field("default_token_env", &self.default_token_env)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn preset_construction() {
        let p = Preset {
            host: "github.com",
            api_base: "https://api.github.com",
            token_env: "GITHUB_TOKEN",
        };
        assert_eq!(p.host, "github.com");
        assert_eq!(p.api_base, "https://api.github.com");
        assert_eq!(p.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn capabilities_all_modes() {
        let c = Capabilities {
            supports_graphql: true,
            supports_rest: true,
        };
        assert!(c.supports_graphql);
        assert!(c.supports_rest);
    }

    #[test]
    fn capabilities_rest_only() {
        let c = Capabilities {
            supports_graphql: false,
            supports_rest: true,
        };
        assert!(!c.supports_graphql);
        assert!(c.supports_rest);
    }

    #[test]
    fn capabilities_graphql_only() {
        let c = Capabilities {
            supports_graphql: true,
            supports_rest: false,
        };
        assert!(c.supports_graphql);
        assert!(!c.supports_rest);
    }

    #[test]
    fn provider_config_construction() {
        let pc = ProviderConfig {
            host: "ghe.corp.com".into(),
            kind: HostKind::Github,
            api_base: "https://ghe.corp.com/api/v3".into(),
            token_env: "GHE_TOKEN".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(50).unwrap(),
        };
        assert_eq!(pc.host, "ghe.corp.com");
        assert_eq!(pc.kind, HostKind::Github);
        assert_eq!(pc.api_base, "https://ghe.corp.com/api/v3");
        assert_eq!(pc.token_env, "GHE_TOKEN");
        assert_eq!(pc.api_mode, ApiMode::Rest);
        assert_eq!(pc.max_tags_per_repo.get(), 50);
    }

    #[test]
    fn provider_config_default_values() {
        let pc = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::Github,
            api_base: "https://api.github.com".into(),
            token_env: "GITHUB_TOKEN".into(),
            api_mode: ApiMode::default(),
            max_tags_per_repo: super::super::DEFAULT_MAX_TAGS_PER_REPO,
        };
        assert_eq!(pc.api_mode, ApiMode::Graphql);
        assert_eq!(pc.max_tags_per_repo.get(), 10);
    }

    /// Mock `create` fn for testing `Registration` construction
    fn mock_create(_config: ProviderConfig) -> crate::Result<Box<dyn Provider>> {
        Err(crate::Error::ProviderNotFound {
            host: "mock".into(),
        })
    }

    #[test]
    fn registration_construction() {
        let reg = Registration {
            kind: HostKind::Github,
            presets: &[Preset {
                host: "github.com",
                api_base: "https://api.github.com",
                token_env: "GITHUB_TOKEN",
            }],
            default_token_env: Some("GITHUB_TOKEN"),
            infer_api_base: |host| {
                if host == "github.com" {
                    None
                } else {
                    Some(format!("https://{host}/api/v3"))
                }
            },
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: mock_create,
        };
        assert_eq!(reg.kind, HostKind::Github);
        assert_eq!(reg.presets.len(), 1);
        assert_eq!(reg.presets[0].host, "github.com");
        assert_eq!(reg.default_token_env, Some("GITHUB_TOKEN"));
        assert!(reg.capabilities.supports_graphql);
        assert!(reg.capabilities.supports_rest);
    }

    #[test]
    fn registration_infer_api_base() {
        let reg = Registration {
            kind: HostKind::Github,
            presets: &[],
            default_token_env: None,
            infer_api_base: |host| {
                if host == "github.com" {
                    None
                } else {
                    Some(format!("https://{host}/api/v3"))
                }
            },
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: mock_create,
        };

        // Preset host — no inference needed
        assert_eq!((reg.infer_api_base)("github.com"), None);

        // Non-preset host — inference kicks in
        assert_eq!(
            (reg.infer_api_base)("ghe.corp.com"),
            Some("https://ghe.corp.com/api/v3".into())
        );
    }

    #[test]
    fn registration_create_returns_error() {
        let reg = Registration {
            kind: HostKind::Gitlab,
            presets: &[],
            default_token_env: None,
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: false,
                supports_rest: true,
            },
            create: mock_create,
        };

        let config = ProviderConfig {
            host: "gitlab.com".into(),
            kind: HostKind::Gitlab,
            api_base: "https://gitlab.com/api/v4".into(),
            token_env: "GITLAB_TOKEN".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        };

        let result = (reg.create)(config);
        assert!(result.is_err());
    }

    #[test]
    fn registration_debug_impl() {
        let reg = Registration {
            kind: HostKind::Github,
            presets: &[Preset {
                host: "github.com",
                api_base: "https://api.github.com",
                token_env: "GITHUB_TOKEN",
            }],
            default_token_env: Some("GITHUB_TOKEN"),
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: mock_create,
        };
        let debug = format!("{reg:?}");
        assert!(debug.contains("Registration"));
        assert!(debug.contains("Github"));
    }

    #[test]
    fn registration_multiple_presets() {
        let reg = Registration {
            kind: HostKind::Github,
            presets: &[
                Preset {
                    host: "github.com",
                    api_base: "https://api.github.com",
                    token_env: "GITHUB_TOKEN",
                },
                Preset {
                    host: "github.dev",
                    api_base: "https://api.github.dev",
                    token_env: "GITHUB_DEV_TOKEN",
                },
            ],
            default_token_env: Some("GITHUB_TOKEN"),
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: mock_create,
        };
        assert_eq!(reg.presets.len(), 2);
        assert_eq!(reg.presets[0].host, "github.com");
        assert_eq!(reg.presets[1].host, "github.dev");
    }

    #[test]
    fn registration_no_presets() {
        let reg = Registration {
            kind: HostKind::Gitea,
            presets: &[],
            default_token_env: None,
            infer_api_base: |host| Some(format!("https://{host}/api/v1")),
            capabilities: Capabilities {
                supports_graphql: false,
                supports_rest: true,
            },
            create: mock_create,
        };
        assert!(reg.presets.is_empty());
        assert_eq!(reg.default_token_env, None);
    }

    #[test]
    fn preset_is_copy() {
        let p = Preset {
            host: "github.com",
            api_base: "https://api.github.com",
            token_env: "GITHUB_TOKEN",
        };
        let p2 = p; // Copy
        let p3 = p; // still usable after copy
        assert_eq!(p2.host, p3.host);
    }

    #[test]
    fn preset_clone_eq() {
        let p = Preset {
            host: "gitlab.com",
            api_base: "https://gitlab.com/api/v4",
            token_env: "GITLAB_TOKEN",
        };
        #[allow(clippy::clone_on_copy)]
        let p2 = p.clone();
        assert_eq!(p.host, p2.host);
        assert_eq!(p.api_base, p2.api_base);
        assert_eq!(p.token_env, p2.token_env);
    }

    #[test]
    fn preset_debug_output() {
        let p = Preset {
            host: "gitea.example.com",
            api_base: "https://gitea.example.com/api/v1",
            token_env: "GITEA_TOKEN",
        };
        let debug = format!("{p:?}");
        assert!(debug.contains("Preset"));
        assert!(debug.contains("gitea.example.com"));
        assert!(debug.contains("GITEA_TOKEN"));
    }

    #[test]
    fn preset_gitlab_pattern() {
        let p = Preset {
            host: "gitlab.com",
            api_base: "https://gitlab.com/api/v4",
            token_env: "GITLAB_TOKEN",
        };
        assert_eq!(p.host, "gitlab.com");
        assert!(p.api_base.ends_with("/v4"));
    }

    #[test]
    fn preset_gitea_pattern() {
        let p = Preset {
            host: "gitea.example.com",
            api_base: "https://gitea.example.com/api/v1",
            token_env: "GITEA_TOKEN",
        };
        assert_eq!(p.host, "gitea.example.com");
        assert!(p.api_base.ends_with("/v1"));
    }

    #[test]
    fn capabilities_no_modes() {
        let c = Capabilities {
            supports_graphql: false,
            supports_rest: false,
        };
        assert!(!c.supports_graphql);
        assert!(!c.supports_rest);
    }

    #[test]
    fn capabilities_is_copy() {
        let c = Capabilities {
            supports_graphql: true,
            supports_rest: false,
        };
        let c2 = c; // Copy
        let c3 = c; // still usable
        assert_eq!(c2.supports_graphql, c3.supports_graphql);
        assert_eq!(c2.supports_rest, c3.supports_rest);
    }

    #[test]
    fn capabilities_debug_output() {
        let c = Capabilities {
            supports_graphql: true,
            supports_rest: true,
        };
        let debug = format!("{c:?}");
        assert!(debug.contains("Capabilities"));
        assert!(debug.contains("supports_graphql: true"));
        assert!(debug.contains("supports_rest: true"));
    }

    #[test]
    fn provider_config_clone() {
        let pc = ProviderConfig {
            host: "ghe.corp.com".into(),
            kind: HostKind::Github,
            api_base: "https://ghe.corp.com/api/v3".into(),
            token_env: "GHE_TOKEN".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(25).unwrap(),
        };
        let pc2 = pc.clone();
        assert_eq!(pc.host, pc2.host);
        assert_eq!(pc.kind, pc2.kind);
        assert_eq!(pc.api_base, pc2.api_base);
        assert_eq!(pc.token_env, pc2.token_env);
        assert_eq!(pc.api_mode, pc2.api_mode);
        assert_eq!(pc.max_tags_per_repo, pc2.max_tags_per_repo);
    }

    #[test]
    fn provider_config_debug_output() {
        let pc = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::Github,
            api_base: "https://api.github.com".into(),
            token_env: "GITHUB_TOKEN".into(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        };
        let debug = format!("{pc:?}");
        assert!(debug.contains("ProviderConfig"));
        assert!(debug.contains("github.com"));
        assert!(debug.contains("GITHUB_TOKEN"));
    }

    #[test]
    fn provider_config_gitlab() {
        let pc = ProviderConfig {
            host: "gitlab.com".into(),
            kind: HostKind::Gitlab,
            api_base: "https://gitlab.com/api/v4".into(),
            token_env: "GITLAB_TOKEN".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(20).unwrap(),
        };
        assert_eq!(pc.kind, HostKind::Gitlab);
        assert_eq!(pc.api_mode, ApiMode::Rest);
    }

    #[test]
    fn provider_config_gitea() {
        let pc = ProviderConfig {
            host: "gitea.example.com".into(),
            kind: HostKind::Gitea,
            api_base: "https://gitea.example.com/api/v1".into(),
            token_env: "GITEA_TOKEN".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(5).unwrap(),
        };
        assert_eq!(pc.kind, HostKind::Gitea);
        assert_eq!(pc.max_tags_per_repo.get(), 5);
    }

    #[test]
    fn provider_config_max_tags_boundary() {
        let pc = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::Github,
            api_base: "https://api.github.com".into(),
            token_env: "GITHUB_TOKEN".into(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(255).unwrap(),
        };
        assert_eq!(pc.max_tags_per_repo.get(), 255);
    }

    #[test]
    fn registration_debug_excludes_fn_pointers() {
        let reg = Registration {
            kind: HostKind::Github,
            presets: &[],
            default_token_env: None,
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: mock_create,
        };
        let debug = format!("{reg:?}");
        // `finish_non_exhaustive` produces `..` to indicate omitted fields
        assert!(debug.contains(".."));
        // fn pointer fields should NOT appear in debug output
        assert!(!debug.contains("infer_api_base"));
        assert!(!debug.contains("create"));
    }

    #[test]
    fn registration_infer_always_none() {
        let reg = Registration {
            kind: HostKind::Gitlab,
            presets: &[Preset {
                host: "gitlab.com",
                api_base: "https://gitlab.com/api/v4",
                token_env: "GITLAB_TOKEN",
            }],
            default_token_env: Some("GITLAB_TOKEN"),
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: false,
                supports_rest: true,
            },
            create: mock_create,
        };
        // Non-preset host: inference returns None (requires user-supplied api_base)
        assert_eq!((reg.infer_api_base)("self-hosted.corp.com"), None);
        assert_eq!((reg.infer_api_base)("gitlab.com"), None);
    }

    #[test]
    fn registration_gitea_infer_api_base() {
        let reg = Registration {
            kind: HostKind::Gitea,
            presets: &[],
            default_token_env: None,
            infer_api_base: |host| Some(format!("https://{host}/api/v1")),
            capabilities: Capabilities {
                supports_graphql: false,
                supports_rest: true,
            },
            create: mock_create,
        };
        assert_eq!(
            (reg.infer_api_base)("gitea.example.com"),
            Some("https://gitea.example.com/api/v1".into())
        );
        assert_eq!(
            (reg.infer_api_base)("code.internal.org"),
            Some("https://code.internal.org/api/v1".into())
        );
    }

    #[test]
    fn registration_create_receives_correct_config() {
        fn capturing_create(config: ProviderConfig) -> crate::Result<Box<dyn Provider>> {
            // Verify the config fields are passed through correctly
            assert_eq!(config.host, "github.com");
            assert_eq!(config.kind, HostKind::Github);
            assert_eq!(config.api_base, "https://api.github.com");
            assert_eq!(config.api_mode, ApiMode::Graphql);
            Err(crate::Error::ProviderNotFound { host: config.host })
        }

        let reg = Registration {
            kind: HostKind::Github,
            presets: &[],
            default_token_env: None,
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: capturing_create,
        };

        let config = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::Github,
            api_base: "https://api.github.com".into(),
            token_env: "GITHUB_TOKEN".into(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        };

        // create fn receives the full ProviderConfig
        let result = (reg.create)(config);
        assert!(result.is_err());
    }

    #[test]
    fn registration_debug_includes_preset_details() {
        let reg = Registration {
            kind: HostKind::Github,
            presets: &[
                Preset {
                    host: "github.com",
                    api_base: "https://api.github.com",
                    token_env: "GITHUB_TOKEN",
                },
                Preset {
                    host: "ghe.corp.com",
                    api_base: "https://ghe.corp.com/api/v3",
                    token_env: "GHE_TOKEN",
                },
            ],
            default_token_env: Some("GITHUB_TOKEN"),
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: true,
            },
            create: mock_create,
        };
        let debug = format!("{reg:?}");
        // Debug includes both presets and capabilities
        assert!(debug.contains("github.com"));
        assert!(debug.contains("ghe.corp.com"));
        assert!(debug.contains("Capabilities"));
        assert!(debug.contains("default_token_env"));
    }
}
