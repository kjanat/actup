//! CLI argument parsing with clap

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use actup_core::config;
use clap::{
    ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    error::{Error, ErrorKind},
    parser::ValueSource,
};
use clap_complete::Shell;

use actup_core::Access;

#[derive(Parser)]
#[command(name = "actup")]
#[command(author, version, about = "GitHub Actions version manager", long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Verbose output
    #[arg(short, long, global = true, env = config::VERBOSE_VAR)]
    pub verbose: bool,

    /// Config file to load over the repo and user config
    #[arg(
        short,
        long,
        global = true,
        env = config::CONFIG_VAR,
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,

    /// When to colour output
    #[arg(
        long,
        global = true,
        env = config::COLOR_VAR,
        value_name = "WHEN",
        default_value = "auto"
    )]
    pub color: ColorChoice,

    #[command(flatten)]
    cache: CacheArgs,

    #[command(flatten)]
    pub latest: LatestArgs,
}

#[derive(clap::Args)]
struct CacheArgs {
    /// Use only cached version data; a ref missing from the cache is an environment error
    #[arg(long, global = true)]
    offline: bool,

    /// Neither read nor write the version cache
    #[arg(long, global = true)]
    no_cache: bool,

    /// Skip cached versions and write the freshly fetched ones
    #[arg(long, global = true)]
    refresh: bool,
}

/// How a run uses the version cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Read fresh entries, fetch the rest and write them.
    Default,
    /// Read entries only and never fetch.
    Offline,
    /// Neither read nor write entries.
    Bypass,
    /// Fetch every repo and write the results.
    Refresh,
}

/// Arguments for the default `actup <owner/repo[@ref]>` quick lookup.
///
/// When no subcommand is provided, actup treats the positional argument as
/// an action reference and prints the latest version.
#[derive(clap::Args)]
pub struct LatestArgs {
    /// Action to look up, e.g. `actions/checkout` or `actions/checkout@v4`
    pub action: Option<String>,

    /// Output format
    #[arg(long, default_value = "human")]
    pub format: OutputFormat,
}

