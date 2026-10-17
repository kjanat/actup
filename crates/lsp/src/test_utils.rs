//! Shared test utilities for the LSP crate.
//!
//! Provides a [`MockProvider`] implementing `Provider` for unit tests that
//! need version resolution without network access, plus common helpers for
//! constructing LSP types (ranges, diagnostics, version sets).

#![allow(dead_code, clippy::unwrap_used)]
// Test utilities are only used in #[cfg(test)] contexts where unwrap is acceptable.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use actup_core::{
    ActionRepoId, Error as CoreError,
    compat::BoxFuture,
    provider::{self, ActionYml, Kind, Provider, Sha40},
    version,
};
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::diagnostics::{DIAGNOSTIC_SOURCE, DiagnosticData};

/// Build a [`Range`] at `(line, col_start..col_end)`.
pub fn make_range(line: u32, col_start: u32, col_end: u32) -> Range {
    Range {
        start: Position {
            line,
            character: col_start,
        },
        end: Position {
            line,
            character: col_end,
        },
    }
}

/// Build an `Outdated` diagnostic with embedded [`DiagnosticData`].
///
/// The message is derived from `uses_value` and `target_ref` to match
/// production output format rather than hardcoding values.
pub fn make_outdated_diagnostic(range: Range, uses_value: &str, target_ref: &str) -> Diagnostic {
    let data = DiagnosticData::Outdated {
        target_ref: target_ref.into(),
        repin_comment: None,
        replace_end_col: None,
    };
    // Extract current ref from uses_value (after @) for realistic messages.
    let current = uses_value.rsplit_once('@').map_or("unknown", |(_, r)| r);
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        code: Some(NumberOrString::String("outdated".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!(
            "{uses_value}: {current} -> {target_ref} ({})",
            &DUMMY_SHA[..7],
        ),
        data: serde_json::to_value(data).ok(),
        ..Default::default()
    }
}

/// Build an `Unresolvable` diagnostic with embedded [`DiagnosticData`].
pub fn make_unresolvable_diagnostic(range: Range, suggested: Option<&str>) -> Diagnostic {
    let data = DiagnosticData::Unresolvable {
        suggested_ref: suggested.map(String::from),
    };
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("unresolvable".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "bad/repo@v1: repository not found".into(),
        data: serde_json::to_value(data).ok(),
        ..Default::default()
    }
}

/// Build a `Pinned` diagnostic (no embedded data — pinned refs have no code action).
pub fn make_pinned_diagnostic(range: Range, sha: &str, tag: Option<&str>) -> Diagnostic {
    let tag_info = tag.map_or_else(|| "unknown version".into(), |t| format!("= {t}"));
    let short_sha = sha.get(..7).unwrap_or(sha);
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::HINT),
        code: Some(NumberOrString::String("pinned".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: format!("owner/repo@{short_sha}: pinned to {short_sha} ({tag_info})"),
        ..Default::default()
    }
}

/// Build a simple [`version::Info`] with a known SHA.
pub fn make_version_info(version_str: &str, sha: &str) -> Arc<version::Info> {
    Arc::new(version::Info::new(
        version::Version::parse(version_str),
        sha,
    ))
}

/// Dummy 40-char SHA for tests.
pub const DUMMY_SHA: &str = "abc1234567890abcdef1234567890abcdef12345";

/// Second dummy SHA for tests requiring two distinct SHAs.
pub const DUMMY_SHA_2: &str = "def4567890abcdef1234567890abcdef456789ab";

/// A configurable mock [`Provider`] for tests.
///
/// Returns pre-configured version sets per repo, or errors. Falls back to
/// empty `version::Set` for unknown repos.
pub struct MockProvider {
    kind: Kind,
    host: String,
    /// Pre-configured version sets, keyed by `"owner/repo"`.
    versions: HashMap<String, Result<version::Set, CoreError>>,
    calls: Arc<AtomicUsize>,
    resolve: Result<String, CoreError>,
    resolve_calls: Arc<AtomicUsize>,
    rewrite: Option<(std::path::PathBuf, String)>,
}

impl MockProvider {
    /// Create a mock provider that returns empty version sets for all repos.
    pub fn new(host: &str) -> Self {
        Self {
            kind: Kind::GitHub,
            host: host.to_string(),
            versions: HashMap::new(),
            calls: Arc::new(AtomicUsize::new(0)),
            resolve: Ok(DUMMY_SHA.to_string()),
            resolve_calls: Arc::new(AtomicUsize::new(0)),
            rewrite: None,
        }
    }

    pub fn with_resolve(mut self, result: Result<&str, CoreError>) -> Self {
        self.resolve = result.map(str::to_string);
        self
    }

    pub fn resolve_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.resolve_calls)
    }

    /// Overwrite `path` with `content` on every version lookup.
    pub fn with_rewrite(mut self, path: std::path::PathBuf, content: &str) -> Self {
        self.rewrite = Some((path, content.to_string()));
        self
    }

    /// Add a version set for a specific repo.
    pub fn with_versions(mut self, owner: &str, repo: &str, versions: version::Set) -> Self {
        self.versions
            .insert(format!("{owner}/{repo}"), Ok(versions));
        self
    }

    /// Add an error response for a specific repo.
    pub fn with_error(mut self, owner: &str, repo: &str, error: CoreError) -> Self {
        self.versions.insert(format!("{owner}/{repo}"), Err(error));
        self
    }

    fn repo_key(repo: &ActionRepoId) -> String {
        format!("{}/{}", repo.owner(), repo.repo())
    }

    pub fn call_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }
}

