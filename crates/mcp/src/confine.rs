//! Path confinement for tool arguments.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

/// The canonical directory that holds every path a tool reads or writes.
#[derive(Debug, Clone)]
pub struct Root(PathBuf);

/// Why a tool refuses a path argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path does not resolve to an existing file or directory.
    Missing {
        /// The path as given.
        path: String,
        /// The resolution error.
        reason: String,
    },
    /// The path resolves outside the root.
    Outside {
        /// The path as given.
        path: String,
    },
    /// The path resolves to the wrong kind of entry.
    WrongKind {
        /// The path as given.
        path: String,
        /// The kind of entry the tool needs.
        expected: &'static str,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path, reason } => write!(f, "path `{path}` does not resolve: {reason}"),
            Self::Outside { path } => write!(f, "path `{path}` resolves outside the server root"),
            Self::WrongKind { path, expected } => write!(f, "path `{path}` is not a {expected}"),
        }
    }
}

impl std::error::Error for Refusal {}

impl Root {
    /// Canonicalize `dir` as the root.
    ///
    /// # Errors
    ///
    /// Returns an error if `dir` does not exist or is not a directory.
    pub fn new(dir: &Path) -> io::Result<Self> {
        let canonical = dir.canonicalize()?;
        if !canonical.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        Ok(Self(canonical))
    }

    /// The canonical root directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Resolve `input`, relative to the root or absolute, to a canonical file inside the root.
    ///
    /// # Errors
    ///
    /// Returns a [`Refusal`] if `input` does not resolve to a file inside the root.
    pub fn file(&self, input: &str) -> Result<PathBuf, Refusal> {
        let path = self.resolve(input)?;
        if path.is_file() {
            Ok(path)
        } else {
            Err(Refusal::WrongKind {
                path: input.to_string(),
                expected: "file",
            })
        }
    }

    /// Resolve `input`, relative to the root or absolute, to a canonical directory inside the root.
    ///
    /// # Errors
    ///
    /// Returns a [`Refusal`] if `input` does not resolve to a directory inside the root.
    pub fn dir(&self, input: &str) -> Result<PathBuf, Refusal> {
        let path = self.resolve(input)?;
        if path.is_dir() {
            Ok(path)
        } else {
            Err(Refusal::WrongKind {
                path: input.to_string(),
                expected: "directory",
            })
        }
    }

    /// Canonicalize `path` and return it when it lies inside the root.
    ///
    /// # Errors
    ///
    /// Returns a [`Refusal`] if `path` does not resolve or resolves outside the root.
    pub fn contain(&self, path: &Path) -> Result<PathBuf, Refusal> {
        let shown = path.display().to_string();
        let canonical = path.canonicalize().map_err(|e| Refusal::Missing {
            path: shown.clone(),
            reason: e.to_string(),
        })?;
        if canonical.starts_with(&self.0) {
            Ok(canonical)
        } else {
            Err(Refusal::Outside { path: shown })
        }
    }

    /// The path of `canonical` relative to the root, with `/` separators.
    #[must_use]
    pub fn relative(&self, canonical: &Path) -> String {
        let relative = canonical.strip_prefix(&self.0).unwrap_or(canonical);
        let parts: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if parts.is_empty() {
            ".".to_string()
        } else {
            parts.join("/")
        }
    }

    fn resolve(&self, input: &str) -> Result<PathBuf, Refusal> {
        if input.is_empty() {
            return Err(Refusal::Missing {
                path: String::new(),
                reason: "empty path".to_string(),
            });
        }
        let given = Path::new(input);
        let joined = if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.0.join(given)
        };
        self.contain(&joined).map_err(|refusal| match refusal {
            Refusal::Missing { reason, .. } => Refusal::Missing {
                path: input.to_string(),
                reason,
            },
            Refusal::Outside { .. } | Refusal::WrongKind { .. } => Refusal::Outside {
                path: input.to_string(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn tree() -> Result<(tempfile::TempDir, Root), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join("repo/.github/workflows"))?;
        fs::write(
            dir.path().join("repo/.github/workflows/ci.yml"),
            "on: push\n",
        )?;
        fs::write(dir.path().join("outside.yml"), "on: push\n")?;
        let root = Root::new(&dir.path().join("repo"))?;
        Ok((dir, root))
    }

    #[test]
    fn file_resolves_relative_and_absolute_paths_inside_the_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, root) = tree()?;
        let relative = root.file(".github/workflows/ci.yml")?;
        let absolute = root.file(&relative.display().to_string())?;
        assert_eq!(relative, absolute);
        assert_eq!(root.relative(&relative), ".github/workflows/ci.yml");
        assert_eq!(root.relative(root.path()), ".");
        Ok(())
    }

    #[test]
    fn file_refuses_traversal_and_absolute_paths_outside_the_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let (dir, root) = tree()?;
        assert_eq!(
            root.file("../outside.yml"),
            Err(Refusal::Outside {
                path: "../outside.yml".into()
            })
        );
        let absolute = dir.path().join("outside.yml").display().to_string();
        assert_eq!(
            root.file(&absolute),
            Err(Refusal::Outside { path: absolute })
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn file_refuses_a_symlink_that_escapes_the_root() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, root) = tree()?;
        std::os::unix::fs::symlink(
            dir.path().join("outside.yml"),
            root.path().join(".github/workflows/link.yml"),
        )?;
        assert_eq!(
            root.file(".github/workflows/link.yml"),
            Err(Refusal::Outside {
                path: ".github/workflows/link.yml".into()
            })
        );
        Ok(())
    }

    #[test]
    fn file_and_dir_refuse_missing_empty_and_wrong_kind() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, root) = tree()?;
        assert!(matches!(
            root.file("nope.yml"),
            Err(Refusal::Missing { .. })
        ));
        assert!(matches!(root.file(""), Err(Refusal::Missing { .. })));
        assert_eq!(
            root.file(".github"),
            Err(Refusal::WrongKind {
                path: ".github".into(),
                expected: "file"
            })
        );
        assert_eq!(
            root.dir(".github/workflows/ci.yml"),
            Err(Refusal::WrongKind {
                path: ".github/workflows/ci.yml".into(),
                expected: "directory"
            })
        );
        assert_eq!(root.dir(".")?, root.path());
        assert!(matches!(root.dir(".."), Err(Refusal::Outside { .. })));
        Ok(())
    }

    #[test]
    fn new_refuses_a_file() -> Result<(), Box<dyn std::error::Error>> {
        let (dir, _root) = tree()?;
        let Err(e) = Root::new(&dir.path().join("outside.yml")) else {
            panic!("a file must not become a root");
        };
        assert_eq!(e.kind(), io::ErrorKind::NotADirectory);
        Ok(())
    }

    #[test]
    fn refusals_name_the_path() {
        let missing = Refusal::Missing {
            path: "a.yml".into(),
            reason: "gone".into(),
        };
        assert_eq!(missing.to_string(), "path `a.yml` does not resolve: gone");
        assert_eq!(
            Refusal::Outside {
                path: "../a".into()
            }
            .to_string(),
            "path `../a` resolves outside the server root"
        );
        assert_eq!(
            Refusal::WrongKind {
                path: "a".into(),
                expected: "file"
            }
            .to_string(),
            "path `a` is not a file"
        );
    }
}
