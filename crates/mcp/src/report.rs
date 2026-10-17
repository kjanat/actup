//! Tool results: the shared status vocabulary and the JSON envelope.

use actup_core::{CheckStatus, Error, Provider};
use rust_mcp_sdk::schema::{CallToolResult, TextContent, schema_utils::CallToolError};
use serde::Serialize;
use serde_json::{Map, Value};

/// Outcome of resolving a reference, shared by every tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// The provider answered and nothing needs to change.
    UpToDate,
    /// A newer allowed version exists, or a mutator changed a ref.
    Outdated,
    /// A network, auth or provider failure prevented verification.
    Unknown,
    /// The repository or ref does not exist, or the input names no valid ref.
    Unresolvable,
    /// The provider rate limited the request.
    RateLimited,
}

impl Status {
    /// The status a provider error leaves.
    #[must_use]
    pub const fn of_error(error: &Error) -> Self {
        match error {
            Error::RateLimited { .. } => Self::RateLimited,
            e if e.is_user_error() => Self::Unresolvable,
            _ => Self::Unknown,
        }
    }

    /// The status a version check leaves.
    #[must_use]
    pub const fn of_check(status: &CheckStatus) -> Self {
        match status {
            CheckStatus::UpToDate
            | CheckStatus::Pinned { .. }
            | CheckStatus::Ignored
            | CheckStatus::Floating { .. } => Self::UpToDate,
            CheckStatus::Outdated { .. } | CheckStatus::PinBranch { .. } => Self::Outdated,
            CheckStatus::Unresolvable {
                is_environment_error: true,
                ..
            } => Self::Unknown,
            CheckStatus::Unresolvable { .. } => Self::Unresolvable,
        }
    }

    const fn failed(self) -> bool {
        matches!(self, Self::Unknown | Self::Unresolvable | Self::RateLimited)
    }
}

/// The provider and host that served a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Served {
    /// Provider kind, or `unknown` when no provider serves the host.
    pub provider: String,
    /// Host the reference resolves to.
    pub host: String,
}

impl Served {
    /// Describe `provider`.
    #[must_use]
    pub fn by(provider: &dyn Provider) -> Self {
        Self {
            provider: provider.kind().to_string(),
            host: provider.host().to_string(),
        }
    }

    /// Describe a host that no provider serves.
    #[must_use]
    pub fn unserved(host: &str) -> Self {
        Self {
            provider: "unknown".to_string(),
            host: host.to_string(),
        }
    }
}

/// A finished tool result as a JSON object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    body: Map<String, Value>,
    failed: bool,
}

impl Reply {
    /// Build a reply from `served`, `status` and the fields of `data`.
    ///
    /// # Errors
    ///
    /// Returns an error if `data` does not serialize to a JSON object.
    pub fn new(
        served: Option<&Served>,
        status: Status,
        data: impl Serialize,
    ) -> Result<Self, CallToolError> {
        let Value::Object(mut body) = serde_json::to_value(data).map_err(CallToolError::new)?
        else {
            return Err(CallToolError::from_message(
                "tool data is not a JSON object",
            ));
        };
        if let Some(served) = served {
            body.insert("provider".into(), served.provider.clone().into());
            body.insert("host".into(), served.host.clone().into());
        }
        body.insert(
            "status".into(),
            serde_json::to_value(status).map_err(CallToolError::new)?,
        );
        Ok(Self {
            body,
            failed: status.failed(),
        })
    }

    /// Build a reply for a provider error.
    #[must_use]
    pub fn failure(served: &Served, error: &Error) -> Self {
        let mut body = Map::new();
        body.insert("provider".into(), served.provider.clone().into());
        body.insert("host".into(), served.host.clone().into());
        body.insert(
            "status".into(),
            serde_json::to_value(Status::of_error(error)).unwrap_or(Value::Null),
        );
        body.insert("error".into(), actup_core::error::chain(error).into());
        Self { body, failed: true }
    }

    /// Mark the reply as an error result.
    #[must_use]
    pub const fn failing(mut self) -> Self {
        self.failed = true;
        self
    }

