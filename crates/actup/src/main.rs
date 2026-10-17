//! actup CLI - GitHub Actions version manager

#![warn(missing_docs)]

mod cli;
mod commands;
mod common;
mod offline;
mod output;
mod progress;
mod sarif;
mod scan;
mod structured;

use anstream::{eprintln, println};
use std::{process::ExitCode, sync::Arc};

use actup_core::{
    Config, FileCache, Registration, Retention, error::chain, neutralize_workflow_commands,
    provider, sanitize_line, user_config_path, validate_no_duplicate_presets,
};
use tracing_subscriber::EnvFilter;

use crate::cli::{CacheMode, Cli, CliExitCode, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::try_parse_args(std::env::args_os()).unwrap_or_else(|e| e.exit());
    cli.color_choice().write_global();

    // Initialize tracing
    let filter = if cli.verbose {
        EnvFilter::new(
            "actup=debug,actup_core=debug,actup_provider_github=debug,actup_provider_gitlab=debug,actup_provider_gitea=debug",
        )
    } else {
        EnvFilter::new("actup=info")
    };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(|| CommandSafe(std::io::stderr()))
        .with_ansi(anstream::AutoStream::choice(&std::io::stderr()) != anstream::ColorChoice::Never)
        .init();

    run(cli).await.into()
}

fn load_config(explicit: Option<&std::path::Path>, start_dir: &std::path::Path) -> Option<Config> {
    match Config::load(explicit, user_config_path().as_deref(), start_dir, None) {
        Ok(config) => {
            for source in &config.sources {
                tracing::info!(%source, "loaded config");
            }
            Some(config)
        }
        Err(e) => {
            eprintln!("Error: {}", sanitize_line(&chain(&e)));
            None
        }
    }
}

async fn run(cli: Cli) -> CliExitCode {
    // Handle completions early (no config/registry needed)
    if let Some(Command::Completions { shell }) = cli.command {
        let Some(shell) = shell.or_else(Cli::detect_shell) else {
            eprintln!(
                "Could not detect shell from $SHELL. Specify one: bash, zsh, fish, elvish, powershell"
            );
            return CliExitCode::EnvironmentError;
        };
        Cli::print_completions(shell);
        return CliExitCode::Success;
    }

    if matches!(cli.command, Some(Command::Schema)) {
        return commands::schema::run();
    }

    // No subcommand and no positional action argument: print help
    if cli.command.is_none() && cli.latest.action.is_none() {
        let _ = <Cli as clap::CommandFactory>::command()
            .color(cli.color)
            .print_help();
        println!();
        return CliExitCode::Success;
    }

    let (config, registry, cache) =
        match start_dir(cli.command.as_ref()).and_then(|start_dir| setup(&cli, &start_dir)) {
            Ok(setup) => setup,
            Err(code) => return code,
        };

    match cli.command {
        Some(Command::Check {
            path,
            strict,
            mut fail_on,
            format,
            github_annotations,
        }) => {
            if strict {
                fail_on.push(cli::FailOn::Outdated);
            }
            commands::check::run(
                &path,
                &fail_on,
                format,
                github_annotations,
                &config,
                &registry,
                &cache,
            )
            .await
        }
        Some(
            Command::Update {
                path,
                dry_run,
                format,
            }
            | Command::Fix {
                path,
                dry_run,
                format,
            },
        ) => commands::update::run(&path, dry_run, format, &config, &registry, &cache).await,
        Some(Command::Pin {
            path,
            unpin,
            dry_run,
            format,
        }) => {
            let mode = commands::pin::Mode::from_unpin_flag(unpin);
            commands::pin::run(&path, mode, dry_run, format, &config, &registry, &cache).await
        }
        Some(Command::Config { path, format }) => {
            let registrations = available_registrations();
            commands::config::run(&path, format, &config, &registrations)
        }
        Some(Command::Cache { action, format }) => {
            commands::cache::run(&action, format, &config).await
        }
        Some(Command::Completions { .. } | Command::Schema) => {
            unreachable!("handled before config load")
        }
        None => commands::latest::run(&cli.latest, &config, &registry, &cache).await,
    }
}

struct CommandSafe<W>(W);

