//! Crash-safe file replacement shared by the CLI and the language server.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

const BOM: char = '\u{feff}';

static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace the file at `path` with `contents`.
///
/// # Errors
///
/// Returns the I/O error of the first step that fails. The file at `path` is
/// then unchanged and the temp file is removed.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = resolve(path)?;
    let lock = lock_for(&target);
    let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    replace(&target, contents, Write::write_all)
}

/// Resolve `path` through symlinks and return it when it lies inside `root` or is `root`.
///
/// # Errors
///
/// Returns a [`io::ErrorKind::PermissionDenied`] error naming `root` for a
/// path outside it, or the I/O error of resolving either path.
pub fn resolve_within(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let target = resolve(path)?;
    if target.starts_with(fs::canonicalize(root)?) {
        Ok(target)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("resolves outside {}", root.display()),
        ))
    }
}

/// Read the UTF-8 file at `path` inside `root`, pass its text to `edit`, and replace the file with the result.
///
/// The read and the write happen under the lock for the file. A byte order
/// mark that starts the old text also starts the new one.
///
/// # Errors
///
/// Returns the error of [`resolve_within`] or `edit`, or the I/O error of the
/// read or the write. The file is then unchanged.
pub fn rewrite_atomic<E: From<io::Error>>(
    root: &Path,
    path: &Path,
    edit: impl FnOnce(&str) -> Result<String, E>,
) -> Result<(), E> {
    let target = resolve_within(root, path)?;
    let lock = lock_for(&target);
    let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let current = fs::read_to_string(&target)?;
    let mut updated = edit(&current)?;
    if current.starts_with(BOM) && !updated.starts_with(BOM) {
        updated.insert(0, BOM);
    }
    replace(&target, updated.as_bytes(), Write::write_all)?;
    Ok(())
}

fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    match fs::canonicalize(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(e);
            };
            Ok(fs::canonicalize(parent)?.join(name))
        }
        resolved => resolved,
    }
}

fn lock_for(target: &Path) -> Arc<Mutex<()>> {
    let mut locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    Arc::clone(locks.entry(target.to_path_buf()).or_default())
}

