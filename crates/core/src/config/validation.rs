//! Config validation rules.
//!
//! Three categories of validation:
//!
//! 1. **Hostname** (`hosts` keys, `default_host`): lowercase, no scheme/path/port/whitespace
//! 2. **`api_base` URL**: scheme required, path allowed, no query/fragment, trailing `/` stripped
//! 3. **Post-merge** (per effective host): `kind`, `api_base`, `token_env` all required

use crate::Error;
use crate::config::registration::ProviderConfig;

/// Validate a hostname string (used for `hosts` map keys and `default_host`).
///
/// Requirements from spec:
/// - Must be lowercase (error if `key != key.to_ascii_lowercase()`)
/// - No scheme (`https://`, `http://`)
/// - No path (`/api/v3`)
/// - No port (`:8443`) — use `api_base` for ports
/// - No whitespace
///
/// # Errors
///
/// Returns `InvalidConfig` with a descriptive reason on failure.
pub fn validate_hostname(hostname: &str, context: &str) -> Result<(), Error> {
    if hostname.is_empty() {
        return Err(invalid_config(context, "hostname must not be empty"));
    }

    if hostname != hostname.to_ascii_lowercase() {
        return Err(invalid_config(
            context,
            &format!(
                "hostname must be lowercase: \"{hostname}\" (use \"{}\")",
                hostname.to_ascii_lowercase()
            ),
        ));
    }

    if hostname.contains("://") {
        return Err(invalid_config(
            context,
            &format!(
                "hostname must not contain a scheme: \"{hostname}\" (remove the protocol prefix)"
            ),
        ));
    }

    if hostname.contains('/') {
        return Err(invalid_config(
            context,
            &format!("hostname must not contain a path: \"{hostname}\" (use api_base for paths)"),
        ));
    }

    if hostname.contains(':') {
        return Err(invalid_config(
            context,
            &format!("hostname must not contain a port: \"{hostname}\" (use api_base for ports)"),
        ));
    }

    if hostname.chars().any(char::is_whitespace) {
        return Err(invalid_config(
            context,
            &format!("hostname must not contain whitespace: \"{hostname}\""),
        ));
    }

    Ok(())
}

/// Validate and normalize an `api_base` URL.
///
/// Requirements from spec:
/// - Scheme required: `http://` or `https://`
/// - Path allowed (needed for `/api/v3`, `/api/v4`, `/api/v1`)
/// - Query (`?`) and fragment (`#`) forbidden
/// - Trailing `/` stripped
///
/// Returns the normalized URL (trailing slash removed) on success.
///
/// # Errors
///
/// Returns `InvalidConfig` with a descriptive reason on failure.
pub fn validate_api_base(api_base: &str, context: &str) -> Result<String, Error> {
    if api_base.is_empty() {
        return Err(invalid_config(context, "api_base must not be empty"));
    }

    if !api_base.starts_with("http://") && !api_base.starts_with("https://") {
        return Err(invalid_config(
            context,
            &format!("api_base must include a scheme (http:// or https://): \"{api_base}\""),
        ));
    }

    if api_base.contains('?') {
        return Err(invalid_config(
            context,
            &format!("api_base must not contain a query string: \"{api_base}\""),
        ));
    }

    if api_base.contains('#') {
        return Err(invalid_config(
            context,
            &format!("api_base must not contain a fragment: \"{api_base}\""),
        ));
    }

    // Strip trailing slash
    Ok(api_base.trim_end_matches('/').to_string())
}

/// Validate that a post-merge [`ProviderConfig`] has all required fields.
///
/// After merging preset defaults, user overrides, and provider inference,
/// `kind`, `api_base`, and `token_env` must all be present and valid.
///
/// This function validates:
/// - Non-empty `api_base` and `token_env`
/// - `api_base` passes URL validation
///
/// # Errors
///
/// Returns `InvalidConfig` if any required field is missing or invalid.
pub fn validate_provider_config(config: &ProviderConfig) -> Result<(), Error> {
    let ctx = format!("host \"{}\"", config.host);

    if config.api_base.is_empty() {
        return Err(invalid_config(
            &ctx,
            &format!("api_base required for {}", config.host),
        ));
    }

    // Validate api_base URL shape
    validate_api_base(&config.api_base, &ctx)?;

    if config.token_env.is_empty() {
        return Err(invalid_config(
            &ctx,
            &format!("token_env required for {}", config.host),
        ));
    }

    Ok(())
}

