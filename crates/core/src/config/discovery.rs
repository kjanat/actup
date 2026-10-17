//! Config file discovery
//!
//! Searches for config files in standard locations with defined priority.
//! Discovery order (per directory level, walking up from start to the repository root):
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

/// Return the directories repo config discovery searches, from `start_dir` upward.
///
/// Both paths resolve through symlinks and `..` first. The walk ends at the
/// first directory that contains `.git` or equals `ceiling`.
#[must_use]
pub fn repo_search_dirs(start_dir: &Path, ceiling: Option<&Path>) -> Vec<PathBuf> {
    let start = canonical_path(start_dir);
    let ceiling = ceiling.map(canonical_path);
    let mut dirs = Vec::new();
    for dir in start.ancestors() {
        dirs.push(dir.to_path_buf());
        if ceiling.as_deref() == Some(dir) || dir.join(".git").exists() {
            break;
        }
    }
    dirs
}

/// Return `path` resolved through symlinks and `..`, in the form the OS displays.
///
/// A Windows `\\?\` verbatim prefix is removed when the path stays valid
/// without it. When `path` cannot be resolved, the result is the absolute
/// path, else `path` itself.
#[must_use]
pub fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize()
        .map(|canonical| {
            if cfg!(windows) {
                canonical
                    .to_str()
                    .and_then(without_verbatim_prefix)
                    .map_or_else(|| canonical.clone(), PathBuf::from)
            } else {
                canonical
            }
        })
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

const MAX_PATH: usize = 260;

fn without_verbatim_prefix(path: &str) -> Option<String> {
    let plain = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        let disk = path.strip_prefix(r"\\?\")?;
        let bytes = disk.as_bytes();
        let drive = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\';
        if !drive {
            return None;
        }
        disk.to_owned()
    };
    (plain.len() < MAX_PATH).then_some(plain)
}

/// Find the first existing repo config in [`repo_search_dirs`].
#[must_use]
#[allow(clippy::module_name_repetitions, reason = "clear naming")]
pub fn find_repo_config(start_dir: &Path, ceiling: Option<&Path>) -> Option<PathBuf> {
    repo_search_dirs(start_dir, ceiling)
        .iter()
        .find_map(|dir| iter_config_candidates(dir).find(|candidate| candidate.exists()))
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
    fn test_without_verbatim_prefix_keeps_valid_plain_paths() {
        assert_eq!(
            without_verbatim_prefix(r"\\?\C:\Users\me\repo").as_deref(),
            Some(r"C:\Users\me\repo")
        );
        assert_eq!(
            without_verbatim_prefix(r"\\?\UNC\server\share\repo").as_deref(),
            Some(r"\\server\share\repo")
        );
        let long = format!(r"\\?\C:\{}", "a".repeat(MAX_PATH));
        for kept in [
            r"C:\Users\me",
            r"\\?\Volume{1234}\repo",
            r"\\?\C:",
            "/tmp/repo",
            long.as_str(),
        ] {
            assert_eq!(without_verbatim_prefix(kept), None, "{kept}");
        }
    }

    #[test]
    fn test_canonical_path_resolves_parents_and_falls_back() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let sub = temp.path().join("sub");
        std::fs::create_dir(&sub)?;
        let resolved = canonical_path(&sub.join("..").join("sub"));
        assert_eq!(resolved, canonical_path(&sub));
        assert!(resolved.ends_with("sub"));
        assert!(!resolved.to_string_lossy().starts_with(r"\\?\"));
        let missing = temp.path().join("missing");
        assert_eq!(canonical_path(&missing), std::path::absolute(&missing)?);
        assert_eq!(canonical_path(Path::new("")), PathBuf::new());
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
