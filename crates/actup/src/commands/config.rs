//! Config command - show effective configuration

use std::path::Path;

use actup_core::{Config, iter_config_candidates};
use owo_colors::OwoColorize;

use crate::cli::{CliExitCode, OutputFormat};

pub fn run(path: &Path, format: OutputFormat, config: &Config) -> i32 {
    match format {
        OutputFormat::Human => {
            display_human(config, path);
        }
        OutputFormat::Json => {
            let json = match serde_json::to_string_pretty(config) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Error serializing config: {e}");
                    return CliExitCode::EnvironmentError.into();
                }
            };
            println!("{json}");
        }
        OutputFormat::Toml => {
            let toml = match toml::to_string_pretty(config) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error serializing config: {e}");
                    return CliExitCode::EnvironmentError.into();
                }
            };
            println!("{toml}");
        }
    }

    CliExitCode::Success.into()
}

fn display_human(config: &Config, path: &Path) {
    println!("{}", "Configuration".bold());
    println!();

    println!("{}:", "Update Policy".bold());
    println!("  update_policy:     {}", config.update_policy);
    println!("  ref_style:         {}", config.ref_style);
    println!("  branch_policy:     {}", config.branch_policy);
    println!("  allow_prerelease:  {}", config.allow_prerelease);
    println!();

    println!("{}:", "Cache".bold());
    println!(
        "  ttl:      {} seconds ({} hours)",
        config.cache.ttl.as_secs(),
        config.cache.ttl.as_secs() / 3600
    );
    println!("  enabled:  {}", config.cache.enabled);
    if let Some(path) = &config.cache.path {
        println!("  path:     {}", path.display());
    }
    println!();

    println!("{}:", "Providers".bold());
    println!("  default_kind: {}", config.providers.default_kind);
    println!("  default_host: {}", config.providers.default_host);
    println!("  api_mode:     {}", config.providers.api_mode);
    println!("  max_tags:     {}", config.providers.max_tags_per_repo);
    if !config.providers.instances.is_empty() {
        println!(
            "  instances:    {} configured",
            config.providers.instances.len()
        );
    }
    println!();

    if !config.ignore.is_empty() {
        println!("{}:", "Ignored Actions".bold());
        for action in &config.ignore {
            println!("  - {action}");
        }
        println!();
    }

    if !config.action_overrides.is_empty() {
        println!("{}:", "Action Overrides".bold());
        println!(
            "  {} actions with custom settings",
            config.action_overrides.len()
        );
        println!();
    }

    if !config.extra_paths.is_empty() {
        println!("{}:", "Extra Paths".bold());
        for p in &config.extra_paths {
            println!("  - {p}");
        }
        println!();
    }

    // Show source
    println!("{}", "Source".dimmed());
    if let Some(loaded_from) = &config.loaded_from {
        println!("  Loaded from: {}", loaded_from.display());
    } else {
        let found: Vec<_> = iter_config_candidates(path)
            .filter(|p| p.exists())
            .collect();

        if found.is_empty() {
            println!("  Using defaults (no config file found)");
        } else {
            println!("  Config file(s) found but not loaded:");
            for p in found {
                println!("    {}", p.display());
            }
        }
    }
}
