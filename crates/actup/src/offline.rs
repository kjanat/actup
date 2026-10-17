//! Provider wrapper for `--offline` runs.

use actup_core::{
    ActionRepoId, BoxFuture, Error as CoreError, Provider,
    provider::{ActionYml, Kind, Sha40},
    version,
};

/// A provider that answers every lookup with a cache miss.
pub struct Offline(Box<dyn Provider>);

impl Offline {
    /// Wrap `inner` so it never reaches the network.
    #[must_use]
    pub fn wrap(inner: Box<dyn Provider>) -> Box<dyn Provider> {
        Box::new(Self(inner))
    }
}

fn miss<T>(lookup: &str, repo: &ActionRepoId) -> BoxFuture<'static, actup_core::Result<T>>
where
    T: 'static,
{
    let err = CoreError::Cache {
        operation: format!("offline {lookup}"),
        reason: format!("{}/{} is not in the cache", repo.owner(), repo.repo()),
    };
    Box::pin(async move { Err(err) })
}

impl Provider for Offline {
    fn kind(&self) -> Kind {
        self.0.kind()
    }

    fn host(&self) -> &str {
        self.0.host()
    }

    fn cache_namespace(&self) -> &str {
        self.0.cache_namespace()
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<version::Set>> {
        miss("version lookup", repo)
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        _ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
        miss("ref lookup", repo)
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<String>> {
        miss("default branch lookup", repo)
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        _ref_name: Option<&str>,
    ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
        miss("readme lookup", repo)
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        _ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
        miss("action.yml lookup", repo)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc, time::Duration};

    use actup_core::{Config, cache::Cache, provider};

    use super::*;
    use crate::{
        cli::{CheckFormat, CliExitCode, OutputFormat},
        commands::check,
        file_cache::FileCache,
    };

    struct Unreachable;

    impl Provider for Unreachable {
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
            panic!("fetched {}", repo.repo());
        }

        fn resolve_ref_to_sha(
            &self,
            repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            panic!("resolved {}", repo.repo());
        }

        fn get_default_branch(
            &self,
            repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            panic!("asked the default branch of {}", repo.repo());
        }

        fn get_readme(
            &self,
            repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            panic!("asked the readme of {}", repo.repo());
        }

        fn get_action_yml(
            &self,
            repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            panic!("asked the action.yml of {}", repo.repo());
        }
    }

    #[tokio::test]
    async fn offline_lookups_are_cache_misses() {
        let offline = Offline::wrap(Box::new(Unreachable));
        let repo = ActionRepoId::simple("o", "r");
        assert_eq!(offline.kind(), Kind::GitHub);
        assert_eq!(offline.host(), "github.com");
        assert_eq!(offline.cache_namespace(), "github");
        assert!(!offline.supports_batch_fetch());
        let errors = [
            offline.get_versions(&repo).await.err(),
            offline.resolve_ref_to_sha(&repo, "v1").await.err(),
            offline.get_default_branch(&repo).await.err(),
            offline.get_readme(&repo, None).await.err(),
            offline.get_action_yml(&repo, "v1").await.err(),
        ];
        for err in errors {
            let Some(err @ CoreError::Cache { .. }) = err else {
                panic!("expected a cache miss, got {err:?}");
            };
            assert!(err.is_environment_error(), "{err}");
            assert!(err.to_string().contains("o/r is not in the cache"), "{err}");
        }
    }

    #[tokio::test]
    async fn offline_check_and_lookup_read_the_cache_and_fail_on_a_miss()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join("cache");
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        std::fs::write(&file, "steps:\n  - uses: o/cached@v1.0.0\n")?;
        let cache = FileCache::with_path(cache_dir.clone(), Duration::from_secs(3600));
        let tag = Arc::new(version::Info::new(
            version::Version::parse("v1.0.0"),
            "a".repeat(40),
        ));
        let mut versions = version::Set::build(
            vec![Arc::clone(&tag)],
            std::collections::HashMap::from([(1, Arc::clone(&tag))]),
            std::collections::HashMap::new(),
            Some(tag),
        );
        versions.set_listing(version::Listing::Complete);
        cache
            .set(
                &ActionRepoId::simple("o", "cached").cache_key("github", "github.com"),
                actup_core::cache::Entry::new(versions),
            )
            .await?;
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                &format!("[cache]\npath = {:?}\n", cache_dir.display().to_string()),
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(Unreachable));
        let registry = registry.map_providers(Offline::wrap);

        let code = check::run(
            dir.path(),
            &[],
            CheckFormat::Json,
            false,
            &config,
            &registry,
        )
        .await;
        assert_eq!(code, i32::from(CliExitCode::Success));

        std::fs::write(
            &file,
            "steps:\n  - uses: o/cached@v1.0.0\n  - uses: o/missing@v1\n",
        )?;
        let code = check::run(
            dir.path(),
            &[],
            CheckFormat::Json,
            false,
            &config,
            &registry,
        )
        .await;
        assert_eq!(code, i32::from(CliExitCode::EnvironmentError));

        for (action, expected) in [
            ("o/cached", CliExitCode::Success),
            ("o/missing", CliExitCode::EnvironmentError),
        ] {
            let args = crate::cli::LatestArgs {
                action: Some(action.to_string()),
                format: OutputFormat::Json,
            };
            let code = crate::commands::latest::run(&args, &config, &registry).await;
            assert_eq!(code, i32::from(expected), "{action}");
        }
        Ok(())
    }
}
