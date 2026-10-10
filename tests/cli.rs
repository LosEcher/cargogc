//! End-to-end tests: the real binary, on throwaway fixtures under the system temp
//! directory, asserting exit codes and output.
//!
//! The negative controls matter more than the happy paths here. A sweeper that deletes
//! what it should is nice; a sweeper that keeps its hands off when a build is running is
//! the reason the tool exists. `a_held_*_lock_refuses_and_deletes_nothing` compares a
//! full tree snapshot before and after, so a single removed byte fails the test.
//!
//! The lock in those tests is a real `flock`, taken through the crate's own
//! `gate::LockGuard` — the same safe wrapper the gate uses — so the test plays the part
//! of a running build without a second `unsafe` block anywhere in the repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cargogc::gate::LockGuard;
use cargogc::ledger;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A throwaway `…/target` tree, removed when the test ends.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "cargogc-cli-{}-{name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("target")).unwrap();
        Fixture { root }
    }

    fn target(&self) -> PathBuf {
        self.root.join("target")
    }

    /// A crate root: `target/<profile>/incremental/<crate>-<hash>`.
    fn crate_root(&self, profile: &str, name: &str) -> PathBuf {
        let dir = self.target().join(profile).join("incremental").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A session directory, with its mtime set `age_secs` into the past.
    fn session(&self, root: &Path, name: &str, age_secs: u64) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dep-graph.bin"), vec![9u8; 4096]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        fs::File::open(&dir).unwrap().set_modified(when).unwrap();
        dir
    }

    fn lock(&self, root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, b"").unwrap();
        path
    }

    fn snapshot(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect(&self.root, &self.root, &mut out);
        out.sort();
        out
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            out.push(format!("d {relative}"));
            collect(root, &path, out);
        } else {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(format!("f {relative} {size}"));
        }
    }
}

