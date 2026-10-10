//! The build gate: prove that no build is writing into the target tree, or refuse.
//!
//! Fail-closed by construction. A lock that cannot be probed — unsupported platform,
//! unreadable lock file — is reported as [`Probe::Unavailable`], and `apply` turns
//! that into exit 3 with nothing deleted. There is no `--force`: "I could not check"
//! and "I checked and it is free" must never collapse into the same answer.
//!
//! Two lock families are probed, because either one alone is a false negative:
//!
//! * cargo holds `target/<profile>/.cargo-lock` for the duration of a `cargo build`,
//! * rustc holds its own `<…>/incremental/<crate>-<hash>/s-<h1>-<h2>.lock` while it
//!   compiles that crate — an editor's `cargo check` or a direct `rustc` invocation
//!   can hold *that* one without cargo's, and deleting the session underneath it is
//!   the same mistake.
//!
//! All candidates are probed in this one process. That is not a detail: the reference
//! implementation measured ~500 session locks in a large repository, and starting a
//! process per lock file cost ~15 s — long enough that a caller starts skipping the
//! gate "just this once".

use std::fs;
use std::path::{Path, PathBuf};

// Only the flock-based probe (Unix) needs these.
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io;

/// Outcome of a gate probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Every candidate lock was acquirable, and was released again immediately.
    Free,
    /// At least one candidate is held right now; the list is non-empty.
    Held(Vec<PathBuf>),
    /// The probe could not be performed. Callers that delete must refuse.
    Unavailable(String),
}

impl Probe {
    pub fn is_free(&self) -> bool {
        matches!(self, Probe::Free)
    }

    /// Machine-readable state for `--json`.
    pub fn state(&self) -> &'static str {
        match self {
            Probe::Free => "free",
            Probe::Held(_) => "held",
            Probe::Unavailable(_) => "unavailable",
        }
    }

    /// One line for the human report.
    pub fn describe(&self) -> String {
        match self {
            Probe::Free => "free".to_string(),
            Probe::Held(paths) => {
                let mut out = format!("a build holds {}", paths[0].display());
                if paths.len() > 1 {
                    out.push_str(&format!(" (and {} more)", paths.len() - 1));
                }
                out
            }
            Probe::Unavailable(why) => format!("unavailable: {why}"),
        }
    }
}

/// candidate_locks — every lock file that must be free before anything is deleted.
///
/// Layouts covered: `target/.cargo-lock`, `target/<profile>/.cargo-lock`,
/// `target/<profile>/incremental/<crate>/<session>.lock`, and the same one level
/// deeper for cross-compilation (`target/<triple>/<profile>/…`). Sorted and deduped so
/// the gate message and the tests are deterministic.
pub fn candidate_locks(target_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    let top_lock = target_dir.join(".cargo-lock");
    if top_lock.is_file() {
        out.push(top_lock);
    }

    for level1 in dirs(target_dir) {
        let cargo_lock = level1.join(".cargo-lock");
        if cargo_lock.is_file() {
            out.push(cargo_lock);
        }
        collect_session_locks(&level1, &mut out);
        for level2 in dirs(&level1) {
            let cargo_lock = level2.join(".cargo-lock");
            if cargo_lock.is_file() {
                out.push(cargo_lock);
            }
            collect_session_locks(&level2, &mut out);
        }
    }

    out.sort();
    out.dedup();
    out
}

fn dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    out
}

// `<…>/incremental/<crate>-<hash>/s-<h1>-<h2>.lock`
fn collect_session_locks(profile_dir: &Path, out: &mut Vec<PathBuf>) {
    let incremental = profile_dir.join("incremental");
    for root in dirs(&incremental) {
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().map(|e| e == "lock").unwrap_or(false) {
                out.push(path);
            }
        }
    }
}

