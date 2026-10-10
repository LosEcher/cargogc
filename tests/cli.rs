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

/// A whole subtree's paths and sizes — the "not one byte changed" instrument.
fn snapshot_of(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect(root, root, &mut out);
    out.sort();
    out
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
    let runs = ledger::view(&ledger_path).unwrap().runs;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].verdict, "skipped");
}

/// A ledger shared with another writer must not read as an empty one.
///
/// Regression: the repo hook wrote its own summary lines into the same file cargogc is given,
/// and `ledger --view` answered "no runs recorded" — true about cargogc's events, false about
/// the ledger, and exactly the silent-no-op shape this tool exists to prevent.
#[test]
fn ledger_view_counts_records_it_did_not_write() {
    let fixture = Fixture::new("foreign-ledger");
    let ledger = fixture.root.join("runs.jsonl");
    let foreign = r#"{"ts":"2026-10-10T15:35:10Z","op":"sweep","target":"/x","superseded":{"items":164,"bytes_du":9176412160},"verdict":"ok"}"#;
    std::fs::write(&ledger, format!("{foreign}\n")).unwrap();

    let (code, stdout, _stderr) = run(&["ledger", "--view", "--ledger", &ledger.to_string_lossy()]);
    assert_eq!(code, 0, "a shared ledger is not an error: {stdout}");
    assert!(
        stdout.contains("ignored: 1 record(s) not written by cargogc"),
        "foreign records must be counted: {stdout}"
    );
    assert!(
        stdout.contains("no cargogc runs recorded"),
        "the empty message must be qualified: {stdout}"
    );
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

// ── batch mode: --scan-root ─────────────────────────────────────────────────
//
// A scan round is judged exactly like a single target (same judge, same accounting, same
// gate); what these tests pin down is the *round*: which directories are found, what
// happens to one that cannot be swept, and how the aggregate is reported. The negative
// control — a locked repository must come out byte-for-byte unchanged while the rest of
// the round proceeds — is the reason the mode exists.

/// Several throwaway repositories under one scan root.
struct ScanFixture {
    root: PathBuf,
}

impl ScanFixture {
    fn new(name: &str) -> ScanFixture {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "cargogc-scan-{}-{name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ScanFixture { root }
    }

    /// A repository with a recognisable target tree; returns its target directory.
    fn repo(&self, repo: &str) -> PathBuf {
        let target = self.root.join(repo).join("target");
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        target
    }

    /// A crate root inside a repository's debug tree.
    fn crate_root(&self, target: &Path, name: &str) -> PathBuf {
        let root = target.join("debug/incremental").join(name);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A session whose mtime is `age_secs` old. 8192 B of content so the accounting
    /// assertions can be about a file this test owns, not about a filesystem's directory
    /// blocks (4096 on ext4, 0 on APFS).
    fn session(&self, root: &Path, name: &str, age_secs: u64) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dep-graph.bin"), vec![5u8; 8192]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        fs::File::open(&dir).unwrap().set_modified(when).unwrap();
        dir
    }

    fn lock(&self, path: &Path) -> PathBuf {
        fs::write(path, b"").unwrap();
        path.to_path_buf()
    }

    fn scan_arg(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    fn snapshot(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect(&self.root, &self.root, &mut out);
        out.sort();
        out
    }
}

impl Drop for ScanFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The round: A has something stale, B is building, C is clean.
fn three_repositories(name: &str) -> (ScanFixture, PathBuf, PathBuf, PathBuf) {
    let fixture = ScanFixture::new(name);
    let a = fixture.repo("repo-a");
    let b = fixture.repo("repo-b");
    let c = fixture.repo("repo-c");

    // A: its newest root is compacted, so the older session goes.
    let a_root = fixture.crate_root(&a, "liba-aaa");
    fixture.session(&a_root, "s-old1-aa-1", 8000);
    fixture.session(&a_root, "s-old2-bb-2", 6000);

    // B: the same shape, but a build holds the repository's lock.
    let b_root = fixture.crate_root(&b, "libb-aaa");
    fixture.session(&b_root, "s-old1-cc-1", 8000);
    fixture.session(&b_root, "s-old2-dd-2", 6000);
    fixture.lock(&b.join("debug/.cargo-lock"));

    // C: everything participated in the last build.
    let c_root = fixture.crate_root(&c, "libc-aaa");
    fixture.session(&c_root, "s-new1-ee-1", 0);

    (fixture, a, b, c)
}

#[test]
fn scan_rounds_over_repositories_and_skips_the_locked_one() {
    let (fixture, _a, b, _c) = three_repositories("rounds");
    let before_b = fs::read_dir(b.join("debug/incremental/libb-aaa"))
        .unwrap()
        .count();
    // Only B's subtree: A is *supposed* to change in this round, B is not.
    let snapshot_b = snapshot_of(&b);
    // One acquisition for the whole test. Re-acquiring after spawning a child is racy and
    // not because of this tool: `flock` lives on the open file description, and a fork
    // duplicates the descriptor — a child that has been forked but not yet exec'd keeps
    // the lock alive even after this process closes its own fd (measured: the second
    // `try_acquire` intermittently returned "held" while tests ran in parallel). The
    // gate never holds a lock across a spawn; only this fixture does.
    let guard = LockGuard::try_acquire(&b.join("debug/.cargo-lock"))
        .unwrap()
        .expect("the fixture lock is free");

    let since = now().to_string();
    let (code, stdout, stderr) = run(&[
        "apply",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &since,
        "--json",
    ]);

    assert_eq!(code, 0, "a batch round is not voided by one locked repo: {stderr}");
    let report = json_of(&stdout);
    assert_eq!(report.get("mode").unwrap().as_str(), Some("scan"));
    let totals = report.get("scan_totals").unwrap();
    assert_eq!(totals.get("targets").unwrap().as_u64(), Some(3));
    assert_eq!(totals.get("applied").unwrap().as_u64(), Some(2));
    assert_eq!(totals.get("skipped").unwrap().as_u64(), Some(1));
    assert_eq!(totals.get("items").unwrap().as_u64(), Some(1), "only A had anything stale");
    assert!(
        totals.get("bytes_du").unwrap().as_u64().unwrap() >= 8192,
        "A's own session file must be in the total: {stdout}"
    );
    let skipped = report.get("skipped").unwrap().as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].as_str().unwrap().contains("repo-b"), "{stdout}");

    assert_eq!(
        snapshot_b,
        snapshot_of(&b),
        "the locked repository must not lose one byte"
    );
    assert_eq!(
        fs::read_dir(b.join("debug/incremental/libb-aaa")).unwrap().count(),
        before_b
    );

    // The human report names the skipped target too — never silently. The same guard is
    // still held: this run is about the report, not a second sweep.
    let (code, stdout, _stderr) = run(&[
        "apply",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &since,
    ]);
    drop(guard);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("skipped targets (nothing was deleted in these):"),
        "{stdout}"
    );
    assert!(
        stdout.contains("not in the round totals above"),
        "a skipped row's numbers must be marked as planned, not freed:\n{stdout}"
    );
    assert!(stdout.contains("*  gate"), "the marker belongs on the skipped row:\n{stdout}");
    assert!(stdout.contains("repo-b"), "{stdout}");
}

