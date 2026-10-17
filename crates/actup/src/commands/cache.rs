//! Cache command - manage the version cache

use anstream::{eprintln, println};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use actup_core::{
    Config, FileCache,
    cache::{self, Cache},
    sanitize_line,
};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::cli::{CacheAction, CliExitCode, OutputFormat};

/// Cache info for JSON output
#[derive(Serialize)]
struct CacheInfo {
    path: Option<String>,
    enabled: bool,
    ttl_secs: u64,
    entry_count: usize,
    total_size_bytes: u64,
}

/// Output for cache clear when the cache is disabled or has no directory
#[derive(Serialize)]
struct DisabledOutput {
    cleared: u64,
    disabled: bool,
    path: Option<String>,
}

/// Output for cache clear operations
#[derive(Serialize)]
struct ClearedOutput {
    cleared: u64,
}

/// Output for cache path command
#[derive(Serialize)]
struct PathOutput {
    path: Option<String>,
}

/// Run the cache command
pub async fn run(action: &CacheAction, format: OutputFormat, config: &Config) -> CliExitCode {
    let dir = cache::dir(config.cache.path.as_deref());
    match action {
        CacheAction::Info => info(format, config, dir.as_deref()),
        CacheAction::Clear => clear(format, config, dir).await,
        CacheAction::Path => path(format, dir.as_deref()),
    }
}

fn dir_text(dir: Option<&Path>) -> String {
    dir.map_or_else(
        || format!("none ({})", cache::NO_DIR_WARNING),
        |dir| sanitize_line(&dir.display().to_string()).into_owned(),
    )
}

/// Show cache info
fn info(format: OutputFormat, config: &Config, cache_dir: Option<&Path>) -> CliExitCode {
    let (entry_count, total_size) = match cache_dir.map_or(Ok((0, 0)), get_cache_stats) {
        Ok(stats) => stats,
        Err(e) => return report_failure("Error reading cache directory", &e),
    };

    match format {
        OutputFormat::Human => {
            println!("{}", "Cache Information".bold());
            println!();
            println!("  {:<12} {}", "Path:".dimmed(), dir_text(cache_dir));
            println!(
                "  {:<12} {}",
                "Enabled:".dimmed(),
                if config.cache.enabled {
                    "yes".green().to_string()
                } else {
                    "no".red().to_string()
                }
            );
            println!(
                "  {:<12} {}",
                "TTL:".dimmed(),
                format_duration(config.cache.ttl)
            );
            println!("  {:<12} {}", "Entries:".dimmed(), entry_count);
            println!("  {:<12} {}", "Size:".dimmed(), format_size(total_size));
        }
        OutputFormat::Json => {
            let info = CacheInfo {
                path: cache_dir.map(|dir| dir.to_string_lossy().into_owned()),
                enabled: config.cache.enabled,
                ttl_secs: config.cache.ttl.as_secs(),
                entry_count,
                total_size_bytes: total_size,
            };
            match crate::structured::json_pretty(&info) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("{}: {}", "Error serializing output".red(), e);
                    return CliExitCode::EnvironmentError;
                }
            }
        }
        OutputFormat::Toml => {
            let info = CacheInfo {
                path: cache_dir.map(|dir| dir.to_string_lossy().into_owned()),
                enabled: config.cache.enabled,
                ttl_secs: config.cache.ttl.as_secs(),
                entry_count,
                total_size_bytes: total_size,
            };
            match crate::structured::toml(&info) {
                Ok(toml) => println!("{toml}"),
                Err(e) => {
                    eprintln!("{}: {}", "Error serializing output".red(), e);
                    return CliExitCode::EnvironmentError;
                }
            }
        }
    }

    CliExitCode::Success
}

