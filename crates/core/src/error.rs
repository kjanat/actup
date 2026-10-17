//! Structured error types for actup-core
//!
//! Errors are categorized as:
//! - **User errors**: Invalid input, typos, misconfiguration (user can fix)
//! - **Environment errors**: Network, rate limits, auth (external factors)

use std::sync::Arc;

/// Result type alias using [`Error`]
pub type Result<T> = std::result::Result<T, Error>;

/// The foreign error behind an [`Error`].
pub type Source = Arc<dyn std::error::Error + Send + Sync>;

/// Primary error type for actup-core operations
///
/// # Provider Implementation Guide
///
/// When implementing a provider, use these error variants:
///
/// - [`NotFound`]: HTTP 404 on repo lookup (not ref-specific)
/// - [`RefNotFound`]: Specific ref/tag/branch doesn't exist
/// - [`RateLimited`]: HTTP 429 or rate limit headers
/// - [`Auth`]: HTTP 401/403 (auth required or insufficient permissions)
/// - [`Network`]: Connection failures, timeouts, or other HTTP errors
/// - [`Internal`]: Unexpected data format (bad SHA, encoding issues)
///
/// For JSON/YAML parsing of responses, use [`Network`] with operation "parse response".
/// For YAML parsing of action files, use [`YamlParse`].
///
/// [`NotFound`]: Error::NotFound
/// [`RefNotFound`]: Error::RefNotFound
/// [`RateLimited`]: Error::RateLimited
/// [`Auth`]: Error::Auth
/// [`Network`]: Error::Network
/// [`Internal`]: Error::Internal
/// [`YamlParse`]: Error::YamlParse
///
/// `Display` leaves out the text of `source`, and [`chain`] renders every cause.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// Invalid `uses:` reference syntax
    #[error("invalid action reference '{input}': {reason}")]
    InvalidRef {
        /// The malformed reference string
        input: String,
        /// Human-readable explanation
        reason: String,
    },

    /// Action repository not found (404)
    ///
    /// # Provider Implementation Example
    ///
    /// ```rust,ignore
    /// // When HTTP 404 on repo lookup:
    /// Err(Error::NotFound {
    ///     owner: owner.to_string(),
    ///     repo: repo.to_string(),
    ///     host: Some(self.host.clone()),
    /// })
    /// ```
    #[error("action not found: {}", qualified(.host.as_deref(), .owner, .repo))]
    NotFound {
        /// The owner/org that wasn't found
        owner: String,
        /// The repository that wasn't found
        repo: String,
        /// Optional host if not default
        host: Option<String>,
    },

    /// Version/tag/branch doesn't exist in the repository
    ///
    /// # Provider Implementation Example
    ///
    /// ```rust,ignore
    /// // When a specific ref (tag/branch) is not found:
    /// Err(Error::RefNotFound {
    ///     owner: owner.to_string(),
    ///     repo: repo.to_string(),
    ///     ref_name: ref_name.to_string(),
    ///     host: Some(self.host.clone()),
    /// })
    /// ```
    #[error("ref '{ref_name}' not found in {}", qualified(.host.as_deref(), .owner, .repo))]
    RefNotFound {
        /// The owner/org
        owner: String,
        /// The repository
        repo: String,
        /// The ref that wasn't found
        ref_name: String,
        /// Optional host if not default
        host: Option<String>,
    },

    /// Configuration file is invalid
    #[error("invalid config at '{path}'{}", detail(.reason))]
    InvalidConfig {
        /// Path to the config file
        path: String,
        /// What's wrong, without the text of `source`
        reason: String,
        /// The read or parse error
        source: Option<Source>,
        /// A suggested fix, like ``did you mean `policy.bump`?``, that [`chain`] prints last
        hint: Option<String>,
    },

    /// YAML parsing failed
    #[error("failed to parse YAML '{path}'{}", detail(.reason))]
    YamlParse {
        /// File path
        path: String,
        /// Parse error details, without the text of `source`
        reason: String,
        /// The parser error
        source: Option<Source>,
    },

    /// Network request failed
    #[error("network error during {operation}{}", detail(.message))]
    Network {
        /// What we were trying to do
        operation: String,
        /// What went wrong, without the text of `source`
        message: String,
        /// The transport or decoding error
        source: Option<Source>,
    },

    /// API rate limit exceeded
    ///
    /// # Provider Implementation Example
    ///
    /// ```rust,ignore
    /// // When HTTP 429 (rate limited):
    /// Err(Error::RateLimited {
    ///     provider: "github".to_string(),
    ///     retry_after: response
    ///         .headers()
    ///         .get("retry-after")
    ///         .and_then(|v| v.to_str().ok())
    ///         .and_then(|s| s.parse::<u64>().ok())
    ///         .map(std::time::Duration::from_secs),
    /// })
    /// ```
    #[error("rate limited by {provider}{}", retry_hint(.retry_after.as_ref()))]
    RateLimited {
        /// Provider that rate limited us
        provider: String,
        /// When we can retry (if known)
        retry_after: Option<std::time::Duration>,
    },

    /// Permission denied (403 without rate limit indicators)
    ///
    /// Distinct from `RateLimited` - this indicates insufficient scopes/access
    /// rather than temporary throttling.
    #[error("permission denied by {provider}: {reason}")]
    Forbidden {
        /// Provider that denied access
        provider: String,
        /// What went wrong
        reason: String,
    },

    /// Authentication failed or missing
    ///
    /// # Provider Implementation Examples
    ///
    /// ```rust,ignore
    /// // Invalid token format:
    /// Err(Error::Auth {
    ///     provider: "github".to_string(),
    ///     reason: "invalid token format".to_string(),
    ///     source: Some(Arc::new(e)),
    /// })
    ///
    /// // HTTP 401 (unauthorized):
    /// Err(Error::Auth {
    ///     provider: "gitlab".to_string(),
    ///     reason: "unauthorized - check your token".to_string(),
    ///     source: None,
    /// })
    /// ```
    #[error("authentication failed for {provider}{}", detail(.reason))]
    Auth {
        /// Provider requiring auth
        provider: String,
        /// What went wrong, without the text of `source`
        reason: String,
        /// The transport or header error
        source: Option<Source>,
    },

    /// Cache operation failed
    #[error("cache {operation} failed: {reason}")]
    Cache {
        /// What operation failed
        operation: String,
        /// Error details
        reason: String,
    },

    /// Provider not found for host
    #[error("no provider configured for host: {host}")]
    ProviderNotFound {
        /// The host we couldn't find a provider for
        host: String,
    },

    /// Generic internal error (should be rare)
    #[error("internal error in {context}{}", detail(.message))]
    Internal {
        /// Error context
        context: String,
        /// Error details, without the text of `source`
        message: String,
        /// The failed task or decoding error
        source: Option<Source>,
    },
}