#[test]
fn scan_plan_is_read_only_and_says_dead_is_skipped() {
    let (fixture, a, _b, _c) = three_repositories("plan");
    let snapshot = fixture.snapshot();
    let a_root = a.join("debug/incremental/liba-aaa");

    let (code, stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("dead: skipped (scan mode has no manifest)"),
        "scan mode must say the dead class does not participate:\n{stdout}"
    );
    assert!(stdout.contains("dry-run: nothing was deleted"), "{stdout}");
    // A preview must not report "N applied": nothing was applied, and a held gate in a preview
    // is information (the plan for that target is still shown), not a skip.
    assert!(
        stdout.contains("would be swept"),
        "preview summary must not claim anything was applied:\n{stdout}"
    );
    assert!(
        !stdout.contains("target(s)") || !stdout.contains(" applied,"),
        "preview summary must not say 'applied':\n{stdout}"
    );
    assert!(a_root.join("s-old1-aa-1").is_dir(), "plan must not delete");
    assert_eq!(snapshot, fixture.snapshot(), "plan is read-only");
}

/// A preview and an apply must agree on the round total while the same target is gated.
///
/// Regression: the preview counted the locked target's plan into `scan_totals`, so with one
/// locked repo it promised "3 items" where apply deleted 2 — the overstatement the four
/// numbers exist to prevent. A gated target is information in a preview (its row is shown and
/// marked) but it contributes nothing to the total.
#[test]
fn scan_preview_and_apply_agree_on_the_total_when_a_target_is_gated() {
    let (fixture, _a, b, _c) = three_repositories("gated-total");
    let guard = LockGuard::try_acquire(&b.join("debug/.cargo-lock"))
        .unwrap()
        .expect("the fixture lock is free");
    let since = now().to_string();

    let (code, stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &since,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let preview = json_of(&stdout);
    let preview_totals = preview.get("scan_totals").unwrap();
    assert_eq!(
        preview_totals.get("items").unwrap().as_u64(),
        Some(1),
        "a gated target must not contribute to the preview total: {stdout}"
    );
    assert_eq!(
        preview_totals.get("skipped").unwrap().as_u64(),
        Some(0),
        "a preview skips nothing; the gate is information"
    );
    let gated = preview
        .get("targets")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t.get("gate").unwrap().get("state").unwrap().as_str() != Some("free"))
        .count();
    assert_eq!(gated, 1, "the held target must still be visible: {stdout}");

    // The lock stays held across the apply run too: the invariant is "while the same target is
    // gated, preview and apply agree on what comes back". Releasing it first would legitimately
    // let apply reclaim that target as well, which is a different statement.
    let (code, stdout, stderr) = run(&[
        "apply",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &since,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let applied = json_of(&stdout);
    let applied_totals = applied.get("scan_totals").unwrap();
    assert_eq!(
        applied_totals.get("items").unwrap().as_u64(),
        Some(1),
        "apply must delete what the preview promised: {stdout}"
    );
    assert_eq!(
        applied_totals.get("skipped").unwrap().as_u64(),
        Some(1),
        "the held target is skipped by apply: {stdout}"
    );
    drop(guard);
}

#[test]
fn scan_finds_a_target_four_levels_down_and_not_a_nested_one() {
    let fixture = ScanFixture::new("depth");
    let deep = fixture.root.join("a/b/c/target");
    fs::create_dir_all(deep.join("debug/deps")).unwrap();
    let deep_root = fixture.crate_root(&deep, "libdeep-aaa");
    fixture.session(&deep_root, "s-old1-aa-1", 8000);
    fixture.session(&deep_root, "s-old2-bb-2", 6000);
    // A second target tree *inside* the first one's profile directory: the walk must stop
    // at a recognised target, or this repository would be counted twice.
    let nested = deep.join("debug/nested/target");
    fs::create_dir_all(nested.join("debug/deps")).unwrap();
    let nested_root = fixture.crate_root(&nested, "libnested-aaa");
    fixture.session(&nested_root, "s-old1-cc-1", 8000);
    fixture.session(&nested_root, "s-old2-dd-2", 6000);

    let (code, stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &now().to_string(),
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let report = json_of(&stdout);
    let targets = report.get("targets").unwrap().as_array().unwrap();
    assert_eq!(targets.len(), 1, "exactly one target: {stdout}");
    assert!(targets[0]
        .get("target")
        .unwrap()
        .as_str()
        .unwrap()
        .ends_with("a/b/c/target"));
    assert!(!stdout.contains("nested"), "the nested target must not be reported");
}

#[test]
fn scan_keeps_the_newest_root_per_crate_in_every_repository() {
    let fixture = ScanFixture::new("newest-root");
    let repo = fixture.repo("repo");
    let old = fixture.crate_root(&repo, "libsame-aaa");
    let new = fixture.crate_root(&repo, "libsame-bbb");
    fixture.session(&old, "s-old1-aa-1", 8000);
    fixture.session(&new, "s-new1-bb-1", 0);

    let (code, _stdout, stderr) = run(&[
        "apply",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!old.exists(), "the superseded root goes");
    assert!(new.join("s-new1-bb-1").is_dir(), "the newest root stays");
}

#[test]
fn scan_deduplicates_overlapping_roots() {
    let fixture = ScanFixture::new("overlap");
    let repo = fixture.repo("repo");
    let root = fixture.crate_root(&repo, "libx-aaa");
    fixture.session(&root, "s-old1-aa-1", 8000);
    fixture.session(&root, "s-old2-bb-2", 6000);

    let (code, stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &fixture.scan_arg(),
        "--scan-root",
        &fixture.root.join("repo").to_string_lossy(),
        "--superseded-since",
        &now().to_string(),
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let report = json_of(&stdout);
    assert_eq!(
        report
            .get("scan_totals")
            .unwrap()
            .get("targets")
            .unwrap()
            .as_u64(),
        Some(1),
        "an overlapping root must not sweep the same target twice: {stdout}"
    );
}

#[test]
fn scan_rejects_conflicting_modes_and_missing_roots() {
    let fixture = ScanFixture::new("usage");
    let target = fixture.repo("repo");
    let scan = fixture.scan_arg();

    // Both modes at once.
    let (code, _stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &scan,
        "--target-dir",
        &target.to_string_lossy(),
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("two different modes"), "{stderr}");

    // `--dead-dir` is one repository's knowledge; a scan has no manifest to derive it from.
    let (code, _stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &scan,
        "--dead-dir",
        &target.join("release/incremental").to_string_lossy(),
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--dead-dir"), "{stderr}");

    // A scan root that does not exist is a usage error, not an empty round.
    let (code, _stdout, stderr) = run(&[
        "plan",
        "--scan-root",
        &fixture.root.join("nope").to_string_lossy(),
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--scan-root is not a directory"), "{stderr}");
}

#[test]
fn scan_with_nothing_to_find_is_an_empty_round_not_an_error() {
    let fixture = ScanFixture::new("empty");
    fs::create_dir_all(fixture.root.join("docs")).unwrap();
    let (code, stdout, stderr) = run(&["plan", "--scan-root", &fixture.scan_arg()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("no cargo target directory found"), "{stdout}");
    assert!(stdout.contains("nothing to reclaim"), "{stdout}");
}

/// The probe cannot answer (here: the lock file is unreadable) ⇒ the target is skipped and
/// named. Treating "I could not check" as "free" is the one thing the gate must never do,
/// and scan mode keeps that rule per target.
#[cfg(unix)]
#[test]
fn scan_skips_a_target_whose_gate_cannot_be_probed() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = ScanFixture::new("unprobeable");
    let repo = fixture.repo("repo");
    let root = fixture.crate_root(&repo, "libu-aaa");
    fixture.session(&root, "s-old1-aa-1", 8000);
    fixture.session(&root, "s-old2-bb-2", 6000);
    let lock = fixture.lock(&repo.join("debug/.cargo-lock"));
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();

    // Self-check: if this process ignores file modes (a root shell), the gate legitimately
    // reports "free" and this test has nothing to assert.
    let probe = cargogc::gate::probe(std::slice::from_ref(&lock));
    if probe.is_free() {
        eprintln!(
            "note: file modes are not enforced for this user, so the unprobeable-lock path \
             cannot be exercised here; the gate unit test covers it directly"
        );
        return;
    }

    let snapshot = fixture.snapshot();
    let (code, stdout, stderr) = run(&[
        "apply",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &now().to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("unavailable") || stdout.contains("cannot"),
        "the reason must be named:\n{stdout}"
    );
    assert_eq!(snapshot, fixture.snapshot(), "an unprobeable target must not be touched");
    assert!(root.join("s-old1-aa-1").is_dir());
}

#[test]
fn scan_json_names_every_target_and_writes_one_ledger_run() {
    let (fixture, a, b, _c) = three_repositories("ledger");
    let ledger_path = fixture.root.join("runs.jsonl");
    let ledger_arg = ledger_path.to_string_lossy().into_owned();

    // Hold the repository lock for the round, so B is skipped and the scan run has to
    // carry both an applied and a skipped target.
    let _ = &b;
    let guard = LockGuard::try_acquire(&b.join("debug/.cargo-lock"))
        .unwrap()
        .expect("the fixture lock is free");

    // A single-target run first, so the view has to show both shapes at once.
    let (code, _stdout, stderr) = run(&[
        "apply",
        "--target-dir",
        &a.to_string_lossy(),
        "--superseded-since",
        &now().to_string(),
        "--ledger",
        &ledger_arg,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = run(&[
        "apply",
        "--scan-root",
        &fixture.scan_arg(),
        "--superseded-since",
        &now().to_string(),
        "--ledger",
        &ledger_arg,
        "--json",
    ]);
    drop(guard);
    assert_eq!(code, 0, "stderr: {stderr}");
    let report = json_of(&stdout);
    let targets = report.get("targets").unwrap().as_array().unwrap();
    assert_eq!(targets.len(), 3, "every discovered target is named: {stdout}");
    assert!(targets
        .iter()
        .any(|target| target.get("skipped").is_some()));
    assert!(targets
        .iter()
        .all(|target| target.get("classes").is_some()));
    assert_eq!(
        report.get("dead").unwrap().as_str(),
        Some("skipped (scan mode has no manifest)")
    );

    // One scan run, and the single-target run beside it.
    let runs = ledger::view(&ledger_path).unwrap();
    assert_eq!(runs.runs.len(), 2, "one run per round, not one per target");
    let single = runs.runs.iter().find(|run| run.mode == "single").unwrap();
    assert_eq!(single.items, 1);
    let scan = runs.runs.iter().find(|run| run.mode == "scan").unwrap();
    assert_eq!(scan.targets_count, 3);
    assert_eq!(scan.skipped_count, 1);
    assert_eq!(scan.verdict, "applied");

    // A shared ledger keeps its discipline: other writers' records are counted, not shown.
    let mut extra = fs::read_to_string(&ledger_path).unwrap();
    extra.push_str("{\"op\":\"sweep\",\"verdict\":\"ok\"}\n");
    fs::write(&ledger_path, extra).unwrap();
    let (code, stdout, stderr) = run(&["ledger", "--view", "--ledger", &ledger_arg]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("1 record(s) not written by cargogc"), "{stdout}");
    assert!(stdout.contains("3 targets (1 skipped)"), "a scan row shows its breadth:\n{stdout}");
    assert!(stdout.contains("target "), "a single row still names its target:\n{stdout}");
}
