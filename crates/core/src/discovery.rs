//! File discovery for GitHub Actions workflows and composite actions
//!
//! Discovers:
//! - Workflow files in .github/workflows/
//! - Composite actions in .github/actions/
//! - Custom paths from configuration

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// Default paths to scan for workflows
pub const DEFAULT_WORKFLOW_PATTERNS: &[&str] =
    &[".github/workflows/*.yml", ".github/workflows/*.yaml"];

/// Default paths to scan for composite actions.
///
/// Covers conventional locations:
/// - Repository root (`action.yml`) - for repos that ARE an action
/// - `.github/actions/*/` - GitHub's recommended location
///
/// Note: GitHub allows `action.yml` in *any* subdirectory, but scanning
/// `**/action.yml` would be slow and produce false positives (test fixtures,
/// vendored deps, etc.). Users can add custom patterns via config's
/// `extra_paths` if they use non-standard locations like `tools/my-action/`.
pub const DEFAULT_ACTION_PATTERNS: &[&str] = &[
    "action.yml",
    "action.yaml",
    ".github/actions/*/action.yml",
    ".github/actions/*/action.yaml",
];

/// A discovered file with its type
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// Absolute path to the file
    path: PathBuf,

    /// Type of file
    kind: FileKind,
}

impl DiscoveredFile {
    /// Create a new discovered file
    #[must_use]
    pub const fn new(path: PathBuf, kind: FileKind) -> Self {
        Self { path, kind }
    }

    /// Get the file path
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the file kind
    #[must_use]
    pub const fn kind(&self) -> FileKind {
        self.kind
    }
}

/// Type of discovered file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A workflow file (.github/workflows/*.yml)
    Workflow,

    /// A composite action definition (.github/actions/*/action.yml)
    CompositeAction,

    /// A custom path (user-specified)
    Custom,
}

impl FileKind {
    /// Human-readable name
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::CompositeAction => "composite action",
            Self::Custom => "custom",
        }
    }
}

/// Discover action-related files from a directory or explicit file path.
///
/// When `root` is a file, returns that YAML file directly.
///
/// When `root` is a directory, searches for:
/// - action.yml and action.yaml in root
/// - .github/workflows/*.yml and .yaml
/// - .github/actions/*/action.yml and .yaml
/// - Any `extra_paths` from config
#[must_use]
pub fn discover_files(root: &Path, extra_patterns: &[String]) -> Vec<DiscoveredFile> {
    let mut files = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    // Explicit file input: scan that file directly instead of treating it as a repo root.
    if root.is_file() && has_yaml_ext(root) {
        files.push(DiscoveredFile::new(
            root.to_path_buf(),
            classify_explicit_file(root),
        ));
        return files;
    }

    // Discover workflows
    for pattern in DEFAULT_WORKFLOW_PATTERNS {
        let full_pattern = root.join(pattern);
        if let Some(pattern_str) = full_pattern.to_str() {
            for entry in glob::glob(pattern_str).into_iter().flatten().flatten() {
                if seen_paths.insert(entry.clone()) {
                    files.push(DiscoveredFile::new(entry, FileKind::Workflow));
                }
            }
        }
    }

    // Discover composite actions
    for pattern in DEFAULT_ACTION_PATTERNS {
        let full_pattern = root.join(pattern);
        if let Some(pattern_str) = full_pattern.to_str() {
            for entry in glob::glob(pattern_str).into_iter().flatten().flatten() {
                if seen_paths.insert(entry.clone()) {
                    files.push(DiscoveredFile::new(entry, FileKind::CompositeAction));
                }
            }
        }
    }

    // Discover from extra patterns
    for pattern in extra_patterns {
        let full_pattern = root.join(pattern);
        if let Some(pattern_str) = full_pattern.to_str() {
            for entry in glob::glob(pattern_str).into_iter().flatten().flatten() {
                // Skip if already discovered (O(1) hash lookup)
                if seen_paths.insert(entry.clone()) {
                    files.push(DiscoveredFile::new(entry, FileKind::Custom));
                }
            }
        }
    }

    // Sort for deterministic output
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn has_yaml_ext(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
}

fn classify_explicit_file(path: &Path) -> FileKind {
    let path_str = path.to_string_lossy();

    if path_str.contains(".github/workflows/") || path_str.contains(".github\\workflows\\") {
        return FileKind::Workflow;
    }

    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy();
        if name.eq_ignore_ascii_case("action.yml") || name.eq_ignore_ascii_case("action.yaml") {
            return FileKind::CompositeAction;
        }
    }

    FileKind::Custom
}

/// Check if a path looks like a GitHub Actions file
#[must_use]
pub fn is_actions_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    // Check extension
    if !has_yaml_ext(path) {
        return false;
    }

    // Check if in .github/workflows/
    if path_str.contains(".github/workflows/") || path_str.contains(".github\\workflows\\") {
        return true;
    }

    // Check if action.yml/action.yaml
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy();
        if name.eq_ignore_ascii_case("action.yml") || name.eq_ignore_ascii_case("action.yaml") {
            return true;
        }
    }

    false
}

/// Get the relative path from root, if possible
#[must_use]
pub fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_path_buf(), std::path::Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn make_temp_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let unique = format!(
            "actup-discovery-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn test_is_actions_file() {
        assert!(is_actions_file(Path::new(".github/workflows/ci.yml")));
        assert!(is_actions_file(Path::new(".github/workflows/test.yaml")));
        assert!(is_actions_file(Path::new(
            ".github/actions/my-action/action.yml"
        )));
        assert!(is_actions_file(Path::new("action.yml")));

        assert!(!is_actions_file(Path::new("random.yml")));
        assert!(!is_actions_file(Path::new("src/config.yaml")));
        assert!(!is_actions_file(Path::new("README.md")));
    }

    #[test]
    fn test_file_kind_as_str() {
        assert_eq!(FileKind::Workflow.as_str(), "workflow");
        assert_eq!(FileKind::CompositeAction.as_str(), "composite action");
        assert_eq!(FileKind::Custom.as_str(), "custom");
    }

    #[test]
    fn test_discover_files_accepts_explicit_workflow_file() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = make_temp_dir()?;
        let workflows_dir = temp.join(".github/workflows");
        fs::create_dir_all(&workflows_dir)?;
        let workflow = workflows_dir.join("release.yml");
        fs::write(&workflow, "name: Release")?;

        let files = discover_files(&workflow, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), workflow.as_path());
        assert_eq!(files[0].kind(), FileKind::Workflow);

        fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn test_discover_files_accepts_explicit_custom_yaml_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = make_temp_dir()?;
        let custom = temp.join("custom.yml");
        fs::write(&custom, "name: Custom")?;

        let files = discover_files(&custom, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), custom.as_path());
        assert_eq!(files[0].kind(), FileKind::Custom);

        fs::remove_dir_all(temp)?;
        Ok(())
    }
}