impl Cli {
    /// Parse `args` and reject top-level arguments given on the command line next to a subcommand, and more than one of `--offline`, `--no-cache` and `--refresh`.
    ///
    /// An empty `ACTUP_*` flag variable counts as unset.
    pub fn try_parse_args<I, T>(args: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let mut command = config::ignore_empty_env(Self::command()).color(Self::color_in(&args));
        let mut matches = command.try_get_matches_from_mut(args)?;
        if let Some(subcommand) = matches.subcommand_name() {
            let conflicts: Vec<String> = command
                .get_arguments()
                .filter(|arg| !arg.is_global_set())
                .filter(|arg| {
                    matches.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine)
                })
                .map(ToString::to_string)
                .collect();
            if !conflicts.is_empty() {
                let message = format!(
                    "the subcommand '{subcommand}' cannot be used with: {}",
                    conflicts.join(", ")
                );
                return Err(command.error(ErrorKind::ArgumentConflict, message));
            }
        }
        let cli = Self::from_arg_matches_mut(&mut matches)?;
        let CacheArgs {
            offline,
            no_cache,
            refresh,
        } = cli.cache;
        let given: Vec<&str> = [
            (offline, "--offline"),
            (no_cache, "--no-cache"),
            (refresh, "--refresh"),
        ]
        .into_iter()
        .filter_map(|(set, flag)| set.then_some(flag))
        .collect();
        if let [first, second, ..] = given[..] {
            return Err(command.error(
                ErrorKind::ArgumentConflict,
                format!("{first} cannot be used with {second}"),
            ));
        }
        Ok(cli)
    }

    /// The file cache access that `--no-cache` or `--refresh` selects.
    #[must_use]
    pub const fn cache_access(&self) -> Access {
        match self.cache_mode() {
            CacheMode::Bypass => Access::Off,
            CacheMode::Refresh => Access::WriteOnly,
            CacheMode::Default | CacheMode::Offline => Access::ReadWrite,
        }
    }

    /// The cache mode that `--offline`, `--no-cache` or `--refresh` selects.
    #[must_use]
    pub const fn cache_mode(&self) -> CacheMode {
        match (self.cache.offline, self.cache.no_cache, self.cache.refresh) {
            (true, _, _) => CacheMode::Offline,
            (_, true, _) => CacheMode::Bypass,
            (_, _, true) => CacheMode::Refresh,
            _ => CacheMode::Default,
        }
    }

    fn color_in(args: &[OsString]) -> ColorChoice {
        let mut given = None;
        let mut rest = args.iter().skip(1).map(|arg| arg.to_str());
        while let Some(arg) = rest.next() {
            match arg {
                Some("--") => break,
                Some("--color") => given = rest.next().flatten().or(given),
                Some(arg) => given = arg.strip_prefix("--color=").or(given),
                None => {}
            }
        }
        let env = std::env::var(config::COLOR_VAR).ok();
        given
            .or(env.as_deref())
            .and_then(|value| ColorChoice::from_str(value, true).ok())
            .unwrap_or_default()
    }

    /// The colour choice `--color` selects for stdout and stderr.
    #[must_use]
    pub const fn color_choice(&self) -> anstream::ColorChoice {
        match self.color {
            ColorChoice::Auto => anstream::ColorChoice::Auto,
            ColorChoice::Always => anstream::ColorChoice::Always,
            ColorChoice::Never => anstream::ColorChoice::Never,
        }
    }

    /// Generate shell completions and write to stdout
    pub fn print_completions(shell: Shell) {
        clap_complete::generate(shell, &mut Self::command(), "actup", &mut std::io::stdout());
    }

    /// Detect shell from SHELL environment variable
    #[must_use]
    pub fn detect_shell() -> Option<Shell> {
        let shell_path = std::env::var("SHELL").ok()?;
        let shell_name = Path::new(&shell_path).file_name()?.to_str()?;

        match shell_name {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "elvish" => Some(Shell::Elvish),
            "pwsh" | "powershell" => Some(Shell::PowerShell),
            _ => None,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Scan workflows and report outdated actions (non-destructive)
    Check {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Fail if any actions are outdated, the same as `--fail-on outdated`
        #[arg(long, env = config::STRICT_VAR)]
        strict: bool,

        /// Exit 1 when a finding of this kind exists (repeatable)
        #[arg(long, value_name = "KIND")]
        fail_on: Vec<FailOn>,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: CheckFormat,

        /// Also write a GitHub Actions `::error`/`::warning` command per finding to stderr
        #[arg(long)]
        github_annotations: bool,
    },

    /// Update actions to their latest versions
    Update {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Dry run - show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: OutputFormat,
    },

    /// Alias for update
    Fix {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Dry run - show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: OutputFormat,
    },

    /// Pin all actions to their current SHA
    Pin {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Rewrite a SHA pin to the tag its `# <tag>` comment names, only when that tag is on the pinned commit
        #[arg(long)]
        unpin: bool,

        /// Dry run - show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: OutputFormat,
    },

    /// Show effective configuration
    Config {
        /// Path to check for config files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: OutputFormat,
    },

    /// Output JSON schema for configuration
    Schema,

    /// Manage the version cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: OutputFormat,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for (auto-detected from $SHELL if omitted)
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
}

/// Cache subcommands
#[derive(Subcommand)]
pub enum CacheAction {
    /// Show cache location and statistics
    Info,

    /// Clear all cached data
    Clear,

    /// Print cache directory path
    Path,
}

#[derive(Clone, Copy, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
    Toml,
}

/// Output format of `check`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum CheckFormat {
    /// Findings grouped by file, with a summary.
    #[default]
    Human,
    /// One JSON entry per finding.
    Json,
    /// One TOML entry per finding.
    Toml,
    /// One aligned row per finding: location, action, current, target, status.
    Table,
    /// A SARIF 2.1.0 log for code scanning.
    Sarif,
}

impl CheckFormat {
    /// The format the shared scanner messages follow.
    #[must_use]
    pub const fn messages(self) -> OutputFormat {
        match self {
            Self::Human | Self::Table => OutputFormat::Human,
            Self::Json | Self::Sarif => OutputFormat::Json,
            Self::Toml => OutputFormat::Toml,
        }
    }
}

/// A finding kind that makes `check` exit 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FailOn {
    /// An outdated ref or a branch ref to pin.
    Outdated,
    /// An outdated ref whose target is a newer major version.
    Major,
    /// A floating branch ref, warned about or to pin.
    Floating,
}

/// Exit codes per AGENTS.md specification
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CliExitCode {
    /// Success
    Success = 0,
    /// A `check --fail-on` kind found, `update --dry-run` with changes, or `update`/`pin` skipped a ref it cannot edit
    Outdated = 1,
    /// User error: a confirmed unresolvable ref, an invalid config file, or invalid CLI input.
    UserError = 2,
    /// Network/auth/rate-limit prevented verification (environment error)
    EnvironmentError = 3,
}

