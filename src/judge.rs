//! The judgement: which paths may be deleted, and on what mechanical grounds.
//!
//! Four classes, each with a criterion that can be checked by a machine and a cost that
//! can be stated in one line:
//!
//! | class | criterion | cost |
//! |---|---|---|
//! | `dead` | the caller declared the tree unreadable by cargo (`--dead-dir`) | `cargo build` |
//! | `superseded-root` | no session in that crate root is at/after `T` | one recompile of that crate |
//! | `superseded-session` | a session older than `T` inside a root that *is* live | none worth naming |
//! | `superseded-lock` | a `s-<h1>-<h2>.lock` whose sessions are all gone | none (rustc recreates it) |
//! | `capped` | a live incremental directory at or over `--live-incremental-max` | a full rebuild of it |
//!
//! `T` is a *build record*, not an age. That distinction is the whole reason this tool
//! exists: on the tree this was measured against, an age gate ("same crate, several
//! hashes, none touched for 7 days") reclaimed **0 B**, while "no build has touched it
//! since the last build started" reclaimed **8.55 GiB**. An age carries no information
//! in a repository that is built all day.
//!
//! Protection: the newest `--keep-newest-roots` roots of every *crate name* are kept
//! (two hashes of one crate name can be two live feature configurations, so dropping
//! the second one costs a recompile every time a build alternates between them). The
//! newest root is compacted to its newest session rather than deleted.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What a planned deletion is, and therefore what it costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Dead,
    SupersededRoot,
    SupersededSession,
    SupersededLock,
    Capped,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Dead => "dead",
            Kind::SupersededRoot => "superseded-root",
            Kind::SupersededSession => "superseded-session",
            Kind::SupersededLock => "superseded-lock",
            Kind::Capped => "capped",
        }
    }

    pub const ALL: [Kind; 5] = [
        Kind::Dead,
        Kind::SupersededRoot,
        Kind::SupersededSession,
        Kind::SupersededLock,
        Kind::Capped,
    ];
}

/// One path the plan wants gone, with the reason a reader can check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub kind: Kind,
    pub path: PathBuf,
    pub reason: String,
}

/// Inputs to the judgement. Everything the tool needs, and nothing it has to guess.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// The cargo target directory (`…/target`), not the repository root.
    pub target_dir: PathBuf,
    /// The build record, in unix seconds. `None` means "there is no record", and the
    /// superseded classes then do nothing at all — guessing T would delete the sessions
    /// the next build is about to reuse.
    pub since: Option<i64>,
    /// Opt-in size cap for live incremental directories.
    pub cap: Option<u64>,
    /// How many of the newest roots per crate name to protect. `0` disables protection.
    pub keep_newest_roots: usize,
    /// Trees the caller declares dead (profiles whose `incremental` is off).
    pub dead_dirs: Vec<PathBuf>,
}

impl Inputs {
    pub fn new(target_dir: PathBuf) -> Inputs {
        Inputs {
            target_dir,
            since: None,
            cap: None,
            keep_newest_roots: 1,
            dead_dirs: Vec::new(),
        }
    }
}

/// plan — the judgement. Read-only: it never creates, moves or deletes anything.
pub fn plan(input: &Inputs) -> io::Result<Vec<PlanItem>> {
    let mut items: Vec<PlanItem> = Vec::new();

    for dead in &input.dead_dirs {
        if dead.is_dir() {
            items.push(PlanItem {
                kind: Kind::Dead,
                path: dead.clone(),
                reason: "declared dead with --dead-dir".to_string(),
            });
        }
    }

    for incremental in discover_incremental_dirs(&input.target_dir) {
        if input
            .dead_dirs
            .iter()
            .any(|dead| incremental.starts_with(dead))
        {
            continue; // already going away, wholesale
        }

        if let Some(cap) = input.cap {
            let du = crate::account::du_of(&incremental)?;
            if du >= cap {
                items.push(PlanItem {
                    kind: Kind::Capped,
                    path: incremental.clone(),
                    reason: format!(
                        "live incremental is {} ({} B), at or over the cap",
                        crate::account::human(du),
                        du
                    ),
                });
                continue; // the whole directory goes; nothing inside it needs judging
            }
        }

        if let Some(since) = input.since {
            judge_superseded(&incremental, since, input.keep_newest_roots, &mut items)?;
        }
    }

    Ok(finish(items))
}

