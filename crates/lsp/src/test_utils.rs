//! Shared test utilities for the LSP crate.
//!
//! Provides a [`MockProvider`] implementing `Provider` for unit tests that
//! need version resolution without network access, plus common helpers for
//! constructing LSP types (ranges, diagnostics, version sets).

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use actup_core::{
    ActionRepoId, Capabilities, Config, Error as CoreError, HostKind, Preset, Registration,
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
        repin_comment: None,
        replace_end_col: None,
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
    default_branch: Result<String, CoreError>,
    rewrite: Option<(std::path::PathBuf, String)>,
    search: Result<provider::Search, CoreError>,
    searches: Arc<AtomicUsize>,
    repo_state: Result<Option<provider::RepoState>, CoreError>,
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
            default_branch: Ok("main".to_string()),
            rewrite: None,
            search: Ok(provider::Search::Unsupported),
            searches: Arc::new(AtomicUsize::new(0)),
            repo_state: Ok(None),
        }
    }

    pub fn with_resolve(mut self, result: Result<&str, CoreError>) -> Self {
        self.resolve = result.map(str::to_string);
        self
    }

    pub fn with_default_branch(mut self, result: Result<&str, CoreError>) -> Self {
        self.default_branch = result.map(str::to_string);
        self
    }

    pub fn resolve_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.resolve_calls)
    }

    /// Answer every repository state lookup with `state`.
    pub fn with_repo_state(
        mut self,
        state: Result<Option<provider::RepoState>, CoreError>,
    ) -> Self {
        self.repo_state = state;
        self
    }

    /// Answer every action search with `search`.
    pub fn with_search(mut self, search: Result<provider::Search, CoreError>) -> Self {
        self.search = search;
        self
    }

    /// Count of action searches answered so far.
    pub fn search_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.searches)
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
        if let Some((path, content)) = &self.rewrite
            && let Err(e) = std::fs::write(path, content)
        {
            panic!("rewriting {}: {e}", path.display());
        }
        let key = Self::repo_key(repo);
        let result = self.versions.get(&key).map_or_else(
            || Ok(version::Set::default()),
            |r| match r {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(e.clone()),
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
                source: None,
            }),
            Err(e) => Err(e.clone()),
        };
        Box::pin(async move { result })
    }

    fn get_default_branch(
        &self,
        _repo: &ActionRepoId,
    ) -> BoxFuture<'_, actup_core::Result<String>> {
        let result = self.default_branch.clone();
        Box::pin(async move { result })
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

    fn get_repo_state(
        &self,
        _repo: &ActionRepoId,
    ) -> BoxFuture<'_, actup_core::Result<Option<provider::RepoState>>> {
        let result = self.repo_state.clone();
        Box::pin(async move { result })
    }

    fn search_actions(&self, _query: &str) -> BoxFuture<'_, actup_core::Result<provider::Search>> {
        self.searches.fetch_add(1, Ordering::SeqCst);
        let result = self.search.clone();
        Box::pin(async move { result })
    }
}

/// A [`actup_core::Clock`] that moves only when a test moves it.
#[derive(Debug)]
pub struct StepClock(std::sync::Mutex<std::time::SystemTime>);

impl StepClock {
    /// Start at the Unix epoch.
    pub const fn new() -> Self {
        Self(std::sync::Mutex::new(std::time::SystemTime::UNIX_EPOCH))
    }

    /// Move the clock forward by `by`.
    pub fn advance(&self, by: std::time::Duration) {
        self.set(|now| now.checked_add(by));
    }

    /// Move the clock back by `by`.
    pub fn rewind(&self, by: std::time::Duration) {
        self.set(|now| now.checked_sub(by));
    }

    fn set(&self, to: impl FnOnce(std::time::SystemTime) -> Option<std::time::SystemTime>) {
        let mut now = match self.0.lock() {
            Ok(now) => now,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(moved) = to(*now) {
            *now = moved;
        }
    }
}

impl actup_core::Clock for StepClock {
    fn now(&self) -> std::time::SystemTime {
        match self.0.lock() {
            Ok(now) => *now,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

/// A GitHub-kind registration whose providers are [`MockProvider`]s.
///
/// Its providers serve `actions/checkout` at `v4.2.0` on [`DUMMY_SHA`] and
/// empty version sets for every other repo.
pub static MOCK_REGISTRATION: Registration = Registration {
    kind: HostKind::GitHub,
    presets: &[Preset {
        host: "github.com",
        api_base: "https://api.github.com",
        token_env: "GITHUB_TOKEN",
    }],
    default_token_env: Some("GITHUB_TOKEN"),
    infer_api_base: |host| (host != "github.com").then(|| format!("https://{host}/api/v3")),
    capabilities: Capabilities {
        supports_graphql: true,
        supports_rest: true,
    },
    create: |config| {
        Ok(Box::new(MockProvider::new(&config.host).with_versions(
            "actions",
            "checkout",
            simple_version_set("v4.2.0", DUMMY_SHA),
        )))
    },
};

/// Build a registry of [`MOCK_REGISTRATION`] providers from `config`.
pub fn mock_registry_from_config(config: &Config) -> actup_core::Result<provider::Registry> {
    provider::Registry::from_config(
        config,
        &[&MOCK_REGISTRATION],
        &actup_core::Retention::expiring(config.cache.ttl),
    )
}

/// Mock providers and a disabled file cache, for a backend that never leaves the process.
pub const MOCK_PROVIDERS: crate::provider_setup::Providers = crate::provider_setup::Providers {
    registry: mock_registry_from_config,
    file_cache: |_| crate::file_cache::FileCache::disabled(),
};

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