impl From<CliExitCode> for std::process::ExitCode {
    fn from(code: CliExitCode) -> Self {
        Self::from(code as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, Error> {
        Cli::try_parse_args(std::iter::once("actup").chain(args.iter().copied()))
    }

    fn env_vars(command: &clap::Command, out: &mut Vec<String>) {
        out.extend(
            command
                .get_arguments()
                .filter_map(clap::Arg::get_env)
                .map(|name| name.to_string_lossy().into_owned()),
        );
        for subcommand in command.get_subcommands() {
            env_vars(subcommand, out);
        }
    }

    #[test]
    fn every_env_var_is_a_cli_flag_var() {
        let mut vars = Vec::new();
        env_vars(&Cli::command(), &mut vars);
        vars.sort_unstable();
        vars.dedup();
        let mut expected = config::CLI_FLAG_VARS.map(str::to_owned).to_vec();
        expected.sort_unstable();
        assert_eq!(vars, expected);
    }

    #[test]
    fn global_flags_parse_before_and_after_the_subcommand() -> Result<(), Error> {
        for args in [
            &["-v", "--config", "x.toml", "check"][..],
            &["check", "-v", "-c", "x.toml"][..],
        ] {
            let cli = parse(args)?;
            assert!(
                matches!(cli.command, Some(Command::Check { .. })),
                "{args:?}"
            );
            assert!(cli.verbose, "{args:?}");
            assert_eq!(cli.config.as_deref(), Some(Path::new("x.toml")), "{args:?}");
        }
        Ok(())
    }

    #[test]
    fn cache_flags_parse_globally_and_exclude_each_other() -> Result<(), Error> {
        for (args, mode) in [
            (&["check"][..], CacheMode::Default),
            (&["check", "--offline"][..], CacheMode::Offline),
            (&["--no-cache", "update"][..], CacheMode::Bypass),
            (&["pin", "--refresh"][..], CacheMode::Refresh),
        ] {
            assert_eq!(parse(args)?.cache_mode(), mode, "{args:?}");
        }
        for args in [
            &["--offline", "check", "--no-cache"][..],
            &["--offline", "--no-cache", "check"][..],
            &["--refresh", "update", "--offline"][..],
            &["--refresh", "--no-cache", "pin"][..],
        ] {
            let Err(error) = parse(args) else {
                panic!("{args:?} must be rejected");
            };
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{args:?}");
        }
        Ok(())
    }

    #[test]
    fn quick_lookup_parses_without_a_subcommand() -> Result<(), Error> {
        let cli = parse(&["actions/checkout", "--format", "json", "-c", "x.toml"])?;
        assert!(cli.command.is_none());
        assert_eq!(cli.latest.action.as_deref(), Some("actions/checkout"));
        assert!(matches!(cli.latest.format, OutputFormat::Json));
        assert_eq!(cli.config.as_deref(), Some(Path::new("x.toml")));
        Ok(())
    }

    #[test]
    fn subcommand_format_is_not_a_conflict() -> Result<(), Error> {
        let cli = parse(&["check", "--format", "json"])?;
        assert!(matches!(
            cli.command,
            Some(Command::Check {
                format: CheckFormat::Json,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn check_takes_repeated_fail_on_and_the_table_format() -> Result<(), Error> {
        let cli = parse(&[
            "check",
            "--fail-on",
            "major",
            "--fail-on",
            "floating",
            "--format",
            "table",
        ])?;
        let Some(Command::Check {
            fail_on, format, ..
        }) = cli.command
        else {
            panic!("expected the check command");
        };
        assert_eq!(fail_on, [FailOn::Major, FailOn::Floating]);
        assert_eq!(format, CheckFormat::Table);
        assert!(matches!(CheckFormat::Table.messages(), OutputFormat::Human));
        Ok(())
    }

    #[test]
    fn only_check_accepts_the_table_format() {
        for args in [
            &["update", "--format", "table"][..],
            &["config", "--format", "table"][..],
            &["--format", "table", "actions/checkout"][..],
            &["pin", "--format", "sarif"][..],
            &["cache", "--format", "sarif", "info"][..],
        ] {
            let Err(error) = parse(args) else {
                panic!("{args:?} must be rejected");
            };
            assert_eq!(error.kind(), ErrorKind::InvalidValue, "{args:?}");
        }
    }

    #[test]
    fn quick_lookup_arguments_conflict_with_a_subcommand() {
        for (args, named) in [
            (&["actions/checkout", "check"][..], "[ACTION]"),
            (&["--format", "json", "check"][..], "--format <FORMAT>"),
        ] {
            let Err(error) = parse(args) else {
                panic!("{args:?} must be rejected");
            };
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{args:?}");
            let message = error.to_string();
            assert!(
                message.contains(&format!(
                    "the subcommand 'check' cannot be used with: {named}"
                )),
                "{args:?}: {message}"
            );
        }
    }
}