impl<W: std::io::Write> std::io::Write for CommandSafe<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.0
            .write_all(neutralize_workflow_commands(&text).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn start_dir(command: Option<&Command>) -> Result<std::path::PathBuf, CliExitCode> {
    match command {
        Some(
            Command::Check { path, .. }
            | Command::Update { path, .. }
            | Command::Fix { path, .. }
            | Command::Pin { path, .. }
            | Command::Config { path, .. },
        ) => Ok(path.clone()),
        Some(Command::Cache { .. }) | None => std::env::current_dir().map_err(|e| {
            eprintln!(
                "Error reading current directory: {}",
                sanitize_line(&e.to_string())
            );
            CliExitCode::EnvironmentError
        }),
        Some(Command::Completions { .. } | Command::Schema) => unreachable!("handled above"),
    }
}

fn setup(
    cli: &Cli,
    start_dir: &std::path::Path,
) -> Result<(Config, provider::Registry, Arc<FileCache>), CliExitCode> {
    let mut config = load_config(cli.config.as_deref(), start_dir).ok_or(CliExitCode::UserError)?;
    match cli.cache_mode() {
        CacheMode::Bypass => config.cache.enabled = false,
        CacheMode::Default | CacheMode::Offline | CacheMode::Refresh => {}
    }
    let registry = build_registry(&config).map_err(|e| {
        eprintln!(
            "Error initializing providers: {}",
            sanitize_line(&chain(&e))
        );
        CliExitCode::EnvironmentError
    })?;
    let registry = if cli.cache_mode() == CacheMode::Offline {
        registry.map_providers(offline::Offline::wrap)
    } else {
        registry
    };
    let cache = Arc::new(FileCache::from_config(&config, cli.cache_access()));
    Ok((config, registry, cache))
}

/// Returns all compiled-in provider registrations.
///
/// Each compiled-in provider crate contributes its static [`Registration`].
/// The returned list is validated: duplicate preset hostnames across
/// providers cause a panic (programmer invariant).
///
/// GitHub is always included. The `gitlab` and `gitea` features add
/// GitLab and Gitea/Forgejo.
///
/// # Panics
///
/// Panics if two or more registrations share the same preset hostname.
#[must_use]
pub(crate) fn available_registrations() -> Vec<&'static Registration> {
    let registrations = vec![
        &actup_provider_github::REGISTRATION,
        #[cfg(feature = "gitlab")]
        &actup_provider_gitlab::REGISTRATION,
        #[cfg(feature = "gitea")]
        &actup_provider_gitea::REGISTRATION,
    ];
    validate_no_duplicate_presets(&registrations);
    registrations
}

fn build_registry(config: &Config) -> actup_core::Result<provider::Registry> {
    provider::Registry::from_config(config, &available_registrations(), &Retention::Run)
}

#[cfg(test)]
mod tests {
    use actup_core::HostKind;

    use super::*;

    /// Construct a `config::Host` with only `kind` set (no Default impl available).
    fn host_config_with_kind(kind: HostKind) -> actup_core::config::Host {
        actup_core::config::Host {
            kind: Some(kind),
            api_base: None,
            token_env: None,
            api_mode: None,
            max_tags_per_repo: None,
        }
    }

    /// Construct an empty `config::Host` (all None).
    fn host_config_empty() -> actup_core::config::Host {
        actup_core::config::Host {
            kind: None,
            api_base: None,
            token_env: None,
            api_mode: None,
            max_tags_per_repo: None,
        }
    }

    #[test]
    fn command_safe_writer_neutralizes_each_written_line() -> std::io::Result<()> {
        use std::io::Write as _;

        let mut writer = CommandSafe(Vec::new());
        let line = b" DEBUG ref_str=v1\n::error::forged\r::warning::cr ##[error]x\n";
        assert_eq!(writer.write(line)?, line.len());
        writer.flush()?;
        assert_eq!(
            String::from_utf8_lossy(&writer.0),
            " DEBUG ref_str=v1\n\u{fffd}:error::forged\r\u{fffd}:warning::cr ##\u{fffd}error]x\n"
        );
        Ok(())
    }

    #[test]
    fn available_registrations_includes_github() {
        let regs = available_registrations();
        assert!(
            regs.iter().any(|r| r.kind == HostKind::GitHub),
            "GitHub registration must always be present"
        );
    }

    #[test]
    fn available_registrations_nonempty() {
        let regs = available_registrations();
        assert!(!regs.is_empty(), "must have at least one registration");
    }

    #[test]
    fn available_registrations_no_duplicate_presets() {
        let regs = available_registrations();
        let mut seen_hosts = std::collections::HashSet::new();
        for reg in &regs {
            for preset in reg.presets {
                assert!(
                    seen_hosts.insert(preset.host),
                    "duplicate preset host: {}",
                    preset.host
                );
            }
        }
    }