/// judge_superseded — the build-record judgement for one `<profile>/incremental` tree.
fn judge_superseded(
    incremental: &Path,
    since: i64,
    keep_newest_roots: usize,
    items: &mut Vec<PlanItem>,
) -> io::Result<()> {
    let mut roots: Vec<Root> = Vec::new();
    for entry in read_dir_sorted(incremental)? {
        if !entry.is_dir() {
            continue;
        }
        let mut sessions: Vec<Session> = Vec::new();
        for child in read_dir_sorted(&entry)? {
            if !child.is_dir() {
                continue;
            }
            let name = file_name(&child);
            if !name.starts_with("s-") {
                continue;
            }
            let mtime = mtime_secs(&child);
            sessions.push(Session {
                path: child,
                name,
                mtime,
            });
        }
        sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.name.cmp(&b.name)));
        let newest = sessions
            .first()
            .map(|s| s.mtime)
            .unwrap_or_else(|| mtime_secs(&entry));
        roots.push(Root {
            name: file_name(&entry),
            path: entry,
            crate_name: String::new(),
            sessions,
            newest,
        });
    }

    // The root name is `<crate>-<metadata hash>`; only the hash is stripped, because two
    // hashes of one crate name can be two live feature configurations.
    for root in &mut roots {
        root.crate_name = root
            .name
            .rsplit_once('-')
            .map(|(c, _)| c.to_string())
            .unwrap_or_else(|| root.name.clone());
    }

    // The newest `keep_newest_roots` roots of each crate name are protected.
    let mut newest_by_crate: HashMap<String, Vec<&Root>> = HashMap::new();
    for root in &roots {
        newest_by_crate
            .entry(root.crate_name.clone())
            .or_default()
            .push(root);
    }
    let mut protected: Vec<PathBuf> = Vec::new();
    for (_, mut group) in newest_by_crate {
        group.sort_by(|a, b| b.newest.cmp(&a.newest).then_with(|| a.name.cmp(&b.name)));
        for root in group.into_iter().take(keep_newest_roots) {
            protected.push(root.path.clone());
        }
    }

    let mut doomed_paths: Vec<PathBuf> = Vec::new();
    for root in &roots {
        let fresh: Vec<&Session> = root.sessions.iter().filter(|s| s.mtime >= since).collect();
        if !fresh.is_empty() {
            for session in root.sessions.iter().filter(|s| s.mtime < since) {
                items.push(PlanItem {
                    kind: Kind::SupersededSession,
                    path: session.path.clone(),
                    reason: "no build has touched this session since T".to_string(),
                });
                doomed_paths.push(session.path.clone());
            }
            continue;
        }

        let is_protected = protected.contains(&root.path);
        if is_protected {
            // Nothing in this root took part in the build, but it is this crate name's
            // newest root: keep its newest session as the fallback for that feature
            // configuration, drop the rest.
            for session in root.sessions.iter().skip(1) {
                items.push(PlanItem {
                    kind: Kind::SupersededSession,
                    path: session.path.clone(),
                    reason: "crate's newest root: kept the newest session, dropped this one"
                        .to_string(),
                });
                doomed_paths.push(session.path.clone());
            }
            continue;
        }

        items.push(PlanItem {
            kind: Kind::SupersededRoot,
            path: root.path.clone(),
            reason: "no session at or after T: this root did not take part in the build"
                .to_string(),
        });
        doomed_paths.push(root.path.clone());
    }

    // Orphan locks: a lock guards the sessions with its `s-<h1>-<h2>` prefix. Once those
    // are gone the lock guards nothing and nothing else ever removes it.
    for root in &roots {
        let entries = match fs::read_dir(&root.path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = file_name(&path);
            let Some(prefix) = name.strip_suffix(".lock") else {
                continue;
            };
            let prefix = format!("{prefix}-");
            let survivors = root
                .sessions
                .iter()
                .filter(|s| s.name.starts_with(&prefix) && !doomed_paths.contains(&s.path))
                .count();
            if survivors == 0 {
                items.push(PlanItem {
                    kind: Kind::SupersededLock,
                    path,
                    reason: "no session left to guard".to_string(),
                });
            }
        }
    }

    Ok(())
}

struct Root {
    path: PathBuf,
    name: String,
    crate_name: String,
    sessions: Vec<Session>,
    newest: i64,
}

struct Session {
    path: PathBuf,
    name: String,
    mtime: i64,
}