impl Provider for MockProvider {
    fn kind(&self) -> Kind {
        self.kind
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<version::Set>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((path, content)) = &self.rewrite {
            std::fs::write(path, content).unwrap();
        }
        let key = Self::repo_key(repo);
        let result = self.versions.get(&key).map_or_else(
            || Ok(version::Set::default()),
            |r| match r {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(clone_core_error(e)),
            },
        );
        Box::pin(async move { result })
    }

    fn resolve_ref_to_sha(
        &self,
        _repo: &ActionRepoId,
        _ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        let result = match &self.resolve {
            Ok(sha) => Sha40::new(sha.clone()).ok_or_else(|| CoreError::Internal {
                context: "mock resolve".to_string(),
                message: format!("invalid SHA {sha}"),
            }),
            Err(e) => Err(clone_core_error(e)),
        };
        Box::pin(async move { result })
    }

    fn get_default_branch(
        &self,
        _repo: &ActionRepoId,
    ) -> BoxFuture<'_, actup_core::Result<String>> {
        Box::pin(async { Ok("main".to_string()) })
    }

    fn get_readme(
        &self,
        _repo: &ActionRepoId,
        _ref_name: Option<&str>,
    ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    fn get_action_yml(
        &self,
        _repo: &ActionRepoId,
        _ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
        Box::pin(async { Ok(ActionYml::default()) })
    }
}

/// Build a [`provider::Registry`] from a single mock provider.
pub fn mock_registry(provider: MockProvider) -> provider::Registry {
    let host = provider.host().to_string();
    let mut registry = provider::Registry::new();
    registry.set_default_host(&host);
    registry.register(Box::new(provider));
    registry
}

/// Build a simple version set with one semver tag as latest.
pub fn simple_version_set(version_str: &str, sha: &str) -> version::Set {
    let info = make_version_info(version_str, sha);
    let v = info.version().clone();
    let mut latest_per_major = HashMap::new();
    if let version::Version::Semver { major, .. } = &v {
        latest_per_major.insert(*major, Arc::clone(&info));
    }
    version::Set::build(
        vec![Arc::clone(&info)],
        latest_per_major,
        HashMap::new(),
        Some(info),
    )
}

/// Clone a `CoreError` (errors don't impl Clone, so we reconstruct).
///
/// Uses exhaustive matching so new variants cause a compile error here
/// rather than silently falling through to a catch-all.
fn clone_core_error(e: &CoreError) -> CoreError {
    match e {
        CoreError::RateLimited {
            provider,
            retry_after,
        } => CoreError::RateLimited {
            provider: provider.clone(),
            retry_after: *retry_after,
        },
        CoreError::Auth { provider, reason } => CoreError::Auth {
            provider: provider.clone(),
            reason: reason.clone(),
        },
        CoreError::Network { operation, message } => CoreError::Network {
            operation: operation.clone(),
            message: message.clone(),
        },
        CoreError::ProviderNotFound { host } => CoreError::ProviderNotFound { host: host.clone() },
        CoreError::Forbidden { provider, reason } => CoreError::Forbidden {
            provider: provider.clone(),
            reason: reason.clone(),
        },
        CoreError::NotFound { owner, repo, host } => CoreError::NotFound {
            owner: owner.clone(),
            repo: repo.clone(),
            host: host.clone(),
        },
        CoreError::InvalidRef { input, reason } => CoreError::InvalidRef {
            input: input.clone(),
            reason: reason.clone(),
        },
        CoreError::InvalidConfig { path, reason } => CoreError::InvalidConfig {
            path: path.clone(),
            reason: reason.clone(),
        },
        CoreError::YamlParse { path, reason } => CoreError::YamlParse {
            path: path.clone(),
            reason: reason.clone(),
        },
        CoreError::RefNotFound {
            owner,
            repo,
            ref_name,
            host,
        } => CoreError::RefNotFound {
            owner: owner.clone(),
            repo: repo.clone(),
            ref_name: ref_name.clone(),
            host: host.clone(),
        },
        CoreError::Cache { operation, reason } => CoreError::Cache {
            operation: operation.clone(),
            reason: reason.clone(),
        },
        CoreError::Internal { context, message } => CoreError::Internal {
            context: context.clone(),
            message: message.clone(),
        },
    }
}