/// Clear the cache
async fn clear(format: OutputFormat, config: &Config, cache_dir: Option<PathBuf>) -> CliExitCode {
    let cache = match cache_dir {
        Some(dir) if config.cache.enabled => FileCache::with_path(dir, config.cache.ttl),
        dir => {
            let output = DisabledOutput {
                cleared: 0,
                disabled: !config.cache.enabled,
                path: dir.map(|dir| dir.to_string_lossy().into_owned()),
            };
            match format {
                OutputFormat::Human if output.disabled => {
                    println!("{}", "Cache is disabled".yellow());
                }
                OutputFormat::Human => println!("{}", dir_text(None).yellow()),
                OutputFormat::Json => match crate::structured::json(&output) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("{}: {}", "Error serializing output".red(), e);
                        return CliExitCode::EnvironmentError;
                    }
                },
                OutputFormat::Toml => match crate::structured::toml(&output) {
                    Ok(toml) => println!("{toml}"),
                    Err(e) => {
                        eprintln!("{}: {}", "Error serializing output".red(), e);
                        return CliExitCode::EnvironmentError;
                    }
                },
            }
            return CliExitCode::Success;
        }
    };

    let cache_dir = cache.cache_dir().clone();
    // Use spawn_blocking to avoid blocking the async runtime with std::fs calls
    let stats_result = {
        let dir = cache_dir.clone();
        tokio::task::spawn_blocking(move || get_cache_stats(&dir))
            .await
            .map_err(|e| std::io::Error::other(format!("task join error: {e}")))
            .and_then(|r| r)
    };
    let (entry_count, _) = match stats_result {
        Ok(stats) => stats,
        Err(e) => return report_failure("Error reading cache directory", &e),
    };

    if entry_count == 0 {
        let output = ClearedOutput { cleared: 0 };
        match format {
            OutputFormat::Human => println!("{}", "Cache is already empty".dimmed()),
            OutputFormat::Json => match crate::structured::json(&output) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("{}: {}", "Error serializing output".red(), e);
                    return CliExitCode::EnvironmentError;
                }
            },
            OutputFormat::Toml => match crate::structured::toml(&output) {
                Ok(toml) => println!("{toml}"),
                Err(e) => {
                    eprintln!("{}: {}", "Error serializing output".red(), e);
                    return CliExitCode::EnvironmentError;
                }
            },
        }
        return CliExitCode::Success;
    }

    match cache.clear().await {
        Ok(()) => {
            let output = ClearedOutput {
                cleared: entry_count as u64,
            };
            match format {
                OutputFormat::Human => {
                    println!("{} {} cache entries", "Cleared".green(), entry_count);
                }
                OutputFormat::Json => match crate::structured::json(&output) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("{}: {}", "Error serializing output".red(), e);
                        return CliExitCode::EnvironmentError;
                    }
                },
                OutputFormat::Toml => match crate::structured::toml(&output) {
                    Ok(toml) => println!("{toml}"),
                    Err(e) => {
                        eprintln!("{}: {}", "Error serializing output".red(), e);
                        return CliExitCode::EnvironmentError;
                    }
                },
            }
            CliExitCode::Success
        }
        Err(e) => report_failure("Error clearing cache", &e),
    }
}

/// Print cache path
fn path(format: OutputFormat, cache_dir: Option<&Path>) -> CliExitCode {
    let output = PathOutput {
        path: cache_dir.map(|dir| dir.to_string_lossy().into_owned()),
    };

    match format {
        OutputFormat::Human => {
            println!("{}", dir_text(cache_dir));
        }
        OutputFormat::Json => match crate::structured::json(&output) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("{}: {}", "Error serializing output".red(), e);
                return CliExitCode::EnvironmentError;
            }
        },
        OutputFormat::Toml => match crate::structured::toml(&output) {
            Ok(toml) => println!("{toml}"),
            Err(e) => {
                eprintln!("{}: {}", "Error serializing output".red(), e);
                return CliExitCode::EnvironmentError;
            }
        },
    }

    CliExitCode::Success
}

fn report_failure(what: &str, error: &dyn std::fmt::Display) -> CliExitCode {
    eprintln!("{}: {}", what.red(), sanitize_line(&error.to_string()));
    CliExitCode::EnvironmentError
}