fn run_in(cwd: Option<&Path>, args: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargogc"));
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .expect("the cargogc binary must be runnable");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run(args: &[&str]) -> (i32, String, String) {
    run_in(None, args)
}

fn target_arg(fixture: &Fixture) -> String {
    fixture.target().to_string_lossy().into_owned()
}

fn json_of(stdout: &str) -> ledger::P {
    ledger::parse(stdout.trim()).expect("the --json report must be one JSON document")
}

// ── P0 acceptance ───────────────────────────────────────────────────────────

/// Acceptance 1: `plan` exits 0 and reports the four accounting numbers.
#[test]
fn plan_reports_the_four_accounting_numbers() {
    let fixture = Fixture::new("four-numbers");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    fixture.session(&root, "s-new-bb-2", 0);

    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for needle in [
        "freed (du, inode-honest)",
        "logical (per path)",
        "shared, still allocated",
        "inflation",
        "gate: free",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in:\n{stdout}");
    }
    assert!(stdout.contains("dry-run: nothing was deleted"));
    assert!(
        stdout.contains("1 items") || stdout.contains("1 item"),
        "{stdout}"
    );
}

/// Acceptance 4 (part): a bare size, a relative timestamp and a stray option are usage
/// errors, not guesses.
#[test]
fn usage_errors_exit_2() {
    let fixture = Fixture::new("usage");
    let target = target_arg(&fixture);

    for args in [
        vec![
            "plan",
            "--target-dir",
            &target,
            "--live-incremental-max",
            "8000",
        ],
        vec![
            "plan",
            "--target-dir",
            &target,
            "--superseded-since",
            "2h ago",
        ],
        vec![
            "plan",
            "--target-dir",
            &target,
            "--superseded-since",
            "2026-10-10T15:50:00",
        ],
        vec!["plan", "--target-dir", &target, "--nope"],
        vec!["plan", "--superseded-since", "1", "--last-build-file", "f"],
        vec!["nonsense"],
        vec![],
        vec!["ledger"],
    ] {
        let (code, _stdout, stderr) = run(&args);
        assert_eq!(
            code, 2,
            "{args:?} should be a usage error, stderr: {stderr}"
        );
    }
}

/// Acceptance 4 (part): no `--target-dir` and no `target/` above the current directory.
#[test]
fn a_missing_target_directory_exits_2_with_a_clear_message() {
    let fixture = Fixture::new("no-target");
    // The fixture itself owns a target/, so remove it: this test is about the *absence*
    // of one in the whole ancestor chain.
    fs::remove_dir_all(fixture.target()).unwrap();
    let empty = fixture.root.join("no-target-here");
    fs::create_dir_all(&empty).unwrap();

    let (code, _stdout, stderr) = run_in(Some(&empty), &["plan", "--superseded-since", "1"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("--target-dir"),
        "message must name the fix: {stderr}"
    );
}

/// `--target-dir` defaults to the `target/` found by walking up from the cwd.
#[test]
fn the_target_directory_is_discovered_upwards() {
    let fixture = Fixture::new("discover");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    fixture.session(&root, "s-new-bb-2", 0);
    let nested = fixture.root.join("src-tauri/nested");
    fs::create_dir_all(&nested).unwrap();

    let (code, stdout, stderr) = run_in(
        Some(&nested),
        &["plan", "--superseded-since", &now().to_string()],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains(&fixture.target().to_string_lossy().into_owned()),
        "the discovered target should be reported: {stdout}"
    );
}

/// Acceptance 2 (fixture-level): the numbers are inode-honest — a file hardlinked from
/// outside the delete set is reported as *not* freed.
#[test]
fn a_hardlink_shared_outside_the_delete_set_is_reported_as_not_freed() {
    let fixture = Fixture::new("hardlink");
    let stale = fixture.crate_root("debug", "libfoo-aaa");
    let live = fixture.crate_root("debug", "libfoo-bbb");
    let session = fixture.session(&stale, "s-old-aa-1", 7200);
    fixture.session(&live, "s-new-bb-2", 0);
    let outside = fixture.root.join("shared-with-elsewhere");
    fs::hard_link(session.join("dep-graph.bin"), &outside).unwrap();

    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let report = json_of(&stdout);
    let totals = report.get("totals").unwrap();
    let freed = totals.get("bytes_du").unwrap().as_u64().unwrap();
    let logical = totals.get("bytes_logical").unwrap().as_u64().unwrap();
    let shared = totals.get("shared_du").unwrap().as_u64().unwrap();
    assert!(
        shared >= 4096,
        "the shared inode must be reported: {stdout}"
    );
    assert!(
        freed + 4096 <= logical,
        "the shared blob must not be counted as freed (freed={freed}, logical={logical})"
    );
    assert!(totals.get("shared_inodes").unwrap().as_u64().unwrap() >= 1);
}

/// Acceptance 6: two roots of one crate name ⇒ the old goes, the newest stays.
#[test]
fn the_newest_root_of_a_crate_name_is_protected() {
    let fixture = Fixture::new("newest-root");
    let old = fixture.crate_root("debug", "libfoo-aaa");
    let new = fixture.crate_root("debug", "libfoo-bbb");
    fixture.session(&old, "s-old-aa-1", 7200);
    fixture.session(&new, "s-new-bb-2", 0);

    let (code, stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!old.exists(), "the superseded root must be gone:\n{stdout}");
    assert!(new.join("s-new-bb-2").is_dir(), "the newest root must stay");
}

/// The same crate name with only one (stale) root: compact, do not delete.
#[test]
fn a_lone_stale_root_is_compacted_not_deleted() {
    let fixture = Fixture::new("compact");
    let root = fixture.crate_root("debug", "libonly-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    fixture.session(&root, "s-new-bb-2", 3600);

    let (code, stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(root.is_dir(), "the newest root must survive:\n{stdout}");
    assert!(
        root.join("s-new-bb-2").is_dir(),
        "its newest session must survive"
    );
    assert!(
        !root.join("s-old-aa-1").exists(),
        "the older session should be dropped"
    );
}

/// Orphan locks go, live locks stay.
#[test]
fn orphan_locks_go_and_live_locks_stay() {
    let fixture = Fixture::new("locks");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-keep-aa-1", 0);
    fixture.session(&root, "s-gone-bb-2", 7200);
    let live_lock = fixture.lock(&root, "s-keep-aa.lock");
    let dead_lock = fixture.lock(&root, "s-gone-bb.lock");
    let orphan_lock = fixture.lock(&root, "s-orphan-cc.lock");

    let (code, stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        live_lock.exists(),
        "a lock whose session survives must stay:\n{stdout}"
    );
    assert!(!dead_lock.exists(), "a lock whose session went must go");
    assert!(!orphan_lock.exists(), "a lock with no session must go");
}

/// Acceptance 3 (negative control): a held rustc session lock ⇒ exit 3, zero deletions.
#[test]
fn a_held_session_lock_refuses_and_deletes_nothing() {
    let fixture = Fixture::new("held-session-lock");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    let lock = fixture.lock(&root, "s-old-aa.lock");

    let before = fixture.snapshot();
    let guard = LockGuard::try_acquire(&lock)
        .expect("opening the lock file must work")
        .expect("the fixture lock is free");

    let (code, stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
    ]);
    drop(guard);

    assert_eq!(
        code, 3,
        "a held lock must refuse: stdout={stdout} stderr={stderr}"
    );
    assert!(stderr.contains("nothing was deleted"), "stderr: {stderr}");
    assert!(
        stdout.contains("gate: a build holds"),
        "the culprit must be named:\n{stdout}"
    );
    assert_eq!(
        before,
        fixture.snapshot(),
        "a refused apply must not remove one byte"
    );
}

/// The same negative control for cargo's own lock, which is a *different* lock file: a
/// gate that only probed session locks (or only cargo's) would pass here and still
/// delete underneath a running build.
#[test]
fn a_held_cargo_lock_refuses_and_deletes_nothing() {
    let fixture = Fixture::new("held-cargo-lock");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    let cargo_lock = fixture.target().join("debug/.cargo-lock");
    fs::write(&cargo_lock, b"").unwrap();

    let before = fixture.snapshot();
    let guard = LockGuard::try_acquire(&cargo_lock).unwrap().expect("free");

    let (code, _stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
    ]);
    drop(guard);

    assert_eq!(code, 3, "stderr: {stderr}");
    assert_eq!(
        before,
        fixture.snapshot(),
        "a refused apply must not remove one byte"
    );
}

/// A refusal is itself recorded, so a hook that ignores the exit code still leaves a trace.
#[test]
fn a_refused_apply_is_recorded_in_the_ledger() {
    let fixture = Fixture::new("refused-ledger");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    let lock = fixture.lock(&root, "s-old-aa.lock");
    let ledger_path = fixture.root.join("runs.jsonl");

    let guard = LockGuard::try_acquire(&lock).unwrap().expect("free");
    let (code, _stdout, _stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
        "--ledger",
        &ledger_path.to_string_lossy(),
    ]);
    drop(guard);

    assert_eq!(code, 3);
    let runs = ledger::view(&ledger_path).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].verdict, "skipped");
}