/// discover_incremental_dirs — every `<target>/…/incremental` tree, one and two levels
/// down (the second level is the cross-compilation layout `target/<triple>/<profile>`).
pub fn discover_incremental_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !target_dir.is_dir() {
        return out;
    }
    for level1 in read_dir_sorted(target_dir).unwrap_or_default() {
        if !level1.is_dir() {
            continue;
        }
        let direct = level1.join("incremental");
        if direct.is_dir() {
            out.push(direct);
            continue;
        }
        // Cross-compilation: target/<triple>/<profile>/incremental.
        for level2 in read_dir_sorted(&level1).unwrap_or_default() {
            if !level2.is_dir() {
                continue;
            }
            let nested = level2.join("incremental");
            if nested.is_dir() {
                out.push(nested);
            }
        }
    }
    out.sort();
    out
}

fn finish(mut items: Vec<PlanItem>) -> Vec<PlanItem> {
    // Drop anything nested inside another planned deletion (a whole root carries its own
    // sessions and locks), then present the plan in a stable order.
    items.sort_by(|a, b| a.path.cmp(&b.path));
    items.dedup_by(|a, b| a.path == b.path);

    let mut kept: Vec<PlanItem> = Vec::new();
    for item in items {
        if kept
            .iter()
            .any(|k| k.path != item.path && item.path.starts_with(&k.path))
        {
            continue;
        }
        kept.push(item);
    }
    kept.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.path.cmp(&b.path)));
    kept
}