fn replace(
    target: &Path,
    contents: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path names no file",
        ));
    };
    let permissions = match fs::metadata(target) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let name = name.to_string_lossy();
    let (temp, mut file) =
        create_temp(dir, &name, || TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))?;
    let written = permissions
        .map_or(Ok(()), |permissions| file.set_permissions(permissions))
        .and_then(|()| write(&mut file, contents))
        .and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, target));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn create_temp(
    dir: &Path,
    name: &str,
    mut next: impl FnMut() -> u64,
) -> io::Result<(PathBuf, File)> {
    loop {
        let temp = dir.join(format!(
            ".{name}.actup-{}-{}.tmp",
            std::process::id(),
            next()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn entries(dir: &Path) -> io::Result<Vec<String>> {
        let mut names = fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    #[test]
    fn write_atomic_replaces_and_creates_files() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, "old")?;

        write_atomic(&path, b"new")?;
        write_atomic(&dir.path().join("fresh.json"), b"{}")?;

        assert_eq!(fs::read_to_string(&path)?, "new");
        assert_eq!(fs::read_to_string(dir.path().join("fresh.json"))?, "{}");
        assert_eq!(entries(dir.path())?, ["ci.yml", "fresh.json"]);
        Ok(())
    }

    #[test]
    fn failed_write_leaves_the_original_and_no_temp_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, "original")?;

        let result = replace(&resolve(&path)?, b"replacement", |file, bytes| {
            file.write_all(&bytes[..4])?;
            Err(io::Error::other("disk full"))
        });

        let Err(e) = result else {
            panic!("the injected error must surface");
        };
        assert_eq!(e.to_string(), "disk full");
        assert_eq!(fs::read_to_string(&path)?, "original");
        assert_eq!(entries(dir.path())?, ["ci.yml"]);
        Ok(())
    }

    #[test]
    fn create_temp_skips_a_taken_name() -> TestResult {
        let dir = tempfile::tempdir()?;
        let taken = dir
            .path()
            .join(format!(".ci.yml.actup-{}-7.tmp", std::process::id()));
        fs::write(&taken, "taken")?;
        let mut ids = [7, 8].into_iter();

        let (temp, _file) = create_temp(dir.path(), "ci.yml", || ids.next().unwrap_or(0))?;

        assert_eq!(
            temp,
            dir.path()
                .join(format!(".ci.yml.actup-{}-8.tmp", std::process::id()))
        );
        assert_eq!(fs::read_to_string(&taken)?, "taken");
        Ok(())
    }

    #[test]
    fn writes_fail_for_paths_that_name_no_writable_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("file");
        fs::write(&file, "data")?;

        let missing_dir = dir.path().join("missing").join("ci.yml");
        let kind = |result: io::Result<()>| result.err().map(|e| e.kind());
        assert_eq!(
            kind(write_atomic(&missing_dir, b"x")),
            Some(io::ErrorKind::NotFound)
        );
        assert_eq!(
            kind(replace(Path::new("/"), b"x", Write::write_all)),
            Some(io::ErrorKind::InvalidInput)
        );
        #[cfg(unix)]
        {
            let dotdot = dir.path().join("missing").join("..");
            assert_eq!(
                kind(write_atomic(&dotdot, b"x")),
                Some(io::ErrorKind::NotFound)
            );
            assert_eq!(
                kind(replace(&file.join("x"), b"x", Write::write_all)),
                Some(io::ErrorKind::NotADirectory)
            );
        }
        assert_eq!(
            create_temp(&dir.path().join("missing"), "x", || 0)
                .err()
                .map(|e| e.kind()),
            Some(io::ErrorKind::NotFound)
        );
        assert_eq!(fs::read_to_string(&file)?, "data");
        Ok(())
    }

    #[test]
    fn rewrite_atomic_refuses_a_file_that_is_not_utf8() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, [0xff, 0xfe, 0x00])?;

        let result = rewrite_atomic(dir.path(), &path, |text| {
            Ok::<_, io::Error>(text.to_string())
        });

        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
        assert_eq!(fs::read(&path)?, [0xff, 0xfe, 0x00]);
        Ok(())
    }

    #[test]
    fn rewrite_atomic_keeps_the_file_when_edit_fails() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, "original")?;

        let result: Result<(), io::Error> =
            rewrite_atomic(dir.path(), &path, |_| Err(io::Error::other("refused")));

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path)?, "original");
        assert_eq!(entries(dir.path())?, ["ci.yml"]);
        Ok(())
    }

    #[test]
    fn rewrite_atomic_keeps_a_leading_bom() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, "\u{feff}uses: a/b@v1\n")?;

        rewrite_atomic(dir.path(), &path, |text| {
            Ok::<_, io::Error>(text.trim_start_matches(BOM).replace("v1", "v2"))
        })?;
        assert_eq!(fs::read(&path)?, "\u{feff}uses: a/b@v2\n".as_bytes());

        rewrite_atomic(dir.path(), &path, |text| {
            Ok::<_, io::Error>(text.replace("v2", "v3"))
        })?;
        assert_eq!(fs::read(&path)?, "\u{feff}uses: a/b@v3\n".as_bytes());

        fs::write(&path, "uses: a/b@v1\n")?;
        rewrite_atomic(dir.path(), &path, |text| {
            Ok::<_, io::Error>(text.replace("v1", "v2"))
        })?;
        assert_eq!(fs::read(&path)?, b"uses: a/b@v2\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_atomic_refuses_a_symlink_that_leaves_the_root() -> TestResult {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let target = outside.path().join("ci.yml");
        fs::write(&target, "uses: a/b@v1\n")?;
        let link = root.path().join("ci.yml");
        std::os::unix::fs::symlink(&target, &link)?;

        let result = rewrite_atomic(root.path(), &link, |text| {
            Ok::<_, io::Error>(text.replace("v1", "v2"))
        });

        let Err(e) = result else {
            panic!("a write outside the root must fail");
        };
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            e.to_string(),
            format!("resolves outside {}", root.path().display())
        );
        assert_eq!(fs::read_to_string(&target)?, "uses: a/b@v1\n");
        assert_eq!(
            resolve_within(root.path(), root.path())?,
            fs::canonicalize(root.path())?
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_the_mode() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, "old")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;

        write_atomic(&path, b"new")?;

        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o7777, 0o640);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn temp_file_takes_the_mode_before_any_byte_is_written() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("secret.yml");
        fs::write(&path, "old")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

        let mut mode_before_write = None;
        replace(&resolve(&path)?, b"new", |file, bytes| {
            mode_before_write = Some(file.metadata()?.permissions().mode() & 0o7777);
            file.write_all(bytes)
        })?;

        assert_eq!(mode_before_write, Some(0o600));
        assert_eq!(fs::read_to_string(&path)?, "new");
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o7777, 0o600);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn write_through_a_symlink_replaces_its_target() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("real.yml");
        let link = dir.path().join("link.yml");
        fs::write(&target, "old")?;
        std::os::unix::fs::symlink(&target, &link)?;

        write_atomic(&link, b"new")?;

        assert!(fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert_eq!(fs::read_to_string(&target)?, "new");
        assert!(Arc::ptr_eq(
            &lock_for(&resolve(&link)?),
            &lock_for(&resolve(&target)?)
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rewrites_through_a_symlink_and_its_target_take_turns() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("real.yml");
        let link = dir.path().join("link.yml");
        fs::write(&target, "")?;
        std::os::unix::fs::symlink(&target, &link)?;

        let workers: Vec<_> = [target.clone(), link]
            .into_iter()
            .map(|path| {
                let root = dir.path().to_path_buf();
                thread::spawn(move || {
                    (0..50).try_for_each(|_| {
                        rewrite_atomic(&root, &path, |text| Ok::<_, io::Error>(format!("{text}x")))
                    })
                })
            })
            .collect();
        for worker in workers {
            let Ok(result) = worker.join() else {
                panic!("writer thread panicked");
            };
            result?;
        }

        assert_eq!(fs::read_to_string(&target)?.len(), 100);
        Ok(())
    }
}
