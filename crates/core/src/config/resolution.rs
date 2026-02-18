//! Config resolution logic.
//!
//! Resolves a hostname into a fully-validated [`ProviderConfig`] by merging
//! provider presets, user `[hosts."hostname"]` overrides, and provider
//! inference/defaults.
//!
//! The entry point is [`effective_host`], which implements the 7-step
//! algorithm from the config-refactor spec.

use std::collections::HashMap;

use super::registration::{ProviderConfig, Registration};
use super::validation::{validate_api_base, validate_hostname, validate_provider_config};
use super::{ApiMode, Config, FetchSettings, HostKind};
use crate::Error;

/// Index mapping preset hostnames to their (registration, preset) pair.
///
/// Built once from the list of available registrations and reused for
/// all hostname lookups within a single config resolution pass.
type PresetIndex<'a> = HashMap<&'a str, (&'a Registration, &'a super::registration::Preset)>;

/// Build a preset index from available registrations.
///
/// Maps each preset hostname to the registration and preset that own it.
/// Callers should have already validated no duplicate preset hostnames
/// via [`crate::validate_no_duplicate_presets`].
fn build_preset_index<'a>(registrations: &'a [&'a Registration]) -> PresetIndex<'a> {
    let mut index = HashMap::new();
    for reg in registrations {
        for preset in reg.presets {
            index.insert(preset.host, (*reg, preset));
        }
    }
    index
}

/// Find a registration by [`HostKind`] in the available registrations.
///
/// Returns `None` if no registration matches the requested kind —
/// meaning the provider is not compiled into this build.
fn find_registration_by_kind<'a>(
    registrations: &'a [&'a Registration],
    kind: HostKind,
) -> Option<&'a Registration> {
    registrations.iter().find(|r| r.kind == kind).copied()
}

/// Resolve a hostname into a fully-validated [`ProviderConfig`].
///
/// Implements the 7-step resolution algorithm:
///
/// 1. Start from preset if hostname matches a registered preset, else empty.
///    If preset is chosen, `kind` is implied by `registration.kind`.
/// 2. Merge user `[hosts."hostname"]` overrides (user `Some` fields win).
/// 3. If `kind` is still missing: error (non-preset host must specify `kind`).
/// 4. Lookup registration by `kind` (error if provider not compiled in).
/// 5. Fill defaults/inference:
///    - `token_env`: if missing and `registration.default_token_env` set, use it
///    - `api_base`: if missing, try `registration.infer_api_base(hostname)`
/// 6. Validate `kind`, `api_base`, `token_env` present + `api_base` URL shape.
/// 7. Validate `api_mode` compatibility with `registration.capabilities`.
///
/// `fetch` provides the global `[fetch]` settings for `api_mode` and
/// `max_tags_per_repo` fallbacks.
///
/// # Errors
///
/// Returns `Error::InvalidConfig` if:
/// - hostname fails validation
/// - `kind` is missing for a non-preset host
/// - the provider for `kind` is not compiled in
/// - required fields (`api_base`, `token_env`) are missing after merge+inference
/// - `api_base` URL shape is invalid
/// - `api_mode` is unsupported by the provider's capabilities
pub fn effective_host(
    hostname: &str,
    config: &Config,
    registrations: &[&Registration],
    fetch: &FetchSettings,
) -> Result<ProviderConfig, Error> {
    // Validate hostname syntax
    validate_hostname(hostname, &format!("host \"{hostname}\""))?;

    let preset_index = build_preset_index(registrations);

    // Step 1: Start from preset if present
    let (mut kind, mut api_base, mut token_env) =
        if let Some((reg, preset)) = preset_index.get(hostname) {
            // Preset found — kind is implied by registration
            (
                Some(reg.kind),
                Some(preset.api_base.to_string()),
                Some(preset.token_env.to_string()),
            )
        } else {
            (None, None, None)
        };

    // Step 2: Merge user [hosts."hostname"] overrides
    if let Some(host_config) = config.hosts.get(hostname) {
        if let Some(k) = host_config.kind {
            kind = Some(k);
        }
        if let Some(ref ab) = host_config.api_base {
            api_base = Some(ab.clone());
        }
        if let Some(ref te) = host_config.token_env {
            token_env = Some(te.clone());
        }
    }

    // Step 3: kind must be known by now
    let resolved_kind = kind.ok_or_else(|| Error::InvalidConfig {
        path: format!("host \"{hostname}\""),
        reason: format!(
            "kind required for \"{hostname}\" (not a preset host; specify kind in [hosts.\"{hostname}\"])"
        ),
    })?;

    // Step 4: Lookup registration by kind
    let registration =
        find_registration_by_kind(registrations, resolved_kind).ok_or_else(|| {
            let available: Vec<_> = registrations.iter().map(|r| r.kind.to_string()).collect();
            Error::InvalidConfig {
                path: format!("host \"{hostname}\""),
                reason: format!(
                    "provider {:?} not compiled in (available: {})",
                    resolved_kind,
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                ),
            }
        })?;

    // Step 5: Fill defaults/inference
    if token_env.is_none()
        && let Some(default_te) = registration.default_token_env
    {
        token_env = Some(default_te.to_string());
    }

    if api_base.is_none() {
        api_base = (registration.infer_api_base)(hostname);
    }

    // Resolve api_mode: per-host override > [fetch] > default
    let host_config = config.hosts.get(hostname);
    let api_mode = host_config
        .and_then(|hc| hc.api_mode)
        .unwrap_or(fetch.api_mode);

    // Resolve max_tags: per-host override > [fetch] > default
    let max_tags_per_repo = host_config
        .and_then(|hc| hc.max_tags_per_repo)
        .unwrap_or(fetch.max_tags_per_repo);

    // Steps 6-7: Build, validate, and check capability compatibility
    build_and_validate(
        hostname,
        resolved_kind,
        api_base,
        token_env,
        api_mode,
        max_tags_per_repo,
        registration,
    )
}

