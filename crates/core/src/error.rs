//! Structured error types for actup-core
//!
//! Errors are categorized as:
//! - **User errors**: Invalid input, typos, misconfiguration (user can fix)
//! - **Environment errors**: Network, rate limits, auth (external factors)

use std::fmt;

/// Result type alias using [`Error`]
pub type Result<T> = std::result::Result<T, Error>;

/// Primary error type for actup-core operations
#[derive(Debug)]
pub enum Error {
    // === User Errors (fixable by user) ===
    /// Invalid `uses:` reference syntax
    InvalidRef {
        /// The malformed reference string
        input: String,
        /// Human-readable explanation
        reason: String,
    },

    /// Action repository not found (404)
    NotFound {
        /// The owner/org that wasn't found
        owner: String,
        /// The repository that wasn't found
        repo: String,
        /// Optional host if not default
        host: Option<String>,
    },

    /// Version/tag/branch doesn't exist in the repository
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
    InvalidConfig {
        /// Path to the config file
        path: String,
        /// What's wrong
        reason: String,
    },

    /// YAML parsing failed
    YamlParse {
        /// File path
        path: String,
        /// Parse error details
        reason: String,
    },

    // === Environment Errors (external factors) ===
    /// Network request failed
    Network {
        /// What we were trying to do
        operation: String,
        /// Underlying error message
        message: String,
    },

    /// API rate limit exceeded
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
    Forbidden {
        /// Provider that denied access
        provider: String,
        /// What went wrong
        reason: String,
    },

    /// Authentication failed or missing
    Auth {
        /// Provider requiring auth
        provider: String,
        /// What went wrong
        reason: String,
    },

    /// Cache operation failed
    Cache {
        /// What operation failed
        operation: String,
        /// Error details
        reason: String,
    },

    /// Provider not found for host
    ProviderNotFound {
        /// The host we couldn't find a provider for
        host: String,
    },

    /// Generic internal error (should be rare)
    Internal {
        /// Error context
        context: String,
        /// Error details
        message: String,
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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRef { input, reason } => {
                write!(f, "invalid action reference '{input}': {reason}")
            }
            Self::NotFound { owner, repo, host } => {
                if let Some(h) = host {
                    write!(f, "action not found: {h}/{owner}/{repo}")
                } else {
                    write!(f, "action not found: {owner}/{repo}")
                }
            }
            Self::RefNotFound {
                owner,
                repo,
                ref_name,
                host,
            } => {
                if let Some(h) = host {
                    write!(f, "ref '{ref_name}' not found in {h}/{owner}/{repo}")
                } else {
                    write!(f, "ref '{ref_name}' not found in {owner}/{repo}")
                }
            }
            Self::InvalidConfig { path, reason } => {
                write!(f, "invalid config at '{path}': {reason}")
            }
            Self::YamlParse { path, reason } => {
                write!(f, "failed to parse YAML '{path}': {reason}")
            }
            Self::Network { operation, message } => {
                write!(f, "network error during {operation}: {message}")
            }
            Self::RateLimited {
                provider,
                retry_after,
            } => {
                if let Some(dur) = retry_after {
                    write!(f, "rate limited by {provider}, retry after {dur:?}")
                } else {
                    write!(f, "rate limited by {provider}")
                }
            }
            Self::Forbidden { provider, reason } => {
                write!(f, "permission denied by {provider}: {reason}")
            }
            Self::Auth { provider, reason } => {
                write!(f, "authentication failed for {provider}: {reason}")
            }
            Self::Cache { operation, reason } => {
                write!(f, "cache {operation} failed: {reason}")
            }
            Self::ProviderNotFound { host } => {
                write!(f, "no provider configured for host: {host}")
            }
            Self::Internal { context, message } => {
                write!(f, "internal error in {context}: {message}")
            }
        }
    }
}

impl std::error::Error for Error {}

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
            },
            Error::YamlParse {
                path: "test.yml".into(),
                reason: "invalid".into(),
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
            retry_after: Some(std::time::Duration::from_secs(60)),
        };
        assert!(err.to_string().contains("rate limited by github"));
        assert!(err.to_string().contains("60"));

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
}