    /// Whether the reply is a success worth caching.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        !self.failed
    }

    /// Convert to an MCP result carrying the object as text and structured content.
    #[must_use]
    pub fn into_result(self) -> CallToolResult {
        let text = serde_json::to_string_pretty(&self.body).unwrap_or_default();
        let mut result = CallToolResult::text_content(vec![TextContent::from(text)])
            .with_structured_content(self.body);
        if self.failed {
            result.is_error = Some(true);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> String {
        String::new()
    }

    #[test]
    fn of_error_separates_rate_limits_user_errors_and_the_rest() {
        let rate = Error::RateLimited {
            provider: s(),
            retry_after: None,
        };
        let missing = Error::NotFound {
            owner: s(),
            repo: s(),
            host: None,
        };
        let network = Error::Network {
            operation: s(),
            message: s(),
            source: None,
        };
        let internal = Error::Internal {
            context: s(),
            message: s(),
            source: None,
        };
        assert_eq!(Status::of_error(&rate), Status::RateLimited);
        assert_eq!(Status::of_error(&missing), Status::Unresolvable);
        assert_eq!(Status::of_error(&network), Status::Unknown);
        assert_eq!(Status::of_error(&internal), Status::Unknown);
    }

    #[test]
    fn of_check_maps_every_check_status() {
        let target = std::sync::Arc::new(actup_core::Info::new(
            actup_core::Version::parse("v2"),
            "b".repeat(40),
        ));
        let cases = [
            (CheckStatus::UpToDate, Status::UpToDate),
            (
                CheckStatus::Pinned {
                    sha: s(),
                    tag: None,
                },
                Status::UpToDate,
            ),
            (CheckStatus::Ignored, Status::UpToDate),
            (CheckStatus::Floating { ref_name: s() }, Status::UpToDate),
            (
                CheckStatus::Outdated {
                    current: s(),
                    target,
                },
                Status::Outdated,
            ),
            (
                CheckStatus::PinBranch {
                    ref_name: s(),
                    sha: s(),
                },
                Status::Outdated,
            ),
            (
                CheckStatus::Unresolvable {
                    reason: s(),
                    is_environment_error: true,
                },
                Status::Unknown,
            ),
            (
                CheckStatus::Unresolvable {
                    reason: s(),
                    is_environment_error: false,
                },
                Status::Unresolvable,
            ),
        ];
        for (check, status) in cases {
            assert_eq!(Status::of_check(&check), status, "{check:?}");
        }
    }

    #[test]
    fn reply_carries_provider_host_status_and_data() -> Result<(), Box<dyn std::error::Error>> {
        let served = Served::unserved("ghe.corp.com");
        let reply = Reply::new(
            Some(&served),
            Status::UpToDate,
            serde_json::json!({ "x": 1 }),
        )
        .map_err(|e| e.to_string())?;
        assert!(reply.succeeded());
        let result = reply.into_result();
        assert_eq!(result.is_error, None);
        assert_eq!(
            result.structured_content.map(Value::Object),
            Some(serde_json::json!({
                "provider": "unknown",
                "host": "ghe.corp.com",
                "status": "upToDate",
                "x": 1,
            }))
        );
        Ok(())
    }

    #[test]
    fn reply_rejects_non_object_data() {
        assert!(Reply::new(None, Status::UpToDate, 1).is_err());
    }

    #[test]
    fn failing_statuses_and_failures_mark_the_result_as_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let reply = Reply::new(None, Status::RateLimited, serde_json::json!({}))
            .map_err(|e| e.to_string())?;
        assert!(!reply.succeeded());
        assert_eq!(reply.into_result().is_error, Some(true));
        let failing = Reply::new(None, Status::UpToDate, serde_json::json!({}))
            .map_err(|e| e.to_string())?
            .failing();
        assert_eq!(failing.into_result().is_error, Some(true));
        let failure = Reply::failure(
            &Served::unserved("h"),
            &Error::ProviderNotFound { host: "h".into() },
        );
        assert_eq!(
            failure.into_result().structured_content.map(Value::Object),
            Some(serde_json::json!({
                "provider": "unknown",
                "host": "h",
                "status": "unknown",
                "error": "no provider configured for host: h",
            }))
        );
        Ok(())
    }

    #[test]
    fn failure_error_carries_the_cause() {
        let failure = Reply::failure(
            &Served::unserved("h"),
            &Error::Network {
                operation: "GET request".into(),
                message: String::new(),
                source: Some(std::sync::Arc::new(std::io::Error::other(
                    "connection refused",
                ))),
            },
        );
        assert_eq!(
            failure.body.get("error"),
            Some(&Value::from(
                "network error during GET request: connection refused"
            ))
        );
    }
}
