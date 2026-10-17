//! CLI argument parsing with clap

use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "actup")]
#[command(author, version, about = "GitHub Actions version manager")]
#[command(propagate_version = true)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Verbose output
    #[arg(short, long, global = true, env = "ACTUP_VERBOSE")]
    pub verbose: bool,

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
    #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
    pub format: OutputFormat,
}

impl Cli {
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
        #[arg(long, env = "ACTUP_STRICT")]
        strict: bool,

        /// Output format
        #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
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
        #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
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
        #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
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
        #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
        format: OutputFormat,
    },

    /// Show effective configuration
    Config {
        /// Path to check for config files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output format
        #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
        format: OutputFormat,
    },

    /// Output JSON schema for configuration
    Schema,

    /// Manage the version cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,

        /// Output format
        #[arg(long, default_value = "human", env = "ACTUP_FORMAT")]
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
