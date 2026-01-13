//! Config command - show effective configuration

use std::path::Path;

use actup_core::Config;
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;

pub fn run(path: &Path, format: OutputFormat, config: &Config) -> i32 {
    match format {
        OutputFormat::Human => {
            println!("{}", "Effective configuration:".bold());
            println!();
            println!(
                "{}",
                serde_json::to_string_pretty(config).expect("failed to serialize config")
            );
        }
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(config).expect("failed to serialize config")
            );
        }
    }

    // Show where config was loaded from
    if matches!(format, OutputFormat::Human) {
        println!();
        let config_paths = [
            path.join(".actup.toml"),
            path.join(".actup.yaml"),
            path.join(".actup.yml"),
            path.join(".actup.json"),
        ];

        let found: Vec<_> = config_paths.iter().filter(|p| p.exists()).collect();

        if found.is_empty() {
            println!("{}", "No config file found, using defaults.".dimmed());
        } else {
            println!("{}", "Config loaded from:".dimmed());
            for p in found {
                println!("  {}", p.display());
            }
        }
    }

    0
}
