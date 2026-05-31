//! Path allowlisting — the M5 "safer than shell" seam.
//!
//! mime-rs exposes no shell, no process spawn, and no network; the only ambient
//! authority it has is the filesystem, through the two buffer-file call sites
//! (`open_file` / `save_buffer`). [`check_path`] is the chokepoint that confines
//! those to a set of allowed roots so an autonomous agent cannot read or write
//! outside the workspace it was granted.
use std::path::{Path, PathBuf};

/// The allowed filesystem roots: the colon-separated absolute paths in
/// `$MIME_ROOTS`, or the current working directory when that is unset or empty.
///
/// Each entry is canonicalized (symlinks + `.`/`..` resolved) so the containment
/// check in [`check_path`] compares real paths against real paths. Entries that
/// don't exist or can't be canonicalized are skipped — a misconfigured root
/// simply doesn't grant access rather than silently widening it.
fn allowed_roots() -> Vec<PathBuf> {
    let raw = std::env::var("MIME_ROOTS").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        // Default-deny everything *outside* the cwd: the process's own working
        // directory is the implicit single root.
        return std::env::current_dir()
            .and_then(|d| d.canonicalize())
            .into_iter()
            .collect();
    }
    raw.split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| Path::new(s).canonicalize().ok())
        .collect()
}

/// Verify `path` resolves to a location inside one of the [`allowed_roots`] and
/// return its canonical form; otherwise return a human-readable `Err`.
///
/// Semantics:
/// * **Existing path** — canonicalized directly (resolving symlinks and `..`),
///   so a symlink that points outside the roots is rejected by its real target.
/// * **Not-yet-existing path** (a save to a new file) — its *parent directory*
///   is canonicalized and the final component re-joined. Creating a new file
///   under an allowed root is therefore permitted, while `../escape` or an
///   absolute path elsewhere still resolves outside the roots and is rejected.
///   (The parent must already exist; we never create directories here.)
///
/// The returned `PathBuf` is the canonical path the caller should actually open
/// or write, so the resolved location and the checked location are the same.
pub fn check_path(path: &Path) -> Result<PathBuf, String> {
    let roots = allowed_roots();
    if roots.is_empty() {
        return Err("no allowed roots configured (set $MIME_ROOTS to absolute paths)".to_string());
    }
    let canonical = canonicalize_target(path)?;
    if roots.iter().any(|root| canonical.starts_with(root)) {
        Ok(canonical)
    } else {
        let roots = roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "path {} is outside the allowed roots ({roots})",
            path.display()
        ))
    }
}

/// Canonicalize `path` whether or not it exists yet. An existing path is
/// canonicalized directly; for a missing one we canonicalize the parent (which
/// must exist) and re-attach the file name, so a new file under a real,
/// in-bounds directory resolves to a real, in-bounds path.
fn canonicalize_target(path: &Path) -> Result<PathBuf, String> {
    if let Ok(real) = path.canonicalize() {
        return Ok(real);
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let name = path.file_name();
    match (parent, name) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(real_parent) => Ok(real_parent.join(name)),
            Err(e) => Err(format!(
                "cannot resolve parent directory of {}: {e}",
                path.display()
            )),
        },
        // No parent (a bare relative name) — resolve it against the cwd.
        (None, Some(name)) => match std::env::current_dir().and_then(|d| d.canonicalize()) {
            Ok(cwd) => Ok(cwd.join(name)),
            Err(e) => Err(format!("cannot resolve current directory: {e}")),
        },
        _ => Err(format!("cannot resolve path {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // `check_path` reads process-global env (`MIME_ROOTS`); serialize the tests
    // that mutate it so they don't race under the multi-threaded test harness.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A unique temp dir we can use as an allowed root, canonicalized so
    /// comparisons match `check_path`'s own canonicalization (macOS/CI often put
    /// the temp dir behind a symlink, e.g. /tmp -> /private/tmp).
    fn temp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mime-safety-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    #[test]
    fn accepts_existing_file_under_root() {
        let _g = lock();
        let root = temp_root();
        let file = root.join("note.txt");
        std::fs::write(&file, "hi").unwrap();
        // SAFETY: serialized by ENV_LOCK; single-threaded within the test.
        unsafe { std::env::set_var("MIME_ROOTS", &root) };

        let resolved = check_path(&file).expect("file under root is allowed");
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn accepts_not_yet_existing_file_under_root() {
        let _g = lock();
        let root = temp_root();
        let new_file = root.join("does-not-exist-yet.txt");
        assert!(!new_file.exists());
        unsafe { std::env::set_var("MIME_ROOTS", &root) };

        // A save to a new file under the root must be permitted, and resolve to
        // the canonical path under the (real) root.
        let resolved = check_path(&new_file).expect("new file under root is allowed");
        assert_eq!(resolved, root.join("does-not-exist-yet.txt"));
    }

    #[test]
    fn rejects_dotdot_escape() {
        let _g = lock();
        let root = temp_root();
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        unsafe { std::env::set_var("MIME_ROOTS", &inner) };

        // ../ climbs out of the (inner) root back to its parent — rejected.
        let escape = inner.join("..").join("outside.txt");
        let err = check_path(&escape).expect_err("dotdot escape must be rejected");
        assert!(err.contains("outside the allowed roots"), "got: {err}");
    }

    #[test]
    fn rejects_absolute_path_outside_roots() {
        let _g = lock();
        let root = temp_root();
        unsafe { std::env::set_var("MIME_ROOTS", &root) };

        let err = check_path(Path::new("/etc/passwd"))
            .expect_err("/etc/passwd must be rejected when root is a temp dir");
        assert!(err.contains("outside the allowed roots"), "got: {err}");
    }

    #[test]
    fn defaults_to_cwd_when_unset() {
        let _g = lock();
        unsafe { std::env::remove_var("MIME_ROOTS") };
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        // A path under the cwd is allowed by the default root...
        let under = cwd.join("Cargo.toml");
        assert!(
            check_path(&under).is_ok(),
            "cwd/Cargo.toml should be allowed"
        );
        // ...and an absolute path well outside it is still rejected.
        assert!(check_path(Path::new("/etc/passwd")).is_err());
    }
}
