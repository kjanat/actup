//! File discovery for GitHub Actions workflows and composite actions
//!
//! Discovers:
//! - Workflow files in .github/workflows/
//! - Composite actions in .github/actions/
//! - Custom paths from configuration

use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::SourceFormat;

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
/// `scan.extra_paths` if they use non-standard locations like `tools/my-action/`.
pub const DEFAULT_ACTION_PATTERNS: &[&str] = &[
    "action.yml",
    "action.yaml",
    ".github/actions/*/action.yml",
    ".github/actions/*/action.yaml",
];

/// A `scan.extra_paths` glob, relative to the scan root and free of `..` segments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExtraPath(String);

impl ExtraPath {
    /// Return the glob as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ExtraPath {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let bytes = raw.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if raw.starts_with(['/', '\\']) || drive || Path::new(raw).is_absolute() {
            return Err(format!(
                "`scan.extra_paths` entry `{raw}` is absolute; write it relative to the scan root"
            ));
        }
        if raw.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(format!(
                "`scan.extra_paths` entry `{raw}` contains `..`; it must stay inside the scan root"
            ));
        }
        Ok(Self(raw.to_owned()))
    }
}

impl TryFrom<String> for ExtraPath {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<ExtraPath> for String {
    fn from(path: ExtraPath) -> Self {
        path.0
    }
}

impl fmt::Display for ExtraPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

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

    /// Get the syntax to read `uses:` refs from, by the file's extension.
    #[must_use]
    pub fn format(&self) -> SourceFormat {
        SourceFormat::from_path(&self.path)
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
/// When `root` is a file, returns that file directly, whatever its extension.
///
/// When `root` is a directory, searches for:
/// - action.yml and action.yaml in root
/// - .github/workflows/*.yml and .yaml
/// - .github/actions/*/action.yml and .yaml
/// - Any `scan.extra_paths` from config
///
/// A match that resolves outside `root` after following symlinks is skipped.
#[must_use]
pub fn discover_files(root: &Path, extra_patterns: &[ExtraPath]) -> Vec<DiscoveredFile> {
    let mut files = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    // Explicit file input: scan that file directly instead of treating it as a repo root.
    if root.is_file() {
        files.push(DiscoveredFile::new(
            root.to_path_buf(),
            classify_explicit_file(root),
        ));
        return files;
    }

    let Ok(canonical_root) = root.canonicalize() else {
        return files;
    };

    let defaults = DEFAULT_WORKFLOW_PATTERNS
        .iter()
        .map(|p| (*p, FileKind::Workflow))
        .chain(
            DEFAULT_ACTION_PATTERNS
                .iter()
                .map(|p| (*p, FileKind::CompositeAction)),
        );
    let extras = extra_patterns
        .iter()
        .map(|p| (p.as_str(), FileKind::Custom));

    for (pattern, kind) in defaults.chain(extras) {
        let full_pattern = root.join(pattern);
        let Some(pattern_str) = full_pattern.to_str() else {
            continue;
        };
        for entry in glob::glob(pattern_str).into_iter().flatten().flatten() {
            if !canonical_inside(&canonical_root, &entry) {
                tracing::debug!(path = %entry.display(), root = %root.display(), "skipping match outside the scan root");
                continue;
            }
            if seen_paths.insert(entry.clone()) {
                files.push(DiscoveredFile::new(entry, kind));
            }
        }
    }

    // Sort for deterministic output
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn canonical_inside(canonical_root: &Path, path: &Path) -> bool {
    path.canonicalize()
        .is_ok_and(|path| path.starts_with(canonical_root))
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

    #[test]
    fn test_extra_path_rejects_escapes() {
        for raw in [
            "../../../etc/*.yml",
            "ci/../../x.yml",
            "..",
            "ci\\..\\x.yml",
        ] {
            let Err(reason) = raw.parse::<ExtraPath>() else {
                panic!("`{raw}` must be rejected");
            };
            assert_eq!(
                reason,
                format!(
                    "`scan.extra_paths` entry `{raw}` contains `..`; it must stay inside the scan root"
                )
            );
        }
        for raw in [
            "/etc/*.yml",
            "\\\\server\\share\\x.yml",
            "C:/ci/x.yml",
            "c:x.yml",
        ] {
            let Err(reason) = raw.parse::<ExtraPath>() else {
                panic!("`{raw}` must be rejected");
            };
            assert_eq!(
                reason,
                format!(
                    "`scan.extra_paths` entry `{raw}` is absolute; write it relative to the scan root"
                )
            );
        }
    }

    #[test]
    fn test_extra_path_accepts_relative_globs() -> Result<(), String> {
        for raw in [
            ".gitea/workflows/*.yml",
            "docs/**/*.md",
            "a..b/x.yml",
            "./ci.yml",
        ] {
            let path: ExtraPath = raw.parse()?;
            assert_eq!(path.as_str(), raw);
            assert_eq!(path.to_string(), raw);
            assert_eq!(String::from(path), raw);
        }
        Ok(())
    }

    #[test]
    fn test_discover_files_missing_root_finds_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let temp = make_temp_dir()?;
        assert_eq!(discover_files(&temp.join("absent"), &[]), []);
        fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_files_skips_symlinks_out_of_the_root() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::symlink;

        let temp = make_temp_dir()?;
        let root = temp.join("repo");
        let outside = temp.join("outside");
        let workflows = root.join(".github/workflows");
        fs::create_dir_all(&workflows)?;
        fs::create_dir_all(&outside)?;
        fs::write(outside.join("evil.yml"), "name: Evil")?;
        fs::write(workflows.join("ci.yml"), "name: CI")?;
        symlink(outside.join("evil.yml"), workflows.join("evil.yml"))?;
        symlink(workflows.join("ci.yml"), workflows.join("alias.yml"))?;
        symlink(&outside, root.join("linked"))?;

        let extra = ["linked/*.yml".parse()?];
        let found: Vec<_> = discover_files(&root, &extra)
            .into_iter()
            .map(|f| f.path().to_path_buf())
            .collect();
        assert_eq!(
            found,
            [workflows.join("alias.yml"), workflows.join("ci.yml")]
        );

        fs::remove_dir_all(temp)?;
        Ok(())
    }
}