impl Error {
    /// Returns true if this is a user error (something the user can fix)
    #[must_use]
    pub const fn is_user_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidRef { .. }
                | Self::NotFound { .. }
                | Self::RefNotFound { .. }
                | Self::InvalidConfig { .. }
                | Self::YamlParse { .. }
        )
    }

    /// Returns true if this is an environment error (external factors)
    #[must_use]
    pub const fn is_environment_error(&self) -> bool {
        matches!(
            self,
            Self::Network { .. }
                | Self::RateLimited { .. }
                | Self::Forbidden { .. }
                | Self::Auth { .. }
                | Self::Cache { .. }
                | Self::ProviderNotFound { .. }
        )
    }

    /// Returns true if this error means we couldn't verify the status
    /// (result is unknown/stale rather than definitively outdated or error)
    #[must_use]
    pub const fn is_unknown_status(&self) -> bool {
        matches!(
            self,
            Self::Network { .. } | Self::RateLimited { .. } | Self::Cache { .. }
        )
    }
}

/// Render `err` followed by each error in its `source()` chain, joined by `": "`.
///
/// The hint of an [`Error::InvalidConfig`] follows the chain after `"; "`.
#[must_use]
pub fn chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut current = err.source();
    while let Some(cause) = current {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        current = cause.source();
    }
    if let Some(Error::InvalidConfig {
        hint: Some(hint), ..
    }) = err.downcast_ref::<Error>()
    {
        text.push_str("; ");
        text.push_str(hint);
    }
    text
}