/// Get the count and total size of the cache entry files in `cache_dir`.
///
/// Returns an error if the cache directory exists but cannot be read.
fn get_cache_stats(cache_dir: &Path) -> std::io::Result<(usize, u64)> {
    if !cache_dir.exists() {
        return Ok((0, 0));
    }

    let mut count = 0;
    let mut size = 0u64;

    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(cache::is_entry_file_name)
        {
            count += 1;
            if let Ok(meta) = entry.metadata() {
                size += meta.len();
            }
        }
    }

    Ok((count, size))
}

/// Format duration for human display
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 86400 {
        let days = secs / 86400;
        format!("{days} day{}", if days == 1 { "" } else { "s" })
    } else if secs >= 3600 {
        let hours = secs / 3600;
        format!("{hours} hour{}", if hours == 1 { "" } else { "s" })
    } else if secs >= 60 {
        let mins = secs / 60;
        format!("{mins} minute{}", if mins == 1 { "" } else { "s" })
    } else {
        format!("{secs} second{}", if secs == 1 { "" } else { "s" })
    }
}

/// Format size for human display
fn format_size(bytes: u64) -> String {
    let tenths = |unit: u64| (u128::from(bytes) * 10 + u128::from(unit) / 2) / u128::from(unit);
    if bytes >= 1024 * 1024 {
        let t = tenths(1024 * 1024);
        format!("{}.{} MB", t / 10, t % 10)
    } else if bytes >= 1024 {
        let t = tenths(1024);
        format!("{}.{} KB", t / 10, t % 10)
    } else if bytes == 1 {
        "1 byte".to_string()
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use actup_core::version;

    use super::*;

    #[tokio::test]
    async fn clear_counts_and_removes_only_cache_entries() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let config = Config::default();
        let cache = FileCache::with_path(temp.path().to_path_buf(), config.cache.ttl);
        for key in ["a", "b"] {
            cache
                .set(key, cache::Entry::new(version::Set::default()))
                .await?;
        }
        for name in ["package.json", "tsconfig.json"] {
            std::fs::write(temp.path().join(name), "{}")?;
        }
        assert_eq!(get_cache_stats(temp.path())?.0, 2);

        for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Toml] {
            assert_eq!(
                info(format, &config, Some(temp.path())),
                CliExitCode::Success
            );
        }
        assert_eq!(
            clear(OutputFormat::Json, &config, Some(temp.path().to_path_buf())).await,
            CliExitCode::Success
        );

        let mut names: Vec<_> = std::fs::read_dir(temp.path())?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        names.sort();
        assert_eq!(names, ["package.json", "tsconfig.json"]);
        assert_eq!(get_cache_stats(temp.path())?, (0, 0));
        Ok(())
    }

    #[tokio::test]
    async fn commands_without_a_cache_dir_report_it_and_succeed() {
        let config = Config::default();
        let mut off = Config::default();
        off.cache.enabled = false;
        for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Toml] {
            assert_eq!(info(format, &config, None), CliExitCode::Success);
            assert_eq!(path(format, None), CliExitCode::Success);
            assert_eq!(clear(format, &config, None).await, CliExitCode::Success);
            assert_eq!(clear(format, &off, None).await, CliExitCode::Success);
        }
        assert_eq!(dir_text(None), format!("none ({})", cache::NO_DIR_WARNING));
        assert_eq!(dir_text(Some(Path::new("/c/actup"))), "/c/actup");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30 seconds");
        assert_eq!(format_duration(Duration::from_secs(1)), "1 second");
        assert_eq!(format_duration(Duration::from_mins(1)), "1 minute");
        assert_eq!(format_duration(Duration::from_mins(2)), "2 minutes");
        assert_eq!(format_duration(Duration::from_hours(1)), "1 hour");
        assert_eq!(format_duration(Duration::from_hours(2)), "2 hours");
        assert_eq!(format_duration(Duration::from_hours(24)), "1 day");
        assert_eq!(format_duration(Duration::from_hours(48)), "2 days");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(512), "512 bytes");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 + 512 * 1024), "1.5 MB");
        assert_eq!(format_size(1075), "1.0 KB");
        assert_eq!(format_size(1076), "1.1 KB");
        assert_eq!(format_size(1024 * 1024 - 1), "1024.0 KB");
        assert_eq!(format_size(u64::MAX), "17592186044416.0 MB");
    }
}
