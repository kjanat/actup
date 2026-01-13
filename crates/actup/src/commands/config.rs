//! Config command - show effective configuration

use std::path::Path;

use actup_core::{Config, iter_config_candidates};
use owo_colors::OwoColorize;

use crate::cli::{CliExitCode, OutputFormat};

pub fn run(path: &Path, format: OutputFormat, config: &Config) -> i32 {
    let json = match serde_json::to_string_pretty(config) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("Error serializing config: {e}");
            return CliExitCode::EnvironmentError.into();
        }
    };

    match format {
        OutputFormat::Human => {
            println!("{}", "Effective configuration:".bold());
            println!();
            println!("{json}");
        }
        OutputFormat::Json => {
            println!("{json}");
        }
    }

    // Show where config was loaded from
    if matches!(format, OutputFormat::Human) {
        println!();

        // Use loaded_from if available, otherwise check standard paths
        if let Some(loaded_from) = &config.loaded_from {
            println!("{}", "Config loaded from:".dimmed());
            println!("  {}", loaded_from.display());
        } else {
            // Use shared discovery to find existing configs
            let found: Vec<_> = iter_config_candidates(path)
                .filter(|p| p.exists())
                .collect();

            if found.is_empty() {
                println!("{}", "No config file found, using defaults.".dimmed());
            } else {
                println!("{}", "Config file(s) found:".dimmed());
                for p in found {
                    println!("  {}", p.display());
                }
            }
        }
    }

    CliExitCode::Success.into()
}
