//! Error user experience: notifications, deduplication, and aggregate logging.
//!
//! When diagnostic computation encounters environment errors (rate limits,
//! auth failures, network issues), this module decides what to show the user
//! via `window/showMessage` vs `window/logMessage`, and prevents the same
//! error from spamming across repeated diagnostic runs within a session.

use std::collections::HashSet;
use std::sync::Mutex;

use actup_core::Error as CoreError;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::MessageType;

/// Category of environment error for deduplication and notification routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// API rate limit exceeded (HTTP 429).
    RateLimit,
    /// Authentication failure or missing token (HTTP 401/403).
    Auth,
    /// Network connectivity issue (timeout, DNS, etc.).
    Network,
    /// Provider not found for a host.
    ProviderNotFound,
    /// Permission denied (distinct from auth — insufficient scopes).
    Forbidden,
}

impl ErrorCategory {
    /// Classify a [`CoreError`] into an error category, if it's an environment error.
    ///
    /// Returns `None` for user errors (which become diagnostics, not notifications).
    pub const fn from_core_error(err: &CoreError) -> Option<Self> {
        match err {
            CoreError::RateLimited { .. } => Some(Self::RateLimit),
            CoreError::Auth { .. } => Some(Self::Auth),
            CoreError::Network { .. } => Some(Self::Network),
            CoreError::ProviderNotFound { .. } => Some(Self::ProviderNotFound),
            CoreError::Forbidden { .. } => Some(Self::Forbidden),
            _ => None,
        }
    }
}

/// Tracks which error categories have already been shown to the user
/// during this LSP session, preventing duplicate notifications.
///
/// Thread-safe via `Mutex` since notifications happen from multiple
/// async diagnostic tasks. The set is small (≤5 variants) so lock
/// contention is negligible.
pub struct ErrorDeduplicator {
    shown: Mutex<HashSet<ErrorCategory>>,
}

impl ErrorDeduplicator {
    /// Create a new deduplicator with no shown errors.
    pub fn new() -> Self {
        Self {
            shown: Mutex::new(HashSet::new()),
        }
    }

    /// Returns `true` if this category has NOT been shown yet and marks it as shown.
    ///
    /// Returns `false` if already shown (caller should skip notification).
    pub fn should_notify(&self, category: ErrorCategory) -> bool {
        self.shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(category)
    }

