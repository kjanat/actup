//! Config file discovery
//!
//! Searches for config files in standard locations with defined priority.
//! Discovery order (per directory level, walking up from start):
//! 1. Root: `actup.*`, `.actup.*`
//! 2. `.config/actup.*`
//! 3. `.github/actup.*`
//!
//! Extension priority: toml, json, jsonc, yaml, yml

use std::path::{Path, PathBuf};

/// Directories to search (relative to each ancestor)
/// Empty string = root level
const CONFIG_DIRS: &[&str] = &["", ".config", ".github"];

/// Config file base names
/// `.actup` only valid at root (subdirs are already hidden)
const CONFIG_NAMES_ROOT: &[&str] = &["actup", ".actup"];
const CONFIG_NAMES_SUBDIR: &[&str] = &["actup"];

/// Supported extensions in priority order
pub const CONFIG_EXTENSIONS: &[&str] = &["toml", "json", "jsonc", "yaml", "yml"];

/// Iterate all candidate config paths for a directory (no existence check).
///
/// Yields paths in priority order for display/debugging.
pub fn iter_config_candidates(base: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    CONFIG_DIRS.iter().flat_map(move |dir| {
        let names: &[&str] = if dir.is_empty() {
            CONFIG_NAMES_ROOT
        } else {
            CONFIG_NAMES_SUBDIR
        };

        names.iter().flat_map(move |name| {
            CONFIG_EXTENSIONS.iter().map(move |ext| {
                if dir.is_empty() {
                    base.join(format!("{name}.{ext}"))
                } else {
                    base.join(dir).join(format!("{name}.{ext}"))
                }
            })
        })
    })
}

/// Find repo config path, searching upward from `start_dir`.
///
/// Returns the first existing config file found.
#[must_use]
#[allow(clippy::module_name_repetitions, reason = "clear naming")]
pub fn find_repo_config(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.to_path_buf();

    loop {
        // Check all candidates at this level
        for candidate in iter_config_candidates(&dir) {
            if candidate.exists() {
                return Some(candidate);
            }
        }

        // Move up one directory
        if !dir.pop() {
            break;
        }
    }

    None
}

/// Get user/global config path.
///
/// - Linux: `~/.config/actup/config.toml`
/// - macOS: `~/Library/Application Support/actup/config.toml`
/// - Windows: `%APPDATA%/actup/config.toml`
#[must_use]
pub fn user_config_path() -> Option<PathBuf> {
    let config_dir = dirs::config_dir()?;
    Some(config_dir.join("actup").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iter_config_candidates_order() {
        let base = Path::new("/repo");
        let candidates: Vec<_> = iter_config_candidates(base).collect();

        // First should be root actup.toml
        assert_eq!(candidates[0], PathBuf::from("/repo/actup.toml"));
        // Then other root extensions
        assert_eq!(candidates[1], PathBuf::from("/repo/actup.json"));
        assert_eq!(candidates[2], PathBuf::from("/repo/actup.jsonc"));
        assert_eq!(candidates[3], PathBuf::from("/repo/actup.yaml"));
        assert_eq!(candidates[4], PathBuf::from("/repo/actup.yml"));
        // Then .actup.*
        assert_eq!(candidates[5], PathBuf::from("/repo/.actup.toml"));
        // Then .config/actup.*
        assert_eq!(candidates[10], PathBuf::from("/repo/.config/actup.toml"));
        // Then .github/actup.*
        assert_eq!(candidates[15], PathBuf::from("/repo/.github/actup.toml"));

        // Total: 5 exts * (2 root names + 1 .config name + 1 .github name) = 20
        assert_eq!(candidates.len(), 20);
    }

    #[test]
    fn test_no_dotfile_in_subdirs() {
        let base = Path::new("/repo");
        let candidates: Vec<_> = iter_config_candidates(base).collect();

        // .actup should only appear at root, not in .config or .github
        let dotfile_in_subdir = candidates
            .iter()
            .any(|p| p.to_string_lossy().contains(".config/.actup"));
        assert!(!dotfile_in_subdir);

        let dotfile_in_github = candidates
            .iter()
            .any(|p| p.to_string_lossy().contains(".github/.actup"));
        assert!(!dotfile_in_github);
    }
}