/// Helper to construct `Error::InvalidConfig` for validation context.
fn invalid_config(context: &str, reason: &str) -> Error {
    Error::InvalidConfig {
        path: context.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::{ApiMode, HostKind};
    use std::num::NonZeroU8;

    // === Hostname validation ===

    #[test]
    fn hostname_valid_simple() {
        assert!(validate_hostname("github.com", "hosts").is_ok());
    }

    #[test]
    fn hostname_valid_subdomain() {
        assert!(validate_hostname("ghe.corp.com", "hosts").is_ok());
    }

    #[test]
    fn hostname_valid_short() {
        assert!(validate_hostname("localhost", "hosts").is_ok());
    }

    #[test]
    fn hostname_valid_with_hyphens() {
        assert!(validate_hostname("my-ghe.corp-internal.com", "hosts").is_ok());
    }

    #[test]
    fn hostname_rejects_empty() {
        let err = validate_hostname("", "hosts").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn hostname_rejects_uppercase() {
        let err = validate_hostname("GitHub.com", "hosts").unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{err}");
        assert!(err.to_string().contains("github.com"), "{err}");
    }

    #[test]
    fn hostname_rejects_all_caps() {
        let err = validate_hostname("GITHUB.COM", "hosts").unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{err}");
    }

    #[test]
    fn hostname_rejects_https_scheme() {
        let err = validate_hostname("https://github.com", "hosts").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn hostname_rejects_http_scheme() {
        let err = validate_hostname("http://github.com", "hosts").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn hostname_rejects_path() {
        let err = validate_hostname("github.com/api/v3", "hosts").unwrap_err();
        assert!(err.to_string().contains("path"), "{err}");
    }

    #[test]
    fn hostname_rejects_port() {
        let err = validate_hostname("ghe.corp.com:8443", "hosts").unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    #[test]
    fn hostname_rejects_leading_whitespace() {
        let err = validate_hostname(" github.com", "hosts").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn hostname_rejects_trailing_whitespace() {
        let err = validate_hostname("github.com ", "hosts").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn hostname_rejects_internal_whitespace() {
        let err = validate_hostname("git hub.com", "hosts").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn hostname_rejects_tab() {
        let err = validate_hostname("github\t.com", "hosts").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn hostname_context_in_error() {
        let err = validate_hostname("Bad.Host", "default_host").unwrap_err();
        assert!(err.to_string().contains("default_host"), "{err}");
    }

    // === api_base validation ===

    #[test]
    fn api_base_valid_https() {
        let result = validate_api_base("https://api.github.com", "test").unwrap();
        assert_eq!(result, "https://api.github.com");
    }

    #[test]
    fn api_base_valid_http() {
        let result = validate_api_base("http://ghe.local/api/v3", "test").unwrap();
        assert_eq!(result, "http://ghe.local/api/v3");
    }

    #[test]
    fn api_base_valid_with_path() {
        let result = validate_api_base("https://ghe.corp.com/api/v3", "test").unwrap();
        assert_eq!(result, "https://ghe.corp.com/api/v3");
    }

    #[test]
    fn api_base_strips_trailing_slash() {
        let result = validate_api_base("https://api.github.com/", "test").unwrap();
        assert_eq!(result, "https://api.github.com");
    }

    #[test]
    fn api_base_strips_multiple_trailing_slashes() {
        let result = validate_api_base("https://api.github.com///", "test").unwrap();
        assert_eq!(result, "https://api.github.com");
    }

    #[test]
    fn api_base_rejects_empty() {
        let err = validate_api_base("", "test").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn api_base_rejects_no_scheme() {
        let err = validate_api_base("api.github.com", "test").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn api_base_rejects_ftp_scheme() {
        let err = validate_api_base("ftp://api.github.com", "test").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn api_base_rejects_query_string() {
        let err = validate_api_base("https://api.github.com?token=abc", "test").unwrap_err();
        assert!(err.to_string().contains("query"), "{err}");
    }

    #[test]
    fn api_base_rejects_fragment() {
        let err = validate_api_base("https://api.github.com#section", "test").unwrap_err();
        assert!(err.to_string().contains("fragment"), "{err}");
    }

    #[test]
    fn api_base_context_in_error() {
        let err = validate_api_base("bad-url", "host \"ghe.corp.com\"").unwrap_err();
        assert!(err.to_string().contains("ghe.corp.com"), "{err}");
    }

    // === Post-merge (ProviderConfig) validation ===

    fn valid_provider_config() -> ProviderConfig {
        ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::Github,
            api_base: "https://api.github.com".into(),
            token_env: "GITHUB_TOKEN".into(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        }
    }

    #[test]
    fn provider_config_valid() {
        assert!(validate_provider_config(&valid_provider_config()).is_ok());
    }

    #[test]
    fn provider_config_valid_ghe() {
        let config = ProviderConfig {
            host: "ghe.corp.com".into(),
            kind: HostKind::Github,
            api_base: "https://ghe.corp.com/api/v3".into(),
            token_env: "GHE_TOKEN".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(50).unwrap(),
        };
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn provider_config_rejects_empty_api_base() {
        let mut config = valid_provider_config();
        config.api_base = String::new();
        let err = validate_provider_config(&config).unwrap_err();
        assert!(err.to_string().contains("api_base"), "{err}");
        assert!(err.to_string().contains("github.com"), "{err}");
    }

    #[test]
    fn provider_config_rejects_invalid_api_base() {
        let mut config = valid_provider_config();
        config.api_base = "no-scheme.example.com".into();
        let err = validate_provider_config(&config).unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn provider_config_rejects_empty_token_env() {
        let mut config = valid_provider_config();
        config.token_env = String::new();
        let err = validate_provider_config(&config).unwrap_err();
        assert!(err.to_string().contains("token_env"), "{err}");
        assert!(err.to_string().contains("github.com"), "{err}");
    }

    #[test]
    fn provider_config_error_mentions_host() {
        let mut config = valid_provider_config();
        config.host = "custom.host.com".into();
        config.token_env = String::new();
        let err = validate_provider_config(&config).unwrap_err();
        assert!(err.to_string().contains("custom.host.com"), "{err}");
    }

    #[test]
    fn provider_config_api_base_query_rejected() {
        let mut config = valid_provider_config();
        config.api_base = "https://api.github.com?foo=bar".into();
        let err = validate_provider_config(&config).unwrap_err();
        assert!(err.to_string().contains("query"), "{err}");
    }

    #[test]
    fn provider_config_api_base_fragment_rejected() {
        let mut config = valid_provider_config();
        config.api_base = "https://api.github.com#frag".into();
        let err = validate_provider_config(&config).unwrap_err();
        assert!(err.to_string().contains("fragment"), "{err}");
    }
}