    /// Reset a single category so it can be shown again.
    ///
    /// Useful if the user changes config (e.g. adds a token) and we want
    /// to re-notify if the error persists. Currently used in tests;
    /// `reset_all` is used by the config reload path.
    #[cfg(test)]
    pub fn reset(&self, category: ErrorCategory) {
        self.shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&category);
    }

    /// Reset all shown categories.
    pub fn reset_all(&self) {
        self.shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// Errors collected during a single diagnostic computation run.
///
/// Separates per-ref failures (shown as diagnostics) from aggregate
/// environment errors (shown as notifications).
#[derive(Debug, Default)]
pub struct DiagnosticErrors {
    /// Environment errors encountered, keyed by category.
    /// Only the first error per category is stored (dedup within a single run).
    errors: Vec<(ErrorCategory, String)>,
}

impl DiagnosticErrors {
    /// Record an error from version fetching.
    pub fn record(&mut self, err: &CoreError) {
        if let Some(category) = ErrorCategory::from_core_error(err) {
            // Only keep one error per category within a single diagnostic run.
            if !self.errors.iter().any(|(c, _)| *c == category) {
                self.errors.push((category, err.to_string()));
            }
        }
    }

    /// Returns `true` if any errors were recorded.
    pub const fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Iterate over recorded errors.
    pub fn iter(&self) -> impl Iterator<Item = &(ErrorCategory, String)> {
        self.errors.iter()
    }

    /// Count of distinct error categories.
    pub const fn category_count(&self) -> usize {
        self.errors.len()
    }
}

/// Format a per-ref diagnostic message for a failed check.
///
/// Uses the pattern `(check failed: reason)` as specified in the task.
pub fn format_check_failed(uses_value: &str, reason: &str) -> String {
    format!("{uses_value} (check failed: {reason})")
}

/// Send user-visible notifications for environment errors.
///
/// Rate limit and auth errors get `window/showMessage` (prominent).
/// Network and other errors get `window/logMessage` (output panel).
/// All are deduplicated by the session-scoped [`ErrorDeduplicator`].
pub async fn notify_errors(client: &Client, errors: &DiagnosticErrors, dedup: &ErrorDeduplicator) {
    for (category, message) in errors.iter() {
        if !dedup.should_notify(*category) {
            tracing::debug!(?category, "skipping duplicate error notification");
            continue;
        }

        match category {
            ErrorCategory::RateLimit => {
                client
                    .show_message(
                        MessageType::WARNING,
                        format!(
                            "actup: API rate limit exceeded. \
                             Version checks may be incomplete. \
                             Set GITHUB_TOKEN for higher limits. ({message})"
                        ),
                    )
                    .await;
            }
            ErrorCategory::Auth => {
                client
                    .show_message(
                        MessageType::WARNING,
                        format!(
                            "actup: Authentication failed. \
                             Set GITHUB_TOKEN environment variable \
                             with a valid token. ({message})"
                        ),
                    )
                    .await;
            }
            ErrorCategory::Forbidden => {
                client
                    .show_message(
                        MessageType::WARNING,
                        format!(
                            "actup: Permission denied. \
                             Your token may lack required scopes. ({message})"
                        ),
                    )
                    .await;
            }
            ErrorCategory::Network => {
                client
                    .log_message(
                        MessageType::WARNING,
                        format!("actup: Network error during version check: {message}"),
                    )
                    .await;
            }
            ErrorCategory::ProviderNotFound => {
                client
                    .log_message(MessageType::WARNING, format!("actup: {message}"))
                    .await;
            }
        }
    }

    // Aggregate status log if there were errors.
    if errors.has_errors() {
        let summary: Vec<&str> = errors.iter().map(|(_, msg)| msg.as_str()).collect();
        client
            .log_message(
                MessageType::LOG,
                format!(
                    "actup: diagnostic run had {} error(s): {}",
                    errors.category_count(),
                    summary.join("; "),
                ),
            )
            .await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn classify_rate_limit() {
        let err = CoreError::RateLimited {
            provider: "github".into(),
            retry_after: Some(std::time::Duration::from_secs(60)),
        };
        assert_eq!(
            ErrorCategory::from_core_error(&err),
            Some(ErrorCategory::RateLimit)
        );
    }

    #[test]
    fn classify_auth() {
        let err = CoreError::Auth {
            provider: "github".into(),
            reason: "bad token".into(),
        };
        assert_eq!(
            ErrorCategory::from_core_error(&err),
            Some(ErrorCategory::Auth)
        );
    }

    #[test]
    fn classify_network() {
        let err = CoreError::Network {
            operation: "get_versions".into(),
            message: "timeout".into(),
        };
        assert_eq!(
            ErrorCategory::from_core_error(&err),
            Some(ErrorCategory::Network)
        );
    }

    #[test]
    fn classify_user_error_returns_none() {
        let err = CoreError::NotFound {
            owner: "x".into(),
            repo: "y".into(),
            host: None,
        };
        assert!(ErrorCategory::from_core_error(&err).is_none());
    }

    #[test]
    fn classify_forbidden() {
        let err = CoreError::Forbidden {
            provider: "github".into(),
            reason: "insufficient scopes".into(),
        };
        assert_eq!(
            ErrorCategory::from_core_error(&err),
            Some(ErrorCategory::Forbidden)
        );
    }

    #[test]
    fn classify_provider_not_found() {
        let err = CoreError::ProviderNotFound {
            host: "gitlab.com".into(),
        };
        assert_eq!(
            ErrorCategory::from_core_error(&err),
            Some(ErrorCategory::ProviderNotFound)
        );
    }

    #[test]
    fn deduplicator_first_call_returns_true() {
        let dedup = ErrorDeduplicator::new();
        assert!(dedup.should_notify(ErrorCategory::RateLimit));
    }

    #[test]
    fn deduplicator_second_call_returns_false() {
        let dedup = ErrorDeduplicator::new();
        assert!(dedup.should_notify(ErrorCategory::RateLimit));
        assert!(!dedup.should_notify(ErrorCategory::RateLimit));
    }

    #[test]
    fn deduplicator_different_categories_independent() {
        let dedup = ErrorDeduplicator::new();
        assert!(dedup.should_notify(ErrorCategory::RateLimit));
        assert!(dedup.should_notify(ErrorCategory::Auth));
        assert!(!dedup.should_notify(ErrorCategory::RateLimit));
    }

    #[test]
    fn deduplicator_reset_allows_re_notify() {
        let dedup = ErrorDeduplicator::new();
        assert!(dedup.should_notify(ErrorCategory::Auth));
        assert!(!dedup.should_notify(ErrorCategory::Auth));
        dedup.reset(ErrorCategory::Auth);
        assert!(dedup.should_notify(ErrorCategory::Auth));
    }

    #[test]
    fn deduplicator_reset_all_clears_everything() {
        let dedup = ErrorDeduplicator::new();
        dedup.should_notify(ErrorCategory::RateLimit);
        dedup.should_notify(ErrorCategory::Auth);
        dedup.should_notify(ErrorCategory::Network);
        dedup.reset_all();
        assert!(dedup.should_notify(ErrorCategory::RateLimit));
        assert!(dedup.should_notify(ErrorCategory::Auth));
        assert!(dedup.should_notify(ErrorCategory::Network));
    }

    #[test]
    fn format_check_failed_message() {
        let msg = format_check_failed("actions/checkout@v4", "rate limited by github");
        assert_eq!(
            msg,
            "actions/checkout@v4 (check failed: rate limited by github)"
        );
    }

    #[test]
    fn diagnostic_errors_record_deduplicates_within_run() {
        let mut errors = DiagnosticErrors::default();
        let err1 = CoreError::RateLimited {
            provider: "github".into(),
            retry_after: None,
        };
        let err2 = CoreError::RateLimited {
            provider: "github".into(),
            retry_after: Some(std::time::Duration::from_secs(30)),
        };
        errors.record(&err1);
        errors.record(&err2);
        // Only one RateLimit entry.
        assert_eq!(errors.category_count(), 1);
    }

    #[test]
    fn diagnostic_errors_records_different_categories() {
        let mut errors = DiagnosticErrors::default();
        errors.record(&CoreError::RateLimited {
            provider: "github".into(),
            retry_after: None,
        });
        errors.record(&CoreError::Auth {
            provider: "github".into(),
            reason: "bad token".into(),
        });
        assert_eq!(errors.category_count(), 2);
    }

    #[test]
    fn diagnostic_errors_skips_user_errors() {
        let mut errors = DiagnosticErrors::default();
        errors.record(&CoreError::NotFound {
            owner: "x".into(),
            repo: "y".into(),
            host: None,
        });
        assert!(!errors.has_errors());
    }
}