/// probe — try to take every candidate exclusively, without blocking.
///
/// Each lock is released immediately after a successful probe, so this is safe to run
/// from a read-only `plan`. Returns [`Probe::Held`] as soon as one candidate resists,
/// so the caller can name the culprit.
#[cfg(unix)]
pub fn probe(candidates: &[PathBuf]) -> Probe {
    let mut held = Vec::new();
    for path in candidates {
        match LockGuard::try_acquire(path) {
            Ok(Some(_guard)) => continue, // acquirable ⇒ free; the guard releases on drop
            Ok(None) => held.push(path.clone()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue, // vanished: nobody holds it
            Err(err) => {
                return Probe::Unavailable(format!("{}: {err}", path.display()));
            }
        }
    }
    if held.is_empty() {
        Probe::Free
    } else {
        Probe::Held(held)
    }
}

/// probe — platforms without `flock` cannot answer the question.
///
/// Windows is the case that matters here: this tool refuses to delete anything while
/// it cannot prove that no build is running, so `apply` exits 3 there and only `plan`
/// is usable. That is a deliberate boundary, not an unfinished port.
#[cfg(not(unix))]
pub fn probe(_candidates: &[PathBuf]) -> Probe {
    Probe::Unavailable(
        "this platform has no flock(2); the gate cannot prove that no build is running, \
         so `apply` refuses (use `plan`, or sweep from a Unix host)"
            .to_string(),
    )
}

/// An exclusive, non-blocking `flock` on one lock file.
///
/// Holding the value keeps the lock; dropping it releases the lock (the fd is closed
/// by `File`'s safe `Drop`). The gate takes and immediately drops one of these, and the
/// test suite uses the same safe API to play the part of a running build.
#[cfg(unix)]
pub struct LockGuard {
    // Owns the descriptor: closing it is what releases the flock, and it needs no
    // unsafe code of its own.
    _file: std::fs::File,
}

#[cfg(unix)]
impl LockGuard {
    /// Try to take `path` exclusively without blocking.
    ///
    /// `Ok(Some(_))` ⇒ the lock was free and is now held by the returned guard.
    /// `Ok(None)` ⇒ somebody else holds it (this is the "a build is running" answer).
    pub fn try_acquire(path: &Path) -> io::Result<Option<LockGuard>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        // SAFETY: this is the only `unsafe` block in the crate.
        // `flock(2)` takes (a) a file descriptor that `File` owns for the whole call —
        // so it cannot be closed or reused underneath us — and (b) two integer
        // operation constants. It has no pointer arguments, cannot touch memory we own,
        // and its return value is checked immediately below. No invariant of Rust is at
        // stake; the alternative (a shell-out to `flock(1)`) does not exist on macOS.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(LockGuard { _file: file }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK => Ok(None),
            _ => Err(err),
        }
    }
}

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cargogc-gate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn candidate_locks_finds_both_families() {
        let dir = scratch("candidates");
        fs::create_dir_all(dir.join("debug/incremental/libfoo-abc")).unwrap();
        fs::create_dir_all(dir.join("release/incremental/libbar-def")).unwrap();
        fs::create_dir_all(dir.join("aarch64-apple-darwin/profiling/incremental/libbaz-ghi"))
            .unwrap();
        fs::write(dir.join("debug/.cargo-lock"), b"").unwrap();
        fs::write(dir.join("debug/incremental/libfoo-abc/s-1-2.lock"), b"").unwrap();
        fs::write(
            dir.join("aarch64-apple-darwin/profiling/incremental/libbaz-ghi/s-3-4.lock"),
            b"",
        )
        .unwrap();
        // Not a lock: the session's own data file.
        fs::write(dir.join("debug/incremental/libfoo-abc/dep-graph.bin"), b"x").unwrap();

        let locks = candidate_locks(&dir);
        let names: Vec<String> = locks
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "aarch64-apple-darwin/profiling/incremental/libbaz-ghi/s-3-4.lock",
                "debug/.cargo-lock",
                "debug/incremental/libfoo-abc/s-1-2.lock",
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_free_then_held_then_free_again() {
        let dir = scratch("probe");
        let lock = dir.join("s-1-2.lock");
        fs::write(&lock, b"").unwrap();

        assert_eq!(probe(std::slice::from_ref(&lock)), Probe::Free);

        let guard = LockGuard::try_acquire(&lock)
            .unwrap()
            .expect("lock should be free");
        assert_eq!(
            probe(std::slice::from_ref(&lock)),
            Probe::Held(vec![lock.clone()])
        );

        drop(guard);
        assert_eq!(probe(std::slice::from_ref(&lock)), Probe::Free);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn probe_skips_vanished_candidates_and_refuses_on_unreadable_ones() {
        let dir = scratch("edge");
        let missing = dir.join("gone.lock");
        assert_eq!(
            probe(std::slice::from_ref(&missing)),
            Probe::Free,
            "a vanished lock cannot be held"
        );

        // A directory where a lock file is expected: open(2) fails with EISDIR, which is
        // not "free" and not "held" — the only honest answer is "cannot tell".
        let weird = dir.join("weird.lock");
        fs::create_dir(&weird).unwrap();
        match probe(std::slice::from_ref(&weird)) {
            Probe::Unavailable(_) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