    #[test]
    fn available_registrations_github_preset_host() -> Result<(), Box<dyn std::error::Error>> {
        let regs = available_registrations();
        let github = regs
            .iter()
            .find(|r| r.kind == HostKind::GitHub)
            .ok_or("no GitHub registration")?;
        assert!(
            github.presets.iter().any(|p| p.host == "github.com"),
            "GitHub preset must include github.com"
        );
        Ok(())
    }

    #[test]
    fn available_registrations_all_have_capabilities() {
        let regs = available_registrations();
        for reg in &regs {
            assert!(
                reg.capabilities.supports_graphql || reg.capabilities.supports_rest,
                "{:?} provider has no supported API modes",
                reg.kind
            );
        }
    }

    #[test]
    fn available_registrations_is_deterministic() {
        let a = available_registrations();
        let b = available_registrations();
        assert_eq!(a.len(), b.len());
        for (ra, rb) in a.iter().zip(b.iter()) {
            assert_eq!(ra.kind, rb.kind);
            assert_eq!(ra.presets.len(), rb.presets.len());
        }
    }

    #[test]
    fn available_registrations_follow_the_provider_features() {
        let kinds: Vec<HostKind> = available_registrations().iter().map(|r| r.kind).collect();
        let expected = [
            Some(HostKind::GitHub),
            cfg!(feature = "gitlab").then_some(HostKind::GitLab),
            cfg!(feature = "gitea").then_some(HostKind::Gitea),
        ];
        assert_eq!(kinds, expected.into_iter().flatten().collect::<Vec<_>>());
    }

