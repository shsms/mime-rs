//! Path allowlisting + an append-only audit journal — the M5 "safer than shell"
//! seam.
//!
//! mime-rs exposes no shell, no process spawn, and no network; the only ambient
//! authority it has is the filesystem, through the two buffer-file call sites
//! (`open_file` / `save_buffer`). [`check_path`] is the chokepoint that confines
//! those to a set of allowed roots so an autonomous agent cannot read or write
//! outside the workspace it was granted. [`audit`] records one JSON line per
//! program run when `$MIME_AUDIT` names a log file, giving the "what did the
//! agent change, and why" replay trail.
use std::path::{Path, PathBuf};

/// The allowed filesystem roots: the colon-separated absolute paths in
/// `$MIME_ROOTS`, or the current working directory when that is unset or empty.
///
/// This is the SAME set [`check_path`] enforces, exposed so callers (the daemon
/// `status` op, the MCP `session_status` tool) can advertise the sandbox to an
/// agent up front instead of leaving it to discover the bounds by a rejected
/// write.
///
/// Each entry is canonicalized (symlinks + `.`/`..` resolved) so the containment
/// check in [`check_path`] compares real paths against real paths. Entries that
/// don't exist or can't be canonicalized are skipped — a misconfigured root
/// simply doesn't grant access rather than silently widening it.
pub fn roots() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    parse_roots(std::env::var("MIME_ROOTS").ok(), &cwd)
}

/// Pure core of [`roots`]: resolve the raw `$MIME_ROOTS` value (already read from
/// the environment) against `cwd`. Split out so it can be unit-tested without the
/// process-global env mutation that makes [`roots`] itself racy under the test
/// harness.
///
/// `raw` unset or whitespace-only => the single canonicalized `cwd` (default-deny
/// everything outside the working directory). Otherwise each colon-separated,
/// trimmed, non-empty entry is canonicalized; ones that can't be are dropped.
fn parse_roots(raw: Option<String>, cwd: &Path) -> Vec<PathBuf> {
    let raw = raw.unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        // Default-deny everything *outside* the cwd: the process's own working
        // directory is the implicit single root.
        return cwd.canonicalize().into_iter().collect();
    }
    raw.split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| Path::new(s).canonicalize().ok())
        .collect()
}

/// Whether the audit journal is active, i.e. `$MIME_AUDIT` is set (to any value,
/// including empty). Lets the status surfaces report whether runs are being
/// recorded, without exposing the log path.
pub fn audit_enabled() -> bool {
    std::env::var_os("MIME_AUDIT").is_some()
}

/// Verify `path` resolves to a location inside one of the [`roots`] and
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
    let roots = roots();
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

/// One audit record, appended as a single JSON line to `$MIME_AUDIT` per
/// program run. Best-effort: a log failure is reported on stderr but never
/// fails the run (the edit already happened; losing the journal line must not
/// look like the edit failed).
///
/// `client` distinguishes the daemon from the MCP server; `session` and
/// `program` identify the run; `dirty`/`len_before`/`len_after` summarize its
/// effect so the journal is useful without re-reading the buffer.
pub fn audit(
    client: &str,
    session: &str,
    program: &str,
    dirty: bool,
    len_before: usize,
    len_after: usize,
) {
    let Some(path) = std::env::var_os("MIME_AUDIT") else {
        return;
    };
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = serde_json::json!({
        "time": time,
        "client": client,
        "session": session,
        "program": program,
        "dirty": dirty,
        "len_before": len_before,
        "len_after": len_after,
    });
    if let Err(e) = append_line(Path::new(&path), &line.to_string()) {
        eprintln!(
            "mime-rs: audit log write to {} failed: {e}",
            path.to_string_lossy()
        );
    }
}

/// Append one line (plus a trailing newline) to the audit file, creating it if
/// absent. Opened in append mode every call — simple and robust to the file
/// being rotated underneath us; this is not a hot path.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
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

    // `parse_roots` is the pure core — no env, no lock needed. These exercise the
    // parsing/canonicalization directly, which `roots()` only wraps.

    #[test]
    fn parse_roots_uses_mime_roots_when_set() {
        let root = temp_root();
        let raw = root.display().to_string();
        let roots = parse_roots(Some(raw), Path::new("/nonexistent-cwd-should-be-ignored"));
        assert_eq!(roots, vec![root]);
    }

    #[test]
    fn parse_roots_splits_multiple_entries() {
        let a = temp_root();
        let b = {
            let mut p = a.clone();
            p.push("second");
            std::fs::create_dir_all(&p).unwrap();
            p.canonicalize().unwrap()
        };
        let raw = format!("{}:{}", a.display(), b.display());
        let roots = parse_roots(Some(raw), Path::new("/ignored"));
        assert_eq!(roots, vec![a, b]);
    }

    #[test]
    fn parse_roots_skips_nonexistent_entries() {
        let root = temp_root();
        // A bogus path can't be canonicalized and is dropped, leaving the real one.
        let raw = format!("/no/such/dir/anywhere:{}", root.display());
        let roots = parse_roots(Some(raw), Path::new("/ignored"));
        assert_eq!(roots, vec![root]);
    }

    #[test]
    fn parse_roots_defaults_to_cwd_when_none() {
        let cwd = temp_root();
        let roots = parse_roots(None, &cwd);
        assert_eq!(roots, vec![cwd.canonicalize().unwrap()]);
    }

    #[test]
    fn parse_roots_defaults_to_cwd_when_blank() {
        let cwd = temp_root();
        // Whitespace-only is treated as unset.
        let roots = parse_roots(Some("   ".to_string()), &cwd);
        assert_eq!(roots, vec![cwd.canonicalize().unwrap()]);
    }

    #[test]
    fn roots_matches_check_path_default() {
        let _g = lock();
        unsafe { std::env::remove_var("MIME_ROOTS") };
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        // The wrapper reads the (now-unset) env and falls back to the cwd, the
        // same root check_path enforces.
        assert_eq!(roots(), vec![cwd]);
    }

    #[test]
    fn audit_enabled_tracks_env() {
        let _g = lock();
        unsafe { std::env::set_var("MIME_AUDIT", "/tmp/whatever.log") };
        assert!(audit_enabled());
        unsafe { std::env::remove_var("MIME_AUDIT") };
        assert!(!audit_enabled());
    }
}