fn detail(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

fn qualified(host: Option<&str>, owner: &str, repo: &str) -> String {
    host.map_or_else(
        || format!("{owner}/{repo}"),
        |h| format!("{h}/{owner}/{repo}"),
    )
}

fn retry_hint(retry_after: Option<&std::time::Duration>) -> String {
    retry_after.map_or_else(String::new, |d| format!(", retry after {d:?}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_is_user_error() {
        let user_errors = [
            Error::InvalidRef {
                input: "bad".into(),
                reason: "malformed".into(),
            },
            Error::NotFound {
                owner: "foo".into(),
                repo: "bar".into(),
                host: None,
            },
            Error::RefNotFound {
                owner: "foo".into(),
                repo: "bar".into(),
                ref_name: "v99".into(),
                host: None,
            },
            Error::InvalidConfig {
                path: "config.toml".into(),
                reason: "syntax error".into(),
                hint: None,
                source: None,
            },
            Error::YamlParse {
                path: "test.yml".into(),
                reason: "invalid".into(),
                source: None,
            },
        ];

        for err in &user_errors {
            assert!(err.is_user_error(), "{err:?} should be user error");
            assert!(
                !err.is_environment_error(),
                "{err:?} should not be env error"
            );
        }
    }

    #[test]
    fn test_is_environment_error() {
        let env_errors = [
            Error::Network {
                operation: "fetch".into(),
                message: "timeout".into(),
                source: None,
            },
            Error::RateLimited {
                provider: "github".into(),
                retry_after: None,
            },
            Error::Forbidden {
                provider: "github".into(),
                reason: "insufficient scope".into(),
            },
            Error::Auth {
                provider: "github".into(),
                reason: "invalid token".into(),
                source: None,
            },
            Error::Cache {
                operation: "read".into(),
                reason: "corrupted".into(),
            },
            Error::ProviderNotFound {
                host: "unknown.com".into(),
            },
        ];

        for err in &env_errors {
            assert!(err.is_environment_error(), "{err:?} should be env error");
            assert!(!err.is_user_error(), "{err:?} should not be user error");
        }
    }

    #[test]
    fn test_is_unknown_status() {
        // These errors mean we couldn't verify - status is unknown
        assert!(
            Error::Network {
                operation: "fetch".into(),
                message: "timeout".into(),
                source: None,
            }
            .is_unknown_status()
        );

        assert!(
            Error::RateLimited {
                provider: "github".into(),
                retry_after: None,
            }
            .is_unknown_status()
        );

        assert!(
            Error::Cache {
                operation: "read".into(),
                reason: "corrupted".into(),
            }
            .is_unknown_status()
        );

        // Auth failure is environment but not unknown - we know it failed
        assert!(
            !Error::Auth {
                provider: "github".into(),
                reason: "bad token".into(),
                source: None,
            }
            .is_unknown_status()
        );

        // User errors are not unknown status
        assert!(
            !Error::NotFound {
                owner: "foo".into(),
                repo: "bar".into(),
                host: None,
            }
            .is_unknown_status()
        );
    }

    #[test]
    fn test_display() {
        let err = Error::InvalidRef {
            input: "bad@ref".into(),
            reason: "missing owner".into(),
        };
        assert_eq!(
            err.to_string(),
            "invalid action reference 'bad@ref': missing owner"
        );

        let err = Error::NotFound {
            owner: "foo".into(),
            repo: "bar".into(),
            host: None,
        };
        assert_eq!(err.to_string(), "action not found: foo/bar");

        let err = Error::NotFound {
            owner: "foo".into(),
            repo: "bar".into(),
            host: Some("ghe.corp.com".into()),
        };
        assert_eq!(err.to_string(), "action not found: ghe.corp.com/foo/bar");

        let err = Error::RateLimited {
            provider: "github".into(),
            retry_after: Some(std::time::Duration::from_mins(1)),
        };
        assert!(err.to_string().contains("rate limited by github"));
        assert!(err.to_string().contains("60"));

        let missing_ref = |host: Option<&str>| Error::RefNotFound {
            owner: "foo".into(),
            repo: "bar".into(),
            ref_name: "v9".into(),
            host: host.map(Into::into),
        };
        assert_eq!(
            missing_ref(None).to_string(),
            "ref 'v9' not found in foo/bar"
        );
        assert_eq!(
            missing_ref(Some("ghe.corp.com")).to_string(),
            "ref 'v9' not found in ghe.corp.com/foo/bar"
        );

        let err = Error::RateLimited {
            provider: "gitea".into(),
            retry_after: None,
        };
        assert_eq!(err.to_string(), "rate limited by gitea");

        let err = Error::ProviderNotFound {
            host: "unknown.com".into(),
        };
        assert_eq!(
            err.to_string(),
            "no provider configured for host: unknown.com"
        );
    }

    #[test]
    fn test_internal_error() {
        let err = Error::Internal {
            context: "version resolution".into(),
            message: "unexpected state".into(),
            source: None,
        };

        // Internal is neither user nor environment (edge case)
        assert!(!err.is_user_error());
        assert!(!err.is_environment_error());
        assert!(!err.is_unknown_status());

        assert_eq!(
            err.to_string(),
            "internal error in version resolution: unexpected state"
        );
    }

    #[derive(Debug, thiserror::Error)]
    #[error("connect failed")]
    struct Connect(#[source] std::io::Error);

    fn refused() -> Source {
        Arc::new(Connect(std::io::Error::other("connection refused")))
    }

    #[test]
    fn display_leaves_out_the_source_and_chain_renders_it() {
        let network = Error::Network {
            operation: "GET /repos/o/r/tags".into(),
            message: String::new(),
            source: Some(refused()),
        };
        assert_eq!(
            network.to_string(),
            "network error during GET /repos/o/r/tags"
        );
        assert_eq!(
            std::error::Error::source(&network).map(ToString::to_string),
            Some("connect failed".to_string())
        );
        assert_eq!(
            chain(&network),
            "network error during GET /repos/o/r/tags: connect failed: connection refused"
        );

        let auth = Error::Auth {
            provider: "github".into(),
            reason: "invalid token format".into(),
            source: Some(refused()),
        };
        let copy = auth.clone();
        assert_eq!(
            auth.to_string(),
            "authentication failed for github: invalid token format"
        );
        assert_eq!(
            chain(&copy),
            "authentication failed for github: invalid token format: connect failed: connection refused"
        );

        let plain = Error::Network {
            operation: "fetch".into(),
            message: "timeout".into(),
            source: None,
        };
        assert!(std::error::Error::source(&plain).is_none());
        assert_eq!(chain(&plain), "network error during fetch: timeout");
    }

    #[test]
    fn chain_prints_the_config_hint_last() {
        let err = Error::InvalidConfig {
            path: "actup.toml".into(),
            reason: String::new(),
            hint: Some("did you mean `policy.bump`?".into()),
            source: Some(refused()),
        };
        assert_eq!(err.to_string(), "invalid config at 'actup.toml'");
        assert_eq!(
            chain(&err),
            "invalid config at 'actup.toml': connect failed: connection refused; did you mean `policy.bump`?"
        );
    }
}