    #[cfg(all(feature = "gitlab", feature = "gitea"))]
    #[test]
    fn build_registry_loads_multi_provider_example() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::path::Path::new("examples/multi-provider.actup.toml");
        let contents = include_str!("../../../examples/multi-provider.actup.toml");
        let config = Config::from_contents(
            Some((path, contents)),
            actup_core::config::EnvOverrides::default(),
        )?;
        let registry = build_registry(&config)?;
        assert_eq!(registry.default_host(), Some("github.com"));
        assert!(registry.find(Some("github.com")).is_some());
        assert!(registry.find(Some("github.enterprise.com")).is_some());
        let kinds = |host| registry.find(Some(host)).map(actup_core::Provider::kind);
        assert_eq!(kinds("gitlab.com"), Some(provider::Kind::GitLab));
        assert_eq!(kinds("codeberg.org"), Some(provider::Kind::Gitea));
        Ok(())
    }

    #[cfg(all(feature = "gitlab", feature = "gitea"))]
    #[test]
    fn build_registry_empty_config_registers_every_preset_host()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = build_registry(&Config::default())?;
        for (uses, kind) in [
            ("actions/checkout@v4", provider::Kind::GitHub),
            ("gitlab.com/group/project@v1", provider::Kind::GitLab),
            ("codeberg.org/o/r@v1", provider::Kind::Gitea),
        ] {
            let action = actup_core::parse_uses(uses)?.ok_or(uses)?;
            let provider = registry.find_for_repo(&action.to_repo_id());
            assert_eq!(
                provider.map(actup_core::Provider::kind),
                Some(kind),
                "{uses}"
            );
        }
        Ok(())
    }

    #[cfg(not(feature = "gitlab"))]
    #[test]
    fn build_registry_without_the_gitlab_feature_rejects_a_gitlab_host() {
        let mut config = Config::default();
        config.hosts.insert(
            "gitlab.com".to_string(),
            host_config_with_kind(HostKind::GitLab),
        );
        let error = build_registry(&config).err().map(|e| e.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("not compiled in")),
            "{error:?}"
        );
    }

    #[test]
    fn build_registry_empty_config_uses_github_preset() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::default();
        let registry = build_registry(&config)?;
        assert_eq!(
            registry.default_host(),
            Some("github.com"),
            "empty config must default to github.com"
        );
        assert!(
            registry.find(Some("github.com")).is_some(),
            "github.com provider must be registered"
        );
        Ok(())
    }

    #[test]
    fn build_registry_explicit_default_host_github() -> Result<(), Box<dyn std::error::Error>> {
        let config = Config {
            default_host: Some("github.com".to_string()),
            ..Default::default()
        };
        let registry = build_registry(&config)?;
        assert_eq!(registry.default_host(), Some("github.com"));
        assert!(registry.find(Some("github.com")).is_some());
        Ok(())
    }

    #[test]
    fn build_registry_default_host_always_registered_even_without_hosts_entry()
    -> Result<(), Box<dyn std::error::Error>> {
        // default_host = github.com, no [hosts."github.com"] entry
        let config = Config::default();
        assert!(config.hosts.is_empty());
        let registry = build_registry(&config)?;
        assert!(
            registry.find(Some("github.com")).is_some(),
            "default host must be registered even without explicit hosts entry"
        );
        Ok(())
    }

    #[test]
    fn build_registry_hosts_entry_for_default_host_no_duplicate()
    -> Result<(), Box<dyn std::error::Error>> {
        // explicit [hosts."github.com"] should not cause double-registration
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let registry = build_registry(&config)?;
        assert!(registry.find(Some("github.com")).is_some());
        // Registry allows duplicates in Vec but build_registry skips re-registering
        // default_host if already in config.hosts
        assert_eq!(
            registry
                .providers()
                .iter()
                .filter(|p| p.host() == "github.com")
                .count(),
            1,
            "github.com should be registered exactly once"
        );
        Ok(())
    }

    #[test]
    fn build_registry_ghe_host_requires_kind() -> Result<(), Box<dyn std::error::Error>> {
        // [hosts."ghe.corp.com"] without kind should error
        let mut config = Config::default();
        config
            .hosts
            .insert("ghe.corp.com".to_string(), host_config_empty());
        let result = build_registry(&config);
        assert!(result.is_err(), "GHE host without kind must error");
        let err = result.err().ok_or("expected an error")?.to_string();
        assert!(
            err.contains("kind"),
            "error must mention missing kind: {err}"
        );
        Ok(())
    }

    #[test]
    fn build_registry_ghe_host_with_github_kind() -> Result<(), Box<dyn std::error::Error>> {
        // [hosts."ghe.corp.com"] with kind = "github" should succeed
        // (api_base inferred, token_env defaults)
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let registry = build_registry(&config)?;
        assert!(
            registry.find(Some("ghe.corp.com")).is_some(),
            "GHE host with github kind must be registered"
        );
        // github.com should also be registered (default)
        assert!(registry.find(Some("github.com")).is_some());
        Ok(())
    }

    #[test]
    fn build_registry_invalid_hostname_errors() -> Result<(), Box<dyn std::error::Error>> {
        let mut config = Config::default();
        config.hosts.insert(
            "UPPERCASE.COM".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let result = build_registry(&config);
        assert!(result.is_err(), "uppercase hostname must error");
        let err = result.err().ok_or("expected an error")?.to_string();
        assert!(
            err.contains("lowercase") || err.contains("case"),
            "error must mention case: {err}"
        );
        Ok(())
    }

    #[test]
    fn build_registry_invalid_default_host_errors() {
        let config = Config {
            default_host: Some("INVALID HOST".to_string()),
            ..Default::default()
        };
        let result = build_registry(&config);
        assert!(
            result.is_err(),
            "invalid default_host must error: {result:?}"
        );
    }

    #[test]
    fn build_registry_invalid_api_base_in_hosts_errors() -> Result<(), Box<dyn std::error::Error>> {
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".to_string(),
            actup_core::config::Host {
                kind: Some(HostKind::GitHub),
                api_base: Some("no-scheme.example.com/api".to_string()),
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );
        let result = build_registry(&config);
        assert!(result.is_err(), "api_base without scheme must error");
        let err = result.err().ok_or("expected an error")?.to_string();
        assert!(
            err.contains("scheme") || err.contains("http"),
            "error must mention scheme: {err}"
        );
        Ok(())
    }

    #[test]
    fn build_registry_fetch_settings_propagated() -> Result<(), Box<dyn std::error::Error>> {
        use std::num::NonZeroU8;

        use actup_core::{ApiMode, FetchSettings};

        let config = Config {
            fetch: FetchSettings {
                api_mode: ApiMode::Rest,
                max_tags_per_repo: NonZeroU8::new(50).ok_or("zero tag limit")?,
            },
            ..Default::default()
        };
        // Should succeed — github provider supports both graphql and rest
        let registry = build_registry(&config)?;
        assert!(registry.find(Some("github.com")).is_some());
        Ok(())
    }

    #[test]
    fn build_registry_multiple_hosts() -> Result<(), Box<dyn std::error::Error>> {
        let mut config = Config::default();
        config.hosts.insert(
            "ghe1.corp.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        config.hosts.insert(
            "ghe2.corp.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let registry = build_registry(&config)?;
        assert!(registry.find(Some("ghe1.corp.com")).is_some());
        assert!(registry.find(Some("ghe2.corp.com")).is_some());
        assert!(registry.find(Some("github.com")).is_some());
        Ok(())
    }
}