/// Build a [`ProviderConfig`] from resolved fields and validate it (steps 6-7).
///
/// Checks required fields are present, normalizes `api_base`, runs
/// post-merge validation, and verifies `api_mode` against capabilities.
fn build_and_validate(
    hostname: &str,
    kind: HostKind,
    api_base: Option<String>,
    token_env: Option<String>,
    api_mode: ApiMode,
    max_tags_per_repo: std::num::NonZeroU8,
    registration: &Registration,
) -> Result<ProviderConfig, Error> {
    let ctx = format!("host \"{hostname}\"");

    // Step 6: Validate required fields present
    let resolved_api_base = api_base.ok_or_else(|| Error::InvalidConfig {
        path: ctx.clone(),
        reason: format!("api_base required for \"{hostname}\" (provider cannot infer; specify api_base in [hosts.\"{hostname}\"])"),
    })?;

    let resolved_token_env = token_env.ok_or_else(|| Error::InvalidConfig {
        path: ctx.clone(),
        reason: format!(
            "token_env required for \"{hostname}\" (specify token_env in [hosts.\"{hostname}\"])"
        ),
    })?;

    let normalized_api_base = validate_api_base(&resolved_api_base, &ctx)?;

    let provider_config = ProviderConfig {
        host: hostname.to_string(),
        kind,
        api_base: normalized_api_base,
        token_env: resolved_token_env,
        api_mode,
        max_tags_per_repo,
    };

    validate_provider_config(&provider_config)?;

    // Step 7: Validate api_mode against capabilities
    validate_api_mode_capability(hostname, kind, api_mode, registration.capabilities)?;

    Ok(provider_config)
}

/// Check that the requested `api_mode` is supported by the provider.
fn validate_api_mode_capability(
    hostname: &str,
    kind: HostKind,
    api_mode: ApiMode,
    capabilities: super::registration::Capabilities,
) -> Result<(), Error> {
    match api_mode {
        ApiMode::Graphql if !capabilities.supports_graphql => Err(Error::InvalidConfig {
            path: format!("host \"{hostname}\""),
            reason: format!(
                "api_mode \"graphql\" not supported by {kind} provider (supports: {})",
                capability_list(capabilities),
            ),
        }),
        ApiMode::Rest if !capabilities.supports_rest => Err(Error::InvalidConfig {
            path: format!("host \"{hostname}\""),
            reason: format!(
                "api_mode \"rest\" not supported by {kind} provider (supports: {})",
                capability_list(capabilities),
            ),
        }),
        _ => Ok(()),
    }
}

/// Resolve the effective default host for bare `owner/repo` refs.
///
/// If `config.default_host` is `Some`, validates it resolves via [`effective_host`].
/// If `None`, looks for a preset matching `"github.com"` — if found, returns it;
/// otherwise errors.
///
/// # Errors
///
/// Returns `Error::InvalidConfig` if:
/// - `default_host` is set but fails hostname validation
/// - `default_host` is set but doesn't resolve (no matching preset or `[hosts]` entry)
/// - no `default_host` set and no `"github.com"` preset available
pub fn resolve_default_host(
    config: &Config,
    registrations: &[&Registration],
) -> Result<String, Error> {
    if let Some(ref host) = config.default_host {
        // Validate the hostname itself
        validate_hostname(host, "default_host")?;

        // Verify it can resolve (must be a preset or have a [hosts] entry)
        let preset_index = build_preset_index(registrations);
        let has_preset = preset_index.contains_key(host.as_str());
        let has_host_entry = config.hosts.contains_key(host.as_str());

        if !has_preset && !has_host_entry {
            return Err(Error::InvalidConfig {
                path: "default_host".to_string(),
                reason: format!(
                    "\"{host}\" is not a preset host and has no [hosts.\"{host}\"] entry"
                ),
            });
        }

        Ok(host.clone())
    } else {
        // No explicit default_host — look for github.com preset
        let preset_index = build_preset_index(registrations);
        if preset_index.contains_key("github.com") {
            Ok("github.com".to_string())
        } else {
            Err(Error::InvalidConfig {
                path: "default_host".to_string(),
                reason: "no default_host set and no \"github.com\" preset available".to_string(),
            })
        }
    }
}

