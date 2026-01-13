//! CLI argument parsing with clap

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "actup")]
#[command(author, version, about = "GitHub Actions version manager")]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Output format
    #[arg(long, global = true, default_value = "human")]
    pub format: OutputFormat,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scan workflows and report outdated actions (non-destructive)
    Check {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Fail if any actions are outdated
        #[arg(long)]
        strict: bool,
    },

    /// Update actions to their latest versions
    Update {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Dry run - show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,
    },

    /// Alias for update
    Fix {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Dry run - show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,
    },

    /// Pin all actions to their current SHA
    Pin {
        /// Path to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Dry run - show what would be changed without modifying files
        #[arg(long)]
        dry_run: bool,
    },

    /// Show effective configuration
    Config {
        /// Path to check for config files
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Output JSON schema for configuration
    Schema,
}

#[derive(Clone, Copy, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

/// Exit codes per AGENTS.md specification
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum CliExitCode {
    /// Success
    Success = 0,
    /// Outdated found (in check with --strict, or dry-run with changes)
    Outdated = 1,
    /// Confirmed unresolvable refs (user error - bad ref, not found, etc.)
    Unresolvable = 2,
    /// Network/auth/rate-limit prevented verification (environment error)
    EnvironmentError = 3,
}

impl From<CliExitCode> for i32 {
    fn from(code: CliExitCode) -> Self {
        code as Self
    }
}