/// Acceptance 5: applying once leaves nothing for a second run with the same T.
#[test]
fn apply_converges_under_the_same_build_record() {
    let fixture = Fixture::new("convergence");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);
    fixture.session(&root, "s-new-bb-2", 0);

    let since = now().to_string();
    let target = target_arg(&fixture);
    let (code, _stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target,
        "--superseded-since",
        &since,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--superseded-since",
        &since,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("nothing to reclaim"),
        "second run must be empty:\n{stdout}"
    );
}

/// Acceptance (design §5): without a build record the superseded classes do nothing.
#[test]
fn without_a_build_record_nothing_is_planned() {
    let fixture = Fixture::new("no-record");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);

    let before = fixture.snapshot();
    let (code, stdout, stderr) = run(&["plan", "--target-dir", &target_arg(&fixture)]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("no build record"), "{stdout}");
    assert!(stdout.contains("nothing to reclaim"), "{stdout}");

    let (code, _stdout, _stderr) = run(&["apply", "--target-dir", &target_arg(&fixture)]);
    assert_eq!(code, 0);
    assert_eq!(
        before,
        fixture.snapshot(),
        "nothing may be deleted without a record"
    );
}

/// `plan` is read-only, no matter how much it finds.
#[test]
fn plan_never_deletes() {
    let fixture = Fixture::new("plan-readonly");
    let stale = fixture.crate_root("debug", "libfoo-aaa");
    let live = fixture.crate_root("debug", "libfoo-bbb");
    fixture.session(&stale, "s-old-aa-1", 7200);
    fixture.session(&live, "s-new-bb-2", 0);
    fixture.lock(&stale, "s-old-aa.lock");

    let before = fixture.snapshot();
    let (code, _stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
        "--dead-dir",
        &fixture.target().join("debug/incremental").to_string_lossy(),
        "--live-incremental-max",
        "1KiB",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(before, fixture.snapshot(), "plan is read-only");
}

// ── P1 ──────────────────────────────────────────────────────────────────────

/// `--last-build-file`: a record makes the superseded classes work; a missing file is the
/// no-record case (no-op), while an unreadable *content* is a runtime error.
#[test]
fn last_build_file_drives_the_judgement() {
    let fixture = Fixture::new("last-build-file");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    // Two stale sessions: the newest of a lone root is always kept (it is that crate
    // name's fallback), so the plan has to be observed on the older one.
    fixture.session(&root, "s-old-aa-1", 7200);
    fixture.session(&root, "s-older-bb-2", 10_800);
    let record = fixture.root.join("last-build-start.epoch");
    let target = target_arg(&fixture);
    let record_arg = record.to_string_lossy().into_owned();

    // Missing ⇒ no record ⇒ no-op, exit 0.
    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--last-build-file",
        &record_arg,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("no build record at"), "{stdout}");

    // Empty ⇒ still no record.
    fs::write(&record, b"\n").unwrap();
    let (code, stdout, _stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--last-build-file",
        &record_arg,
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("is empty"), "{stdout}");

    // A real record ⇒ the stale session is planned.
    fs::write(&record, format!("{}\n", now())).unwrap();
    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--last-build-file",
        &record_arg,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("s-older-bb-2"), "{stdout}");

    // Garbage ⇒ runtime error, not a guess.
    fs::write(&record, b"not a timestamp").unwrap();
    let (code, _stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--last-build-file",
        &record_arg,
    ]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("build record"), "{stderr}");
}

