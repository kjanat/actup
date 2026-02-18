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
    let ttl_secs = config.cache.ttl.as_secs();
    if ttl_secs < 3600 {
        println!(
            "  ttl:      {ttl_secs} seconds ({:.1} minutes)",
            config.cache.ttl.as_secs_f64() / 60.0
        );
    } else {
        println!(
            "  ttl:      {ttl_secs} seconds ({:.1} hours)",
            config.cache.ttl.as_secs_f64() / 3600.0
        );
    }
    println!("  enabled:  {}", config.cache.enabled);
    if let Some(path) = &config.cache.path {
        println!("  path:     {}", path.display());
    }
    println!();

    println!("{}:", "Hosts".bold());
    let default_host = config
        .default_host
        .as_deref()
        .unwrap_or("(github.com via preset)");
    println!("  default_host: {default_host}");
    println!();

    println!("{}:", "Fetch".bold());
    println!("  api_mode:     {}", config.fetch.api_mode);
    println!("  max_tags:     {}", config.fetch.max_tags_per_repo);
    if !config.hosts.is_empty() {
        println!();
        println!("  {} host(s) configured:", config.hosts.len());
        for (host, hc) in &config.hosts {
            let kind = hc
                .kind
                .map_or_else(|| "(inherited)".into(), |k| k.to_string());
            println!("    {host}: kind={kind}");
        }
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
            println!(
                "  Config file(s) found but not loaded (using CLI defaults and/or explicit overrides):"
            );
            for p in found {
                println!("    {}", p.display());
            }
        }
    }
}
