//! Workflow scanning and batched version fetching shared by `check`, `update` and `pin`.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use actup_core::{
    ActionRef, ActionRepoId, Config, DiscoveredFile, Error as CoreError, SourceFormat,
    UsesLocation, provider, version,
};
use itertools::Itertools;
use tracing::debug;

use crate::{
    common::{ErrorTally, WorkflowScanner},
    file_cache::FileCache,
    progress::Spinner,
};

/// Fetched versions keyed by repo without subpath, or the error that stopped the fetch.
pub type Versions = HashMap<ActionRepoId, Result<version::Set, CoreError>>;

/// A workflow or action file read for scanning.
pub struct ScannedFile {
    /// Path on disk.
    pub path: PathBuf,
    /// Path as displayed.
    pub relative: String,
    /// Content as read.
    pub content: String,
    /// The syntax its refs are read from.
    pub format: SourceFormat,
}

/// A remote `uses:` reference found in a [`ScannedFile`].
pub struct ScannedRef {
    /// Index of the file in [`Scan::files`].
    pub file: usize,
    /// Where the reference sits.
    pub loc: UsesLocation,
    /// The parsed reference.
    pub action_ref: ActionRef,
    /// The repo whose versions answer the reference.
    pub repo_id: ActionRepoId,
}

/// Every readable discovered file and the remote references in it.
pub struct Scan {
    /// Files in discovery order.
    pub files: Vec<ScannedFile>,
    /// References in file and line order.
    pub refs: Vec<ScannedRef>,
}

impl Scan {
    /// Read `discovered` and extract its references, recording unreadable files in `errors`.
    pub fn collect(
        scanner: &WorkflowScanner<'_>,
        discovered: &[DiscoveredFile],
        errors: &mut ErrorTally,
    ) -> Self {
        let mut files = Vec::new();
        let mut refs = Vec::new();
        for file in discovered {
            let Some(content) = scanner.read_file(file, errors) else {
                continue;
            };
            let index = files.len();
            let format = file.format();
            refs.extend(WorkflowScanner::extract_actions(&content, format).map(
                |(loc, action_ref)| ScannedRef {
                    file: index,
                    loc,
                    repo_id: action_ref.to_repo_id().without_subpath(),
                    action_ref,
                },
            ));
            files.push(ScannedFile {
                path: file.path().to_path_buf(),
                relative: scanner.relative_path(file),
                content,
                format,
            });
        }
        Self { files, refs }
    }

    /// The references in file `index`.
    pub fn refs_in(&self, index: usize) -> impl Iterator<Item = &ScannedRef> {
        self.refs.iter().filter(move |r| r.file == index)
    }

    /// Fetch versions in one batch for every repo that a served, unignored reference accepted by `wanted` names.
    pub async fn fetch(
        &self,
        scanner: &WorkflowScanner<'_>,
        config: &Config,
        registry: &provider::Registry,
        wanted: impl Fn(&ScannedRef) -> bool,
    ) -> Versions {
        let repos: Vec<ActionRepoId> = self
            .refs
            .iter()
            .filter(|r| scanner.provider_for(&r.action_ref).is_some())
            .filter(|r| !config.should_ignore(&r.action_ref))
            .filter(|r| wanted(r))
            .map(|r| r.repo_id.clone())
            .unique()
            .collect();
        if repos.is_empty() {
            return Versions::new();
        }
        debug!(
            total_actions = self.refs.len(),
            unique_repos = repos.len(),
            "batch fetching versions"
        );
        batch_fetch_versions(&repos, config, registry).await
    }
}

/// The fetched versions for `repo`, or the error that left them unfetched.
pub fn versions_for<'a>(
    versions: &'a Versions,
    repo: &ActionRepoId,
) -> Result<&'a version::Set, CoreError> {
    match versions.get(repo) {
        Some(Ok(set)) => Ok(set),
        Some(Err(e)) => Err(e.clone()),
        None => Err(CoreError::Internal {
            context: "batch fetch".to_string(),
            message: "no version fetch attempted".to_string(),
        }),
    }
}