/// `--json` is one parseable document whose numbers match the human report.
#[test]
fn the_json_report_is_parseable() {
    let fixture = Fixture::new("json");
    let stale = fixture.crate_root("debug", "libfoo-aaa");
    let live = fixture.crate_root("debug", "libfoo-bbb");
    fixture.session(&stale, "s-old-aa-1", 7200);
    fixture.session(&live, "s-new-bb-2", 0);

    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let report = json_of(&stdout);
    assert_eq!(report.get("tool").unwrap().as_str(), Some("cargogc"));
    assert_eq!(report.get("op").unwrap().as_str(), Some("plan"));
    assert_eq!(report.get("verdict").unwrap().as_str(), Some("preview"));
    assert_eq!(
        report.get("gate").unwrap().get("state").unwrap().as_str(),
        Some("free")
    );
    let items = report.get("items").unwrap().as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get("kind").unwrap().as_str(),
        Some("superseded-root")
    );
    assert!(items[0].get("bytes_du").unwrap().as_u64().unwrap() > 0);
}

/// `ledger --view` shows the run, and `--reconcile` reads it back.
#[test]
fn ledger_view_and_reconcile_report_the_run() {
    let fixture = Fixture::new("ledger-view");
    let stale = fixture.crate_root("debug", "libfoo-aaa");
    let live = fixture.crate_root("debug", "libfoo-bbb");
    fixture.session(&stale, "s-old-aa-1", 7200);
    fixture.session(&live, "s-new-bb-2", 0);
    let ledger_path = fixture.root.join("runs.jsonl");
    let ledger_arg = ledger_path.to_string_lossy().into_owned();
    let target = target_arg(&fixture);
    let since = now().to_string();

    let (code, _stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target,
        "--superseded-since",
        &since,
        "--ledger",
        &ledger_arg,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = run(&["ledger", "--view", "--ledger", &ledger_arg]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("applied"), "{stdout}");

    let (code, stdout, stderr) = run(&["ledger", "--view", "--ledger", &ledger_arg, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let runs = json_of(&stdout);
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].get("verdict").unwrap().as_str(), Some("applied"));
    assert!(runs[0].get("bytes_du").unwrap().as_u64().unwrap() > 0);
    assert_eq!(runs[0].get("interrupted").unwrap(), &ledger::P::B(false));

    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--superseded-since",
        &since,
        "--ledger",
        &ledger_arg,
        "--reconcile",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("last apply:"), "{stdout}");
    assert!(
        stdout.contains("converged"),
        "after a full apply nothing is reclaimable:\n{stdout}"
    );
}

