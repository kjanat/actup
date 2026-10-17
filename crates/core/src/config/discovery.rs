//! Config file discovery
//!
//! Searches for config files in standard locations with defined priority.
//! Discovery order (per directory level, walking up from start):
//! 1. Root: `actup.*`, `.actup.*`
//! 2. `.config/actup.*`
//! 3. `.github/actup.*`
//!
//! Extension priority: toml, json, jsonc, yaml, yml

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Directories to search (relative to each ancestor)
/// Empty string = root level
const CONFIG_DIRS: &[&str] = &["", ".config", ".github"];

/// Config file base names
/// `.actup` only valid at root (subdirs are already hidden)
const CONFIG_NAMES_ROOT: &[&str] = &["actup", ".actup"];
const CONFIG_NAMES_SUBDIR: &[&str] = &["actup"];

/// Supported extensions in priority order
pub const CONFIG_EXTENSIONS: &[&str] = &["toml", "json", "jsonc", "yaml", "yml"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Toml,
    Json,
    Jsonc,
    Yaml,
}

impl Format {
    pub(crate) fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => Self::Json,
            Some("jsonc") => Self::Jsonc,
            Some("yaml" | "yml") => Self::Yaml,
            _ => Self::Toml,
        }
    }
}

/// A repo config location relative to the directory being searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    dir: Option<&'static str>,
    name: &'static str,
    ext: &'static str,
}

impl Candidate {
    /// Return the path of this candidate inside `base`.
    #[must_use]
    pub fn path_in(&self, base: &Path) -> PathBuf {
        let file = format!("{}.{}", self.name, self.ext);
        match self.dir {
            Some(dir) => base.join(dir).join(file),
            None => base.join(file),
        }
    }

    /// Return the `/`-separated path of this candidate relative to the searched directory.
    #[must_use]
    pub fn relative(&self) -> String {
        self.dir.map_or_else(
            || format!("{}.{}", self.name, self.ext),
            |dir| format!("{dir}/{}.{}", self.name, self.ext),
        )
    }
}

/// Iterate the repo config candidates of one directory in priority order.
pub fn repo_candidates() -> impl Iterator<Item = Candidate> {
    CONFIG_DIRS.iter().flat_map(|&dir| {
        let (dir, names) = if dir.is_empty() {
            (None, CONFIG_NAMES_ROOT)
        } else {
            (Some(dir), CONFIG_NAMES_SUBDIR)
        };

        names.iter().flat_map(move |&name| {
            CONFIG_EXTENSIONS
                .iter()
                .map(move |&ext| Candidate { dir, name, ext })
        })
    })
}

/// Iterate all candidate config paths for a directory (no existence check).
///
/// Yields paths in priority order for display/debugging.
pub fn iter_config_candidates(base: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    repo_candidates().map(move |candidate| candidate.path_in(base))
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
/// - Any OS with an absolute `XDG_CONFIG_HOME`: `$XDG_CONFIG_HOME/actup/config.toml`
/// - Linux: `~/.config/actup/config.toml`
/// - macOS: `~/Library/Application Support/actup/config.toml`
/// - Windows: `%APPDATA%/actup/config.toml`
#[must_use]
pub fn user_config_path() -> Option<PathBuf> {
    user_config_path_from(std::env::var_os("XDG_CONFIG_HOME"), dirs::config_dir)
}

fn user_config_path_from(
    xdg_config_home: Option<OsString>,
    platform_config_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let config_dir = xdg_config_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(platform_config_dir)?;
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

    #[test]
    fn test_repo_candidates_match_paths() {
        let base = Path::new("/repo");
        let relative: Vec<_> = repo_candidates().map(|c| base.join(c.relative())).collect();
        let joined: Vec<_> = iter_config_candidates(base).collect();
        assert_eq!(relative, joined);
        assert_eq!(relative.len(), 20);
    }

    #[test]
    fn test_repo_candidate_relative() {
        let candidates: Vec<_> = repo_candidates().collect();
        let Some(first) = candidates.first() else {
            panic!("no candidates");
        };
        assert_eq!(first.relative(), "actup.toml");
        let Some(github_yaml) = candidates
            .iter()
            .find(|c| c.relative() == ".github/actup.yaml")
        else {
            panic!("missing .github/actup.yaml");
        };
        assert_eq!(
            github_yaml.path_in(Path::new("/repo")),
            PathBuf::from("/repo/.github/actup.yaml")
        );
    }

    #[test]
    fn test_user_config_path_prefers_absolute_xdg_config_home() -> std::io::Result<()> {
        let xdg = std::path::absolute("xdg")?;
        let platform = std::path::absolute("platform")?;
        let platform_config = platform.join("actup").join("config.toml");

        assert_eq!(
            user_config_path_from(Some(xdg.clone().into_os_string()), || Some(
                platform.clone()
            )),
            Some(xdg.join("actup").join("config.toml"))
        );
        for unset in [
            None,
            Some(OsString::new()),
            Some(OsString::from("relative/xdg")),
        ] {
            assert_eq!(
                user_config_path_from(unset.clone(), || Some(platform.clone())),
                Some(platform_config.clone()),
                "{unset:?}"
            );
        }
        assert_eq!(user_config_path_from(None, || None), None);
        Ok(())
    }

    #[test]
    fn test_format_from_path() {
        assert_eq!(Format::from_path(Path::new("a/actup.toml")), Format::Toml);
        assert_eq!(Format::from_path(Path::new("a/actup.json")), Format::Json);
        assert_eq!(Format::from_path(Path::new("a/actup.jsonc")), Format::Jsonc);
        assert_eq!(Format::from_path(Path::new("a/actup.yaml")), Format::Yaml);
        assert_eq!(Format::from_path(Path::new("a/actup.yml")), Format::Yaml);
        assert_eq!(Format::from_path(Path::new("a/actup.conf")), Format::Toml);
        assert_eq!(Format::from_path(Path::new("a/actup")), Format::Toml);
    }
}