/// Batch fetch versions for `repos` through the registry, using the file cache that `config` selects.
pub async fn batch_fetch_versions(
    repos: &[ActionRepoId],
    config: &Config,
    registry: &provider::Registry,
) -> Versions {
    let cache = Arc::new(if config.cache.enabled {
        config.cache.path.as_ref().map_or_else(
            || FileCache::new(config.cache.ttl),
            |path| FileCache::with_path(path.clone(), config.cache.ttl),
        )
    } else {
        FileCache::disabled()
    });

    let spinner = Spinner::stderr();
    let versions = actup_provider_github::fetch_versions_with_registry(
        repos, config, registry, cache, &spinner,
    )
    .await;
    spinner.finish();
    versions
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use actup_core::{
        BoxFuture,
        provider::{ActionYml, Kind, Provider, Sha40},
    };

    use super::*;
    use crate::cli::OutputFormat;

    struct CountingProvider {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Provider for CountingProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(repo.repo().to_string());
            }
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            let err = CoreError::InvalidRef {
                input: ref_name.to_string(),
                reason: "unused".to_string(),
            };
            Box::pin(async move { Err(err) })
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

    #[tokio::test]
    async fn fetch_asks_once_per_wanted_repo() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        std::fs::write(
            workflows.join("a.yml"),
            "steps:\n  - uses: o/a@v1\n  - uses: o/a/sub@v2\n  - uses: skip/b@v1\n",
        )?;
        std::fs::write(
            workflows.join("b.yml"),
            "steps:\n  - uses: o/c@main\n  - uses: ghe.corp.example/o/d@v1\n  - uses: o/e@v1\n  - uses: ./local\n",
        )?;
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                "[scan]\nignore = [\"skip/*\"]\n\n[cache]\nenabled = false\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        registry.register(Box::new(CountingProvider {
            calls: Arc::clone(&calls),
        }));
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut errors = ErrorTally::default();

        let scan = Scan::collect(&scanner, &scanner.discover(), &mut errors);
        let found: Vec<(usize, usize, String)> = scan
            .refs
            .iter()
            .map(|r| (r.file, r.loc.line(), r.action_ref.to_string()))
            .collect();
        assert_eq!(
            found,
            [
                (0, 2, "o/a@v1".to_string()),
                (0, 3, "o/a/sub@v2".to_string()),
                (0, 4, "skip/b@v1".to_string()),
                (1, 2, "o/c@main".to_string()),
                (1, 3, "ghe.corp.example/o/d@v1".to_string()),
                (1, 4, "o/e@v1".to_string()),
            ]
        );
        assert_eq!(scan.refs_in(1).count(), 3);

        let versions = scan
            .fetch(&scanner, &config, &registry, |r| r.action_ref.repo() != "e")
            .await;
        let mut calls = calls.lock().map_err(|e| e.to_string())?.clone();
        calls.sort_unstable();
        assert_eq!(calls, ["a", "c"]);
        let mut fetched: Vec<&str> = versions.keys().map(ActionRepoId::repo).collect();
        fetched.sort_unstable();
        assert_eq!(fetched, ["a", "c"]);
        assert!(versions_for(&versions, &scan.refs[1].repo_id).is_ok());
        assert!(matches!(
            versions_for(&versions, &scan.refs[5].repo_id),
            Err(CoreError::Internal { .. })
        ));
        assert_eq!(errors.total(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn fetch_without_wanted_refs_asks_nothing() {
        let config = Config::default();
        let registry = provider::Registry::new();
        let scanner =
            WorkflowScanner::new(Path::new("/repo"), &config, &registry, OutputFormat::Json);
        let scan = Scan {
            files: Vec::new(),
            refs: Vec::new(),
        };
        assert!(
            scan.fetch(&scanner, &config, &registry, |_| true)
                .await
                .is_empty()
        );
        assert!(
            batch_fetch_versions(&[], &config, &registry)
                .await
                .is_empty()
        );
    }
}