/// `--dead-dir` deletes a tree wholesale — the class the caller decides from its manifest.
#[test]
fn a_dead_dir_is_deleted_wholesale() {
    let fixture = Fixture::new("dead-dir");
    let dead = fixture.crate_root("release", "libfoo-aaa");
    fixture.session(&dead, "s-1-1-1", 0);
    fixture.session(&dead, "s-2-2-2", 10);

    let (code, stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--dead-dir",
        &fixture
            .target()
            .join("release/incremental")
            .to_string_lossy(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!dead.exists(), "a dead tree goes whole:\n{stdout}");
}

/// The size cap is opt-in, and it is a cap on the *directory*, not on its sessions.
#[test]
fn the_cap_is_opt_in() {
    let fixture = Fixture::new("cap");
    let root = fixture.crate_root("debug", "libfoo-aaa");
    fixture.session(&root, "s-live-aa-1", 0);

    let target = target_arg(&fixture);
    let (code, stdout, stderr) = run(&["plan", "--target-dir", &target]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("nothing to reclaim"),
        "no cap ⇒ a live session stays"
    );

    let (code, stdout, stderr) = run(&[
        "plan",
        "--target-dir",
        &target,
        "--live-incremental-max",
        "1KiB",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("capped"), "{stdout}");
    assert!(root.is_dir(), "a plan must not delete the capped tree");
}

/// `--keep-newest-roots 0` turns the protection off, which is what a caller that keeps no
/// alternatives wants.
#[test]
fn keeping_zero_roots_disables_the_protection() {
    let fixture = Fixture::new("keep-zero");
    let root = fixture.crate_root("debug", "libonly-aaa");
    fixture.session(&root, "s-old-aa-1", 7200);

    let (code, stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &target_arg(&fixture),
        "--superseded-since",
        &now().to_string(),
        "--keep-newest-roots",
        "0",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        !root.exists(),
        "with no protection the root goes whole:\n{stdout}"
    );
}

/// The report must name the tree it acted on: a hook that logs the output has to be able
/// to prove *which* target directory it swept.
#[test]
fn the_report_names_the_target_it_swept() {
    let fixture = Fixture::new("names-target");
    let (code, stdout, stderr) = run(&["plan", "--target-dir", &target_arg(&fixture)]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains(&fixture.target().to_string_lossy().into_owned()),
        "{stdout}"
    );
}

/// A `--target-dir` that does not exist is a runtime error (1), not a usage error: the
/// option was well-formed, the world was not.
#[test]
fn a_nonexistent_target_dir_is_a_runtime_error() {
    let fixture = Fixture::new("missing-target");
    let missing = fixture.root.join("does-not-exist");
    let (code, _stdout, stderr) = run(&["plan", "--target-dir", &missing.to_string_lossy()]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("not a directory"), "{stderr}");
}
