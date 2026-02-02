//! Cache command - manage the version cache

use std::path::PathBuf;
use std::time::Duration;

use actup_core::Config;
use actup_core::cache::Cache;
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::cli::{CacheAction, OutputFormat};
use crate::file_cache::{FileCache, default_cache_dir};

/// Cache info for JSON output
#[derive(Serialize)]
struct CacheInfo {
    path: String,
    enabled: bool,
    ttl_secs: u64,
    entry_count: usize,
    total_size_bytes: u64,
}

/// Run the cache command
pub async fn run(action: &CacheAction, format: OutputFormat, config: &Config) -> i32 {
    match action {
        CacheAction::Info => info(format, config),
        CacheAction::Clear => clear(format, config).await,
        CacheAction::Path => path(format, config),
    }
}

/// Show cache info
fn info(format: OutputFormat, config: &Config) -> i32 {
    let cache_dir = get_cache_dir(config);
    let (entry_count, total_size) = match get_cache_stats(&cache_dir) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("{}: {}", "Error reading cache directory".red(), e);
            return 3; // Environment error
        }
    };

    match format {
        OutputFormat::Human => {
            println!("{}", "Cache Information".bold());
            println!();
            println!("  {:<12} {}", "Path:".dimmed(), cache_dir.display());
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
                path: cache_dir.to_string_lossy().into_owned(),
                enabled: config.cache.enabled,
                ttl_secs: config.cache.ttl.as_secs(),
                entry_count,
                total_size_bytes: total_size,
            };
            match serde_json::to_string_pretty(&info) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("{}: {}", "Error serializing JSON".red(), e);
                    return 3; // Environment error
                }
            }
        }
    }

    0
}

/// Clear the cache
async fn clear(format: OutputFormat, config: &Config) -> i32 {
    let cache = if config.cache.enabled {
        config.cache.path.as_ref().map_or_else(
            || FileCache::new(config.cache.ttl),
            |path| FileCache::with_path(path.clone(), config.cache.ttl),
        )
    } else {
        match format {
            OutputFormat::Human => println!("{}", "Cache is disabled".yellow()),
            OutputFormat::Json => println!(r#"{{"cleared": 0, "disabled": true}}"#),
        }
        return 0;
    };

    let cache_dir = cache.cache_dir().clone();
    let (entry_count, _) = match get_cache_stats(&cache_dir) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("{}: {}", "Error reading cache directory".red(), e);
            return 3; // Environment error
        }
    };

    if entry_count == 0 {
        match format {
            OutputFormat::Human => println!("{}", "Cache is already empty".dimmed()),
            OutputFormat::Json => println!(r#"{{"cleared": 0}}"#),
        }
        return 0;
    }

    match cache.clear().await {
        Ok(()) => {
            match format {
                OutputFormat::Human => {
                    println!("{} {} cache entries", "Cleared".green(), entry_count);
                }
                OutputFormat::Json => {
                    println!(r#"{{"cleared": {entry_count}}}"#);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("{}: {}", "Error clearing cache".red(), e);
            3 // Environment error
        }
    }
}

/// Print cache path
fn path(format: OutputFormat, config: &Config) -> i32 {
    let cache_dir = get_cache_dir(config);

    match format {
        OutputFormat::Human => {
            println!("{}", cache_dir.display());
        }
        OutputFormat::Json => {
            #[derive(Serialize)]
            struct PathOutput {
                path: String,
            }
            let output = PathOutput {
                path: cache_dir.to_string_lossy().into_owned(),
            };
            match serde_json::to_string(&output) {
                Ok(json) => println!("{json}"),
                Err(e) => {
                    eprintln!("{}: {}", "Error serializing JSON".red(), e);
                    return 3; // Environment error
                }
            }
        }
    }

    0
}

/// Get cache directory from config or default
fn get_cache_dir(config: &Config) -> PathBuf {
    config.cache.path.clone().unwrap_or_else(default_cache_dir)
}

/// Get cache statistics (entry count, total size)
///
/// Returns an error if the cache directory exists but cannot be read.
fn get_cache_stats(cache_dir: &PathBuf) -> std::io::Result<(usize, u64)> {
    if !cache_dir.exists() {
        return Ok((0, 0));
    }

    let mut count = 0;
    let mut size = 0u64;

    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
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
#[allow(clippy::cast_precision_loss)] // Acceptable for display purposes
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30 seconds");
        assert_eq!(format_duration(Duration::from_secs(1)), "1 second");
        assert_eq!(format_duration(Duration::from_secs(60)), "1 minute");
        assert_eq!(format_duration(Duration::from_secs(120)), "2 minutes");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1 hour");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2 hours");
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1 day");
        assert_eq!(format_duration(Duration::from_secs(172_800)), "2 days");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(512), "512 bytes");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 + 512 * 1024), "1.5 MB");
    }
}
