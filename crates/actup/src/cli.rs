//! CLI argument parsing with clap

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use actup_core::config;
use clap::{
    CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    error::{Error, ErrorKind},
    parser::ValueSource,
};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "actup")]
#[command(author, version, about = "GitHub Actions version manager")]
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

    #[command(flatten)]
    pub latest: LatestArgs,
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
    #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
    pub format: OutputFormat,
}

impl Cli {
    /// Parse `args` and reject top-level arguments given on the command line next to a subcommand.
    pub fn try_parse_args<I, T>(args: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut command = Self::command();
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
        Self::from_arg_matches_mut(&mut matches)
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

        /// Fail if any actions are outdated
        #[arg(long, env = config::STRICT_VAR)]
        strict: bool,

        /// Output format
        #[arg(long, default_value = "human", env = config::FORMAT_VAR)]
        format: OutputFormat,
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

/// Exit codes per AGENTS.md specification
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum CliExitCode {
    /// Success
    Success = 0,
    /// Outdated found (in check with --strict, or dry-run with changes), or `update`/`pin` skipped a ref
    Outdated = 1,
    /// User error: a confirmed unresolvable ref, an invalid config file, or invalid CLI input.
    UserError = 2,
    /// Network/auth/rate-limit prevented verification (environment error)
    EnvironmentError = 3,
}

impl From<CliExitCode> for i32 {
    fn from(code: CliExitCode) -> Self {
        code as Self
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
                format: OutputFormat::Json,
                ..
            })
        ));
        Ok(())
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