fn read_dir_sorted(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in entries.flatten() {
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn mtime_secs(path: &Path) -> i64 {
    fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(system_time_secs)
        .unwrap_or(0)
}

/// system_time_secs — unix seconds, 0 before the epoch (never a panic).
pub fn system_time_secs(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => Some(delta.as_secs() as i64),
        Err(_) => Some(0),
    }
}

// ── batch discovery (`--scan-root`) ─────────────────────────────────────────

/// SCAN_MAX_DEPTH — how many levels below a `--scan-root` the discovery descends.
pub const SCAN_MAX_DEPTH: usize = 4;

/// Directory names the scan never descends into: version control and dependency trees,
/// which contain no cargo target directory but plenty of files.
const SCAN_SKIP_DIRS: [&str; 3] = [".git", ".jj", "node_modules"];

/// discover_targets — every cargo target directory at or below `root`, up to `max_depth`
/// levels down (the root itself is level 0).
///
/// A directory is a target directory when one of its child directories contains an
/// `incremental/` or a `deps/` directory: that is exactly what a cargo *profile tree*
/// looks like — `debug`, `release`, a cross-compilation triple such as
/// `aarch64-apple-darwin`, or a custom profile such as `profiling`. The named cases are
/// the common ones, but generalising to "any profile-shaped child" is deliberate: a
/// repository whose only profile tree is a custom profile would otherwise be skipped
/// silently, and *missing a repository* is the failure mode this mode exists to prevent.
///
/// A recognised target directory is never descended into: its `deps/` and `incremental/`
/// are cargo's own bookkeeping, not nested repositories. Children that are symlinks are
/// not followed, so a link pointing at an ancestor cannot make the walk loop.
pub fn discover_targets(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    discover_visit(root, 0, max_depth, &mut found);
    // Two roots can reach the same target (`--scan-root /a --scan-root /a/b`, or a
    // symlinked root): deduplicate by identity, not by spelling.
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in found {
        let identity = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        unique.push(path);
    }
    unique.sort();
    unique
}

fn discover_visit(dir: &Path, depth: usize, max_depth: usize, found: &mut Vec<PathBuf>) {
    if is_target_dir(dir) {
        found.push(dir.to_path_buf());
        return;
    }
    if depth >= max_depth {
        return;
    }
    for child in read_dir_sorted(dir).unwrap_or_default() {
        // Only real directories: a symlink is not followed, so the walk terminates even
        // when something links back up the tree.
        match fs::symlink_metadata(&child) {
            Ok(meta) if meta.is_dir() => {}
            _ => continue,
        }
        let name = file_name(&child);
        if SCAN_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        discover_visit(&child, depth + 1, max_depth, found);
    }
}

fn is_target_dir(dir: &Path) -> bool {
    read_dir_sorted(dir)
        .unwrap_or_default()
        .iter()
        .any(|child| child.is_dir() && is_profile_tree(child))
}

/// is_profile_tree — does this child directory look like `target/<profile>`?
///
/// `debug` and `release` are unambiguous under a target directory, so either cargo marker
/// is enough. Anything else — a cross-compilation triple such as `aarch64-apple-darwin`,
/// or a custom profile such as `profiling` — needs **two** cargo-specific markers
/// (`deps/` plus `incremental/` or `.cargo-lock`). That second requirement is what keeps
/// an ordinary source directory named `…/incremental` or `…/deps` from being mistaken for
/// a profile tree, while still catching the custom-profile layouts that a
/// `debug`/`release`/triple-only rule would silently skip.
fn is_profile_tree(child: &Path) -> bool {
    let deps = child.join("deps").is_dir();
    let incremental = child.join("incremental").is_dir();
    let lock = child.join(".cargo-lock").is_file();
    let name = file_name(child);
    if name == "debug" || name == "release" {
        return deps || incremental;
    }
    deps && (incremental || lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cargogc-judge-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("target")).unwrap();
        dir
    }

    fn session(root: &Path, name: &str, age_secs: u64) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dep-graph.bin"), vec![1u8; 4096]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        File::open(&dir).unwrap().set_modified(when).unwrap();
    }

    fn root(target: &Path, profile: &str, name: &str) -> PathBuf {
        let dir = target.join(profile).join("incremental").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn without_a_build_record_nothing_is_superseded() {
        let dir = scratch("no-record");
        let target = dir.join("target");
        let r = root(&target, "debug", "libfoo-aaa");
        session(&r, "s-1-1-1", 7200);

        let items = plan(&Inputs::new(target.clone())).unwrap();
        assert!(
            items.is_empty(),
            "no T ⇒ the superseded classes must be a no-op: {items:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_old_session_goes_and_the_live_one_stays() {
        let dir = scratch("stale-session");
        let target = dir.join("target");
        let r = root(&target, "debug", "libfoo-aaa");
        session(&r, "s-old-aa-1", 7200);
        session(&r, "s-new-bb-1", 0);

        let mut input = Inputs::new(target.clone());
        input.since = Some(now());
        let items = plan(&input).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].kind, Kind::SupersededSession);
        assert!(items[0].path.ends_with("s-old-aa-1"));
        assert!(
            r.join("s-new-bb-1").is_dir(),
            "plan must not delete anything"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_superseded_root_goes_but_the_newest_root_of_that_crate_name_stays() {
        let dir = scratch("newest-root");
        let target = dir.join("target");
        let old = root(&target, "debug", "libfoo-aaa");
        let new = root(&target, "debug", "libfoo-bbb");
        session(&old, "s-1-1-1", 7200);
        session(&new, "s-2-2-2", 0);

        let mut input = Inputs::new(target.clone());
        input.since = Some(now());
        let items = plan(&input).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].kind, Kind::SupersededRoot);
        assert_eq!(items[0].path, old);
        assert!(new.is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lone_stale_root_is_compacted_not_deleted() {
        let dir = scratch("compacted");
        let target = dir.join("target");
        let r = root(&target, "debug", "libonly-aaa");
        session(&r, "s-old-aa-1", 7200);
        session(&r, "s-old-bb-2", 3600);

        let mut input = Inputs::new(target.clone());
        input.since = Some(now());
        let items = plan(&input).unwrap();
        assert_eq!(items.len(), 1, "the newest session stays: {items:?}");
        assert!(items[0].path.ends_with("s-old-aa-1"));
        assert!(r.is_dir());

        // With protection disabled the whole root is fair game.
        input.keep_newest_roots = 0;
        let items = plan(&input).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, Kind::SupersededRoot);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn orphan_locks_go_and_live_locks_stay() {
        let dir = scratch("locks");
        let target = dir.join("target");
        let r = root(&target, "debug", "libfoo-aaa");
        session(&r, "s-keep-aa-1", 0);
        session(&r, "s-gone-bb-2", 7200);
        fs::write(r.join("s-keep-aa.lock"), b"").unwrap();
        fs::write(r.join("s-gone-bb.lock"), b"").unwrap();
        fs::write(r.join("s-orphan-cc.lock"), b"").unwrap();

        let mut input = Inputs::new(target.clone());
        input.since = Some(now());
        let items = plan(&input).unwrap();
        let locks: Vec<&PlanItem> = items
            .iter()
            .filter(|i| i.kind == Kind::SupersededLock)
            .collect();
        let names: Vec<String> = locks
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["s-gone-bb.lock", "s-orphan-cc.lock"],
            "live lock must stay"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dead_dir_is_deleted_wholesale_and_its_contents_are_not_re_judged() {
        let dir = scratch("dead");
        let target = dir.join("target");
        let dead_root = root(&target, "release", "libfoo-aaa");
        session(&dead_root, "s-1-1-1", 7200);

        let mut input = Inputs::new(target.clone());
        input.since = Some(now());
        input.dead_dirs = vec![target.join("release/incremental")];
        let items = plan(&input).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].kind, Kind::Dead);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_cap_deletes_the_whole_tree_and_is_opt_in() {
        let dir = scratch("cap");
        let target = dir.join("target");
        let r = root(&target, "debug", "libfoo-aaa");
        session(&r, "s-1-1-1", 0);

        let mut input = Inputs::new(target.clone());
        input.since = Some(now());
        assert!(
            plan(&input).unwrap().is_empty(),
            "under no cap, a live session is kept"
        );

        input.cap = Some(1);
        let items = plan(&input).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, Kind::Capped);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discover_handles_the_cross_compilation_layout() {
        let dir = scratch("layout");
        let target = dir.join("target");
        root(&target, "debug", "libfoo-aaa");
        root(&target, "aarch64-apple-darwin/profiling", "libbar-bbb");
        let found = discover_incremental_dirs(&target);
        assert_eq!(found.len(), 2, "{found:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn now() -> i64 {
        system_time_secs(SystemTime::now()).unwrap()
    }

    // ── batch discovery ─────────────────────────────────────────────────────

    /// A cargo target directory at `path`, with a profile tree that makes it recognisable.
    fn fake_target(path: &Path) -> PathBuf {
        fs::create_dir_all(path.join("debug/deps")).unwrap();
        fs::create_dir_all(path.join("debug/incremental")).unwrap();
        path.to_path_buf()
    }

    fn mkdirs(path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        path.to_path_buf()
    }

    #[test]
    fn scan_finds_a_target_four_levels_down_and_not_five() {
        let dir = scratch("scan-depth");
        let shallow = fake_target(&dir.join("a/b/c/target"));
        mkdirs(&dir.join("x/y/z/w"));
        let too_deep = fake_target(&dir.join("x/y/z/w/target"));

        let found = discover_targets(&dir, SCAN_MAX_DEPTH);
        assert_eq!(found, vec![shallow.clone()], "{found:?}");
        assert!(!found.contains(&too_deep), "five levels down is out of range");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_does_not_descend_into_a_target_directory() {
        let dir = scratch("scan-nested");
        let outer = fake_target(&dir.join("app/target"));
        // A second target directory *inside* the first one's profile tree: cargo never
        // nests these, so finding it would mean the walk descended past a boundary and a
        // real repository could be counted twice.
        let inner = fake_target(&dir.join("app/target/debug/nested/target"));

        let found = discover_targets(&dir, SCAN_MAX_DEPTH);
        assert_eq!(found, vec![outer], "only the outer target may be reported: {found:?}");
        assert!(!found.contains(&inner));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_skips_vcs_and_dependency_trees() {
        let dir = scratch("scan-skips");
        fake_target(&dir.join(".git/modules/x/target"));
        fake_target(&dir.join(".jj/repo/target"));
        fake_target(&dir.join("node_modules/pkg/target"));
        let real = fake_target(&dir.join("repo/target"));

        let found = discover_targets(&dir, SCAN_MAX_DEPTH);
        assert_eq!(found, vec![real], "{found:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_recognises_a_custom_profile_tree() {
        let dir = scratch("scan-custom-profile");
        // `profiling` is a single-word custom profile: the named cases (`debug`,
        // `release`, `<triple>`) would all miss this tree, and missing a repository is the
        // failure this mode exists to prevent.
        let target = dir.join("repo/target");
        fs::create_dir_all(target.join("profiling/incremental")).unwrap();
        fs::create_dir_all(target.join("profiling/deps")).unwrap();

        let found = discover_targets(&dir, SCAN_MAX_DEPTH);
        assert_eq!(found, vec![target], "{found:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_ignores_a_directory_that_is_not_a_target_tree() {
        let dir = scratch("scan-negative");
        // Two shapes an over-eager rule would swallow: a source directory named
        // `incremental` (only one cargo marker) and a vendored `deps` directory (also only
        // one). Neither is a profile tree, so neither parent is a target directory.
        mkdirs(&dir.join("docs/incremental"));
        mkdirs(&dir.join("repo/src/vendor/deps"));

        let found = discover_targets(&dir, SCAN_MAX_DEPTH);
        assert!(found.is_empty(), "no profile tree ⇒ no target directory: {found:?}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
