//! Config command - show effective configuration

use anstream::{eprintln, println};
use std::path::Path;

use actup_core::{
    Config, ConfigSource, Registration, effective_host, iter_config_candidates,
    resolve_default_host, sanitize_line,
};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::{
    cli::{CliExitCode, OutputFormat},
    common::in_unit,
};

#[derive(Serialize)]
struct Report<'a> {
    #[serde(flatten)]
    config: &'a Config,
    sources: &'a [ConfigSource],
}

impl<'a> Report<'a> {
    fn new(config: &'a Config) -> Self {
        Self {
            config,
            sources: &config.sources,
        }
    }
}

pub fn run(
    path: &Path,
    format: OutputFormat,
    config: &Config,
    registrations: &[&'static Registration],
) -> CliExitCode {
    match format {
        OutputFormat::Human => {
            display_human(config, path, registrations);
        }
        OutputFormat::Json => {
            let json = match crate::structured::json_pretty(&Report::new(config)) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Error serializing config: {e}");
                    return CliExitCode::EnvironmentError;
                }
            };
            println!("{json}");
        }
        OutputFormat::Toml => {
            let toml = match crate::structured::toml(&Report::new(config)) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error serializing config: {e}");
                    return CliExitCode::EnvironmentError;
                }
            };
            println!("{toml}");
        }
    }

    CliExitCode::Success
}

fn display_human(config: &Config, path: &Path, registrations: &[&'static Registration]) {
    println!("{}", "Configuration".bold());
    println!();

    println!("{}:", "Policy".bold());
    println!("  bump:        {}", config.policy.bump);
    println!("  pin:         {}", config.policy.pin);
    println!("  track:       {}", config.policy.track);
    println!("  branch:      {}", config.policy.branch);
    println!("  prerelease:  {}", config.policy.prerelease);
    println!("  archived:    {}", config.policy.archived);
    println!();

    println!("{}:", "Pin formatting".bold());
    println!("  comment:     {}", config.pin.comment);
    println!();

    println!("{}:", "Cache".bold());
    println!("  ttl:      {}", ttl_text(config.cache.ttl));
    println!("  enabled:  {}", config.cache.enabled);
    if let Some(path) = &config.cache.path {
        println!("  path:     {}", sanitize_line(&path.display().to_string()));
    }
    println!();

    println!("{}:", "Fetch".bold());
    println!("  api_mode:          {}", config.fetch.api_mode);
    println!("  max_tags_per_repo: {}", config.fetch.max_tags_per_repo);
    println!();

    display_hosts(config, registrations);

    if !config.scan.ignore.is_empty() {
        println!("{}:", "Ignored Actions".bold());
        for action in &config.scan.ignore {
            println!("  - {}", sanitize_line(&action.to_string()));
        }
        println!();
    }

    if !config.overrides.is_empty() {
        println!("{}:", "Overrides".bold());
        for o in &config.overrides {
            println!("  - {}", sanitize_line(&o.pattern.to_string()));
        }
        println!();
    }

    if !config.scan.extra_paths.is_empty() {
        println!("{}:", "Extra Paths".bold());
        for p in &config.scan.extra_paths {
            println!("  - {}", sanitize_line(p.as_str()));
        }
        println!();
    }

    println!("{}", "Sources (lowest to highest precedence)".dimmed());
    if config.sources.is_empty() {
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
                println!("    {}", sanitize_line(&p.display().to_string()));
            }
        }
    }
    for source in &config.sources {
        println!("  - {}", sanitize_line(&source.to_string()));
    }
}