/// Format a human-readable list of supported API modes from capabilities.
fn capability_list(caps: super::registration::Capabilities) -> String {
    let mut modes = Vec::new();
    if caps.supports_rest {
        modes.push("rest");
    }
    if caps.supports_graphql {
        modes.push("graphql");
    }
    if modes.is_empty() {
        "none".to_string()
    } else {
        modes.join(", ")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::num::NonZeroU8;

    use super::*;
    use crate::config::HostConfig;
    use crate::config::registration::{Capabilities, Preset, Registration};

    fn mock_create(_config: ProviderConfig) -> crate::Result<Box<dyn crate::provider::Provider>> {
        Err(crate::Error::ProviderNotFound {
            host: "mock".into(),
        })
    }

    fn github_registration() -> Registration {
        Registration {
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
        }
    }

    fn gitlab_rest_only_registration() -> Registration {
        Registration {
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
        }
    }

    fn gitea_registration() -> Registration {
        Registration {
            kind: HostKind::Gitea,
            presets: &[],
            default_token_env: None,
            infer_api_base: |host| Some(format!("https://{host}/api/v1")),
            capabilities: Capabilities {
                supports_graphql: false,
                supports_rest: true,
            },
            create: mock_create,
        }
    }

    fn default_fetch() -> FetchSettings {
        FetchSettings::default()
    }

    // === effective_host: preset-only host ===

    #[test]
    fn preset_only_github_com() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.host, "github.com");
        assert_eq!(result.kind, HostKind::Github);
        assert_eq!(result.api_base, "https://api.github.com");
        assert_eq!(result.token_env, "GITHUB_TOKEN");
        assert_eq!(result.api_mode, ApiMode::Graphql);
        assert_eq!(result.max_tags_per_repo.get(), 10);
    }

    #[test]
    fn preset_only_gitlab_com() {
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gl];
        let config = Config::default();

        // GitLab is REST-only; default api_mode is Graphql, which should fail
        // unless we override. Use REST fetch settings.
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let result = effective_host("gitlab.com", &config, &regs, &fetch).unwrap();
        assert_eq!(result.kind, HostKind::Gitlab);
        assert_eq!(result.api_base, "https://gitlab.com/api/v4");
        assert_eq!(result.token_env, "GITLAB_TOKEN");
    }

    // === effective_host: user override of preset ===

    #[test]
    fn user_overrides_preset_token_env() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: Some("MY_CUSTOM_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.token_env, "MY_CUSTOM_TOKEN");
        // Other preset values unchanged
        assert_eq!(result.api_base, "https://api.github.com");
        assert_eq!(result.kind, HostKind::Github);
    }

    #[test]
    fn user_overrides_preset_api_base() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: Some("https://proxy.corp.com/github".into()),
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_base, "https://proxy.corp.com/github");
    }

    #[test]
    fn user_overrides_preset_api_mode() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: Some(ApiMode::Rest),
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_mode, ApiMode::Rest);
    }

    #[test]
    fn user_overrides_preset_max_tags() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: NonZeroU8::new(50),
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.max_tags_per_repo.get(), 50);
    }

    // === effective_host: non-preset with kind ===

    #[test]
    fn non_preset_ghe_with_kind_and_full_config() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: Some("https://ghe.corp.com/api/v3".into()),
                token_env: Some("GHE_TOKEN".into()),
                api_mode: Some(ApiMode::Rest),
                max_tags_per_repo: NonZeroU8::new(25),
            },
        );

        let result = effective_host("ghe.corp.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.host, "ghe.corp.com");
        assert_eq!(result.kind, HostKind::Github);
        assert_eq!(result.api_base, "https://ghe.corp.com/api/v3");
        assert_eq!(result.token_env, "GHE_TOKEN");
        assert_eq!(result.api_mode, ApiMode::Rest);
        assert_eq!(result.max_tags_per_repo.get(), 25);
    }

    #[test]
    fn non_preset_ghe_with_inference() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: None,  // should be inferred
                token_env: None, // should use default_token_env
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("ghe.corp.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_base, "https://ghe.corp.com/api/v3");
        assert_eq!(result.token_env, "GITHUB_TOKEN");
    }

    // === effective_host: error cases ===

    #[test]
    fn error_missing_kind_non_preset() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "custom.host.com".into(),
            HostConfig {
                kind: None, // no kind!
                api_base: Some("https://custom.host.com/api".into()),
                token_env: Some("TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let err = effective_host("custom.host.com", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("kind required"), "{msg}");
        assert!(msg.contains("custom.host.com"), "{msg}");
    }

    #[test]
    fn error_kind_not_compiled_in() {
        // Only GitHub registration available, but user specifies Gitlab kind
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "gitlab.internal".into(),
            HostConfig {
                kind: Some(HostKind::Gitlab),
                api_base: Some("https://gitlab.internal/api/v4".into()),
                token_env: Some("GL_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let err = effective_host("gitlab.internal", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not compiled in"), "{msg}");
        assert!(msg.contains("available:"), "{msg}");
        assert!(msg.contains("github"), "{msg}");
    }

    #[test]
    fn error_missing_api_base_no_inference() {
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gl];
        let mut config = Config::default();
        config.hosts.insert(
            "gitlab.internal".into(),
            HostConfig {
                kind: Some(HostKind::Gitlab),
                api_base: None, // GitLab infer_api_base returns None
                token_env: Some("GL_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let err = effective_host("gitlab.internal", &config, &regs, &fetch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_base required"), "{msg}");
    }

    #[test]
    fn error_missing_token_env() {
        // Gitea has no presets and no default_token_env
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gitea];
        let mut config = Config::default();
        config.hosts.insert(
            "gitea.local".into(),
            HostConfig {
                kind: Some(HostKind::Gitea),
                api_base: None,  // inferred
                token_env: None, // no default
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let err = effective_host("gitea.local", &config, &regs, &fetch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("token_env required"), "{msg}");
    }

    #[test]
    fn error_invalid_api_mode_graphql_on_rest_only() {
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gl];
        let config = Config::default();

        // Global fetch uses graphql (default), but GitLab is REST-only
        let err = effective_host("gitlab.com", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("graphql"), "{msg}");
        assert!(msg.contains("not supported"), "{msg}");
        assert!(msg.contains("rest"), "{msg}");
    }

    #[test]
    fn error_invalid_hostname() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err =
            effective_host("https://github.com", &config, &regs, &default_fetch()).unwrap_err();
        assert!(err.to_string().contains("scheme"), "{}", err);
    }

    #[test]
    fn error_uppercase_hostname() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err = effective_host("GitHub.com", &config, &regs, &default_fetch()).unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{}", err);
    }

    // === effective_host: fetch settings precedence ===

    #[test]
    fn fetch_api_mode_used_when_no_host_override() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let result = effective_host("github.com", &config, &regs, &fetch).unwrap();
        assert_eq!(result.api_mode, ApiMode::Rest);
    }

    #[test]
    fn host_override_beats_fetch_api_mode() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: Some(ApiMode::Rest),
                max_tags_per_repo: None,
            },
        );

        // Global fetch says graphql, but host override says rest
        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_mode, ApiMode::Rest);
    }

    #[test]
    fn fetch_max_tags_used_when_no_host_override() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(42).unwrap(),
        };

        let result = effective_host("github.com", &config, &regs, &fetch).unwrap();
        assert_eq!(result.max_tags_per_repo.get(), 42);
    }

    // === effective_host: gitea with inference ===

    #[test]
    fn gitea_api_base_inferred() {
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gitea];
        let mut config = Config::default();
        config.hosts.insert(
            "code.internal.org".into(),
            HostConfig {
                kind: Some(HostKind::Gitea),
                api_base: None, // should be inferred
                token_env: Some("GITEA_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let result = effective_host("code.internal.org", &config, &regs, &fetch).unwrap();
        assert_eq!(result.api_base, "https://code.internal.org/api/v1");
        assert_eq!(result.kind, HostKind::Gitea);
    }

    // === effective_host: api_base normalization ===

    #[test]
    fn api_base_trailing_slash_stripped() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: Some("https://api.github.com/".into()),
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_base, "https://api.github.com");
    }

    // === effective_host: hostname not in hosts map and not preset ===

    #[test]
    fn error_unknown_host_no_config() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err = effective_host("unknown.host.com", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("kind required"), "{msg}");
    }

    // === resolve_default_host ===

    #[test]
    fn default_host_from_config() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config {
            default_host: Some("github.com".into()),
            ..Default::default()
        };

        let result = resolve_default_host(&config, &regs).unwrap();
        assert_eq!(result, "github.com");
    }

    #[test]
    fn default_host_falls_back_to_github_preset() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default(); // no explicit default_host

        let result = resolve_default_host(&config, &regs).unwrap();
        assert_eq!(result, "github.com");
    }

    #[test]
    fn default_host_errors_when_no_preset_and_no_config() {
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gitea]; // no github preset
        let config = Config::default();

        let err = resolve_default_host(&config, &regs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no default_host set"), "{msg}");
        assert!(msg.contains("github.com"), "{msg}");
    }

    #[test]
    fn default_host_validates_hostname() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config {
            default_host: Some("GITHUB.COM".into()),
            ..Default::default()
        };

        let err = resolve_default_host(&config, &regs).unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{}", err);
    }

    #[test]
    fn default_host_requires_resolvable() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config {
            default_host: Some("nonexistent.host.com".into()),
            ..Default::default()
        };

        let err = resolve_default_host(&config, &regs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not a preset"), "{msg}");
        assert!(msg.contains("nonexistent.host.com"), "{msg}");
    }

    #[test]
    fn default_host_accepts_hosts_entry() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config {
            default_host: Some("ghe.corp.com".into()),
            ..Default::default()
        };
        config.hosts.insert(
            "ghe.corp.com".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: Some("https://ghe.corp.com/api/v3".into()),
                token_env: Some("GHE_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = resolve_default_host(&config, &regs).unwrap();
        assert_eq!(result, "ghe.corp.com");
    }

    // === build_preset_index ===

    #[test]
    fn preset_index_empty_registrations() {
        let regs: Vec<&Registration> = vec![];
        let index = build_preset_index(&regs);
        assert!(index.is_empty());
    }

    #[test]
    fn preset_index_contains_all_presets() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl];
        let index = build_preset_index(&regs);
        assert!(index.contains_key("github.com"));
        assert!(index.contains_key("gitlab.com"));
        assert_eq!(index.len(), 2);
    }

    // === find_registration_by_kind ===

    #[test]
    fn find_registration_found() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let found = find_registration_by_kind(&regs, HostKind::Github);
        assert!(found.is_some());
        assert_eq!(found.unwrap().kind, HostKind::Github);
    }

    #[test]
    fn find_registration_not_found() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        assert!(find_registration_by_kind(&regs, HostKind::Gitlab).is_none());
    }

    // === capability_list ===

    #[test]
    fn capability_list_both() {
        let caps = Capabilities {
            supports_graphql: true,
            supports_rest: true,
        };
        let s = capability_list(caps);
        assert!(s.contains("rest"));
        assert!(s.contains("graphql"));
    }

    #[test]
    fn capability_list_rest_only() {
        let caps = Capabilities {
            supports_graphql: false,
            supports_rest: true,
        };
        assert_eq!(capability_list(caps), "rest");
    }

    #[test]
    fn capability_list_none() {
        let caps = Capabilities {
            supports_graphql: false,
            supports_rest: false,
        };
        assert_eq!(capability_list(caps), "none");
    }

    // === Multiple registrations ===

    #[test]
    fn multiple_registrations_correct_dispatch() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        // GitHub preset resolves correctly
        let github_result = effective_host("github.com", &config, &regs, &fetch).unwrap();
        assert_eq!(github_result.kind, HostKind::Github);

        // GitLab preset resolves correctly
        let gitlab_result = effective_host("gitlab.com", &config, &regs, &fetch).unwrap();
        assert_eq!(gitlab_result.kind, HostKind::Gitlab);
    }

    // === User kind override changes registration lookup ===

    #[test]
    fn user_kind_override_on_preset_host() {
        // User overrides github.com preset to use Gitlab kind (unusual but valid if
        // a Gitlab registration exists)
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: Some(HostKind::Gitlab),
                api_base: Some("https://gitlab-proxy.corp.com/api/v4".into()),
                token_env: Some("GL_TOKEN".into()),
                api_mode: Some(ApiMode::Rest),
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        // Kind should be overridden to Gitlab
        assert_eq!(result.kind, HostKind::Gitlab);
        assert_eq!(result.api_base, "https://gitlab-proxy.corp.com/api/v4");
    }

    // === Step 1: preset lookup (additional coverage) ===

    #[test]
    fn preset_populates_all_fields_from_registration() {
        // Verify step 1 fills kind, api_base, AND token_env from preset
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        // All three fields come from the single preset
        assert_eq!(result.kind, HostKind::Github);
        assert_eq!(result.api_base, "https://api.github.com");
        assert_eq!(result.token_env, "GITHUB_TOKEN");
        assert_eq!(result.host, "github.com");
    }

    #[test]
    fn non_preset_host_starts_empty() {
        // Step 1: non-preset host starts with (None, None, None) — needs all from user
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "custom.host.io".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: Some("https://custom.host.io/api/v3".into()),
                token_env: Some("CUSTOM_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("custom.host.io", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.host, "custom.host.io");
        assert_eq!(result.api_base, "https://custom.host.io/api/v3");
        assert_eq!(result.token_env, "CUSTOM_TOKEN");
    }

    // === Step 2: merge user overrides (additional coverage) ===

    #[test]
    fn user_overrides_all_preset_fields_simultaneously() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None, // keep preset kind
                api_base: Some("https://proxy.corp.com/gh".into()),
                token_env: Some("CORP_GH_TOKEN".into()),
                api_mode: Some(ApiMode::Rest),
                max_tags_per_repo: NonZeroU8::new(99),
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.kind, HostKind::Github);
        assert_eq!(result.api_base, "https://proxy.corp.com/gh");
        assert_eq!(result.token_env, "CORP_GH_TOKEN");
        assert_eq!(result.api_mode, ApiMode::Rest);
        assert_eq!(result.max_tags_per_repo.get(), 99);
    }

    #[test]
    fn user_none_fields_preserve_preset_values() {
        // Step 2: all-None HostConfig should change nothing from preset
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.kind, HostKind::Github);
        assert_eq!(result.api_base, "https://api.github.com");
        assert_eq!(result.token_env, "GITHUB_TOKEN");
    }

    // === Step 3: kind required (additional coverage) ===

    #[test]
    fn error_kind_required_message_includes_hostname() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "my.special.host".into(),
            HostConfig {
                kind: None,
                api_base: Some("https://my.special.host/api".into()),
                token_env: Some("TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let err = effective_host("my.special.host", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("my.special.host"), "{msg}");
        assert!(msg.contains("kind required"), "{msg}");
        assert!(msg.contains("not a preset host"), "{msg}");
    }

    // === Step 4: registration lookup by kind (additional coverage) ===

    #[test]
    fn error_no_registrations_at_all() {
        let regs: Vec<&Registration> = vec![];
        let mut config = Config::default();
        config.hosts.insert(
            "some.host".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: Some("https://some.host/api/v3".into()),
                token_env: Some("TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let err = effective_host("some.host", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not compiled in"), "{msg}");
        assert!(msg.contains("available: none"), "{msg}");
    }

    #[test]
    fn error_kind_not_compiled_lists_all_available() {
        // Multiple registrations available but not the requested one
        let gh = github_registration();
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gh, &gitea];
        let mut config = Config::default();
        config.hosts.insert(
            "gl.corp.com".into(),
            HostConfig {
                kind: Some(HostKind::Gitlab),
                api_base: Some("https://gl.corp.com/api/v4".into()),
                token_env: Some("GL_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };
        let err = effective_host("gl.corp.com", &config, &regs, &fetch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("github"), "{msg}");
        assert!(msg.contains("gitea"), "{msg}");
    }

    // === Step 5: defaults/inference (explicit coverage) ===

    #[test]
    fn default_token_env_used_when_not_in_preset_or_user() {
        // Non-preset GHE host: no user token_env, no preset token_env —
        // but registration.default_token_env fills it
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.internal".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: None,  // inferred: https://ghe.internal/api/v3
                token_env: None, // should use default_token_env: "GITHUB_TOKEN"
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("ghe.internal", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn user_token_env_overrides_default_token_env() {
        // User specifies token_env → default_token_env NOT used
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.internal".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: None,
                token_env: Some("MY_GHE_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("ghe.internal", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.token_env, "MY_GHE_TOKEN");
    }

    #[test]
    fn infer_api_base_called_when_not_preset_and_not_user() {
        // GHE host with no preset and no user api_base → infer_api_base called
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.example.org".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: None,
                token_env: Some("TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("ghe.example.org", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_base, "https://ghe.example.org/api/v3");
    }

    #[test]
    fn user_api_base_overrides_inference() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.example.org".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: Some("https://proxy.corp.com/ghe".into()),
                token_env: Some("TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("ghe.example.org", &config, &regs, &default_fetch()).unwrap();
        // User api_base wins over inference
        assert_eq!(result.api_base, "https://proxy.corp.com/ghe");
    }

    #[test]
    fn inference_returns_none_and_no_user_api_base_errors() {
        // GitLab infer_api_base returns None for unknown hosts
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gl];
        let mut config = Config::default();
        config.hosts.insert(
            "gl.internal.io".into(),
            HostConfig {
                kind: Some(HostKind::Gitlab),
                api_base: None,
                token_env: Some("GL_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };
        let err = effective_host("gl.internal.io", &config, &regs, &fetch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_base required"), "{msg}");
        assert!(msg.contains("gl.internal.io"), "{msg}");
    }

    // === Step 6: required field validation (additional coverage) ===

    #[test]
    fn error_invalid_api_base_shape_via_user_override() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: Some("ftp://api.github.com".into()), // invalid scheme
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let err = effective_host("github.com", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("http://") || msg.contains("https://"), "{msg}");
    }

    #[test]
    fn error_api_base_with_query_string() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: Some("https://api.github.com?key=val".into()),
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let err = effective_host("github.com", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("query") || msg.contains('?'), "{msg}");
    }

    // === Step 7: api_mode capability validation (additional coverage) ===

    #[test]
    fn graphql_only_provider_rejects_rest() {
        // Hypothetical graphql-only provider
        let graphql_only = Registration {
            kind: HostKind::Github,
            presets: &[Preset {
                host: "graphql-only.example.com",
                api_base: "https://graphql-only.example.com/graphql",
                token_env: "TOKEN",
            }],
            default_token_env: Some("TOKEN"),
            infer_api_base: |_| None,
            capabilities: Capabilities {
                supports_graphql: true,
                supports_rest: false,
            },
            create: mock_create,
        };
        let regs: Vec<&Registration> = vec![&graphql_only];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let err = effective_host("graphql-only.example.com", &config, &regs, &fetch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rest"), "{msg}");
        assert!(msg.contains("not supported"), "{msg}");
        assert!(msg.contains("graphql"), "{msg}");
    }

    #[test]
    fn rest_only_provider_succeeds_with_rest_mode() {
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gl];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let result = effective_host("gitlab.com", &config, &regs, &fetch).unwrap();
        assert_eq!(result.api_mode, ApiMode::Rest);
    }

    #[test]
    fn both_modes_supported_allows_graphql() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_mode, ApiMode::Graphql);
    }

    #[test]
    fn both_modes_supported_allows_rest() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let result = effective_host("github.com", &config, &regs, &fetch).unwrap();
        assert_eq!(result.api_mode, ApiMode::Rest);
    }

    #[test]
    fn host_api_mode_override_checked_against_capability() {
        // Host overrides api_mode to graphql on REST-only provider → error
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gl];
        let mut config = Config::default();
        config.hosts.insert(
            "gitlab.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: Some(ApiMode::Graphql),
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };
        let err = effective_host("gitlab.com", &config, &regs, &fetch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("graphql"), "{msg}");
        assert!(msg.contains("not supported"), "{msg}");
    }

    // === Precedence: per-host > fetch > default ===

    #[test]
    fn host_max_tags_overrides_fetch() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: NonZeroU8::new(77),
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(42).unwrap(),
        };

        let result = effective_host("github.com", &config, &regs, &fetch).unwrap();
        // Host override (77) beats fetch (42)
        assert_eq!(result.max_tags_per_repo.get(), 77);
    }

    #[test]
    fn max_tags_boundary_255() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: NonZeroU8::new(255),
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.max_tags_per_repo.get(), 255);
    }

    #[test]
    fn max_tags_boundary_1() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: NonZeroU8::new(1),
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.max_tags_per_repo.get(), 1);
    }

    // === Hostname validation integration ===

    #[test]
    fn error_empty_hostname() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err = effective_host("", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty"), "{msg}");
    }

    #[test]
    fn error_hostname_with_whitespace() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err = effective_host("github .com", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("whitespace"), "{msg}");
    }

    #[test]
    fn error_hostname_with_path() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err =
            effective_host("github.com/api/v3", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("path") || msg.contains('/'), "{msg}");
    }

    #[test]
    fn error_hostname_with_port() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err = effective_host("github.com:8443", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("port") || msg.contains(':'), "{msg}");
    }

    // === Multiple registrations (additional coverage) ===

    #[test]
    fn three_registrations_all_resolve_own_presets() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl, &gitea];
        let config = Config::default();
        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let gh_result = effective_host("github.com", &config, &regs, &fetch).unwrap();
        assert_eq!(gh_result.kind, HostKind::Github);

        let gitlab_result = effective_host("gitlab.com", &config, &regs, &fetch).unwrap();
        assert_eq!(gitlab_result.kind, HostKind::Gitlab);

        // Gitea has no presets — can't resolve without hosts entry
        let err = effective_host("gitea.example.com", &config, &regs, &fetch).unwrap_err();
        assert!(err.to_string().contains("kind required"));
    }

    #[test]
    fn non_preset_gitea_with_all_three_registrations() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl, &gitea];
        let mut config = Config::default();
        config.hosts.insert(
            "code.corp.io".into(),
            HostConfig {
                kind: Some(HostKind::Gitea),
                api_base: None,
                token_env: Some("GITEA_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let fetch = FetchSettings {
            api_mode: ApiMode::Rest,
            ..Default::default()
        };

        let result = effective_host("code.corp.io", &config, &regs, &fetch).unwrap();
        assert_eq!(result.kind, HostKind::Gitea);
        assert_eq!(result.api_base, "https://code.corp.io/api/v1");
        assert_eq!(result.token_env, "GITEA_TOKEN");
    }

    // === resolve_default_host (additional coverage) ===

    #[test]
    fn resolve_default_host_with_multiple_registrations() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl];
        let config = Config::default();

        let result = resolve_default_host(&config, &regs).unwrap();
        assert_eq!(result, "github.com");
    }

    #[test]
    fn resolve_default_host_explicit_gitlab() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl];
        let config = Config {
            default_host: Some("gitlab.com".into()),
            ..Default::default()
        };

        let result = resolve_default_host(&config, &regs).unwrap();
        assert_eq!(result, "gitlab.com");
    }

    #[test]
    fn resolve_default_host_empty_registrations_no_default() {
        let regs: Vec<&Registration> = vec![];
        let config = Config::default();

        let err = resolve_default_host(&config, &regs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no default_host set"), "{msg}");
    }

    #[test]
    fn resolve_default_host_explicit_but_invalid_hostname() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config {
            default_host: Some("https://github.com".into()),
            ..Default::default()
        };

        let err = resolve_default_host(&config, &regs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("scheme"), "{msg}");
    }

    #[test]
    fn resolve_default_host_hosts_entry_without_preset() {
        // default_host points to a [hosts] entry that has no preset
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config {
            default_host: Some("ghe.internal".into()),
            ..Default::default()
        };
        config.hosts.insert(
            "ghe.internal".into(),
            HostConfig {
                kind: Some(HostKind::Github),
                api_base: Some("https://ghe.internal/api/v3".into()),
                token_env: Some("GHE_TOKEN".into()),
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = resolve_default_host(&config, &regs).unwrap();
        assert_eq!(result, "ghe.internal");
    }

    // === Idempotency ===

    #[test]
    fn effective_host_idempotent() {
        // Calling twice with same inputs yields same result
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let r1 = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        let r2 = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(r1.host, r2.host);
        assert_eq!(r1.kind, r2.kind);
        assert_eq!(r1.api_base, r2.api_base);
        assert_eq!(r1.token_env, r2.token_env);
        assert_eq!(r1.api_mode, r2.api_mode);
        assert_eq!(r1.max_tags_per_repo, r2.max_tags_per_repo);
    }

    // === capability_list (additional coverage) ===

    #[test]
    fn capability_list_graphql_only() {
        let caps = Capabilities {
            supports_graphql: true,
            supports_rest: false,
        };
        assert_eq!(capability_list(caps), "graphql");
    }

    // === Edge: preset token_env vs user token_env on preset host ===

    #[test]
    fn preset_token_env_preserved_when_user_sets_other_fields() {
        // User overrides api_base on preset host but NOT token_env
        // → token_env should still come from preset
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: Some("https://proxy.github.com".into()),
                token_env: None, // not overridden
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.token_env, "GITHUB_TOKEN"); // from preset
        assert_eq!(result.api_base, "https://proxy.github.com"); // from user
    }

    // === Edge: no hosts entry and not a preset ===

    #[test]
    fn non_preset_no_hosts_entry_errors_kind_required() {
        // Host not in presets AND not in config.hosts → step 3 errors
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let config = Config::default();

        let err =
            effective_host("totally.unknown.host", &config, &regs, &default_fetch()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("kind required"), "{msg}");
        assert!(msg.contains("totally.unknown.host"), "{msg}");
    }

    // === api_base normalization via resolution pipeline ===

    #[test]
    fn multiple_trailing_slashes_stripped() {
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gh];
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".into(),
            HostConfig {
                kind: None,
                api_base: Some("https://api.github.com///".into()),
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let result = effective_host("github.com", &config, &regs, &default_fetch()).unwrap();
        assert_eq!(result.api_base, "https://api.github.com");
    }

    // === build_preset_index: registration with no presets ===

    #[test]
    fn preset_index_skips_registration_without_presets() {
        let gitea = gitea_registration(); // no presets
        let gh = github_registration();
        let regs: Vec<&Registration> = vec![&gitea, &gh];
        let index = build_preset_index(&regs);
        assert_eq!(index.len(), 1);
        assert!(index.contains_key("github.com"));
    }

    // === find_registration_by_kind: multiple kinds ===

    #[test]
    fn find_registration_returns_first_match() {
        let gh = github_registration();
        let gl = gitlab_rest_only_registration();
        let gitea = gitea_registration();
        let regs: Vec<&Registration> = vec![&gh, &gl, &gitea];

        assert_eq!(
            find_registration_by_kind(&regs, HostKind::Gitlab)
                .unwrap()
                .kind,
            HostKind::Gitlab
        );
        assert_eq!(
            find_registration_by_kind(&regs, HostKind::Gitea)
                .unwrap()
                .kind,
            HostKind::Gitea
        );
    }

    #[test]
    fn find_registration_empty_list() {
        let regs: Vec<&Registration> = vec![];
        assert!(find_registration_by_kind(&regs, HostKind::Github).is_none());
    }
}