/// Display hosts section with resolved effective configuration.
///
/// Shows the default host and then each host's resolved config,
/// distinguishing preset vs user-specified values.
fn display_hosts(config: &Config, registrations: &[&'static Registration]) {
    println!("{}:", "Hosts".bold());

    // Default host
    let default_host = match resolve_default_host(config, registrations) {
        Ok(host) => host,
        Err(e) => {
            println!(
                "  default_host: {} {}",
                "(error)".red(),
                sanitize_line(&e.to_string())
            );
            println!();
            return;
        }
    };

    let explicit_default = config.default_host.is_some();
    let shown = sanitize_line(&default_host);
    if explicit_default {
        println!("  default_host: {shown}");
    } else {
        println!("  default_host: {shown} {}", "(via preset)".dimmed());
    }
    println!();

    // Collect all hosts to display: explicit hosts + default host if not already listed
    let mut hosts_to_show: Vec<&str> = config.hosts.keys().map(String::as_str).collect();
    hosts_to_show.sort_unstable();

    let default_in_hosts = config.hosts.contains_key(default_host.as_str());
    if !default_in_hosts {
        // Show default host first even though it's not in [hosts]
        display_effective_host(&default_host, config, registrations, true);
    }

    for hostname in &hosts_to_show {
        let is_default = *hostname == default_host;
        display_effective_host(hostname, config, registrations, is_default);
    }
}

/// Display the resolved effective config for a single host.
fn display_effective_host(
    hostname: &str,
    config: &Config,
    registrations: &[&'static Registration],
    is_default: bool,
) {
    let suffix = if is_default {
        " (default)".dimmed().to_string()
    } else {
        String::new()
    };

    // Check if this is a preset host
    let is_preset = registrations
        .iter()
        .any(|r| r.presets.iter().any(|p| p.host == hostname));

    let has_user_entry = config.hosts.contains_key(hostname);

    println!("  {}{}:", sanitize_line(hostname).bold(), suffix);

    match effective_host(hostname, config, registrations, &config.fetch) {
        Ok(resolved) => {
            let user_config = config.hosts.get(hostname);

            // kind
            let kind_source = source_label(is_preset, user_config.and_then(|hc| hc.kind).is_some());
            println!("    kind:      {} {}", resolved.kind, kind_source);

            // api_base
            let api_base_source = source_label(
                is_preset,
                user_config.and_then(|hc| hc.api_base.as_ref()).is_some(),
            );
            let api_base_label =
                if !is_preset && user_config.and_then(|hc| hc.api_base.as_ref()).is_none() {
                    "(inferred)".dimmed().to_string()
                } else {
                    api_base_source
                };
            println!(
                "    api_base:  {} {}",
                sanitize_line(&resolved.api_base),
                api_base_label
            );

            // token_env
            let token_env_source = source_label(
                is_preset,
                user_config.and_then(|hc| hc.token_env.as_ref()).is_some(),
            );
            match &resolved.token_env {
                Some(token_env) => println!(
                    "    token_env: {} {token_env_source}",
                    sanitize_line(token_env)
                ),
                None => println!(
                    "    token_env: {} {}",
                    "(none)".dimmed(),
                    "(repo-only host)".dimmed()
                ),
            }

            // api_mode
            let api_mode_source = if user_config.and_then(|hc| hc.api_mode).is_some() {
                "(host override)".dimmed().to_string()
            } else {
                "(from [fetch])".dimmed().to_string()
            };
            println!("    api_mode:  {} {}", resolved.api_mode, api_mode_source);

            // max_tags_per_repo
            let max_tags_source = if user_config.and_then(|hc| hc.max_tags_per_repo).is_some() {
                "(host override)".dimmed().to_string()
            } else {
                "(from [fetch])".dimmed().to_string()
            };
            println!(
                "    max_tags:  {} {}",
                resolved.max_tags_per_repo, max_tags_source
            );
        }
        Err(e) => {
            let entry_note = if has_user_entry {
                ""
            } else {
                " (no config entry)"
            };
            println!(
                "    {} {}{}",
                "error:".red(),
                sanitize_line(&e.to_string()),
                entry_note
            );
        }
    }
    println!();
}

/// Produce a source annotation for a resolved field value.
fn source_label(is_preset: bool, has_user_override: bool) -> String {
    if has_user_override {
        "(user override)".dimmed().to_string()
    } else if is_preset {
        "(preset)".dimmed().to_string()
    } else {
        "(default)".dimmed().to_string()
    }
}

fn ttl_text(ttl: std::time::Duration) -> String {
    let secs = ttl.as_secs();
    format!(
        "{secs} seconds ({})",
        in_unit(secs, &[(60, "minutes")], (3600, "hours"))
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn ttl_text_picks_the_unit_after_rounding() {
        let text = |secs| ttl_text(Duration::from_secs(secs));
        assert_eq!(text(0), "0 seconds (0.0 minutes)");
        assert_eq!(text(3596), "3596 seconds (59.9 minutes)");
        assert_eq!(text(3597), "3597 seconds (1.0 hours)");
        assert_eq!(text(3598), "3598 seconds (1.0 hours)");
        assert_eq!(text(3599), "3599 seconds (1.0 hours)");
        assert_eq!(text(3600), "3600 seconds (1.0 hours)");
        assert_eq!(text(86_400), "86400 seconds (24.0 hours)");
    }
}
