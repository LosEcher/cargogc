//! Rendering. Two renderers over one structure: the human one is what a developer reads
//! after a build, the JSON one is what a hook or a dashboard reads.
//!
//! Both print the same four accounting numbers, and the human one spells out what they
//! mean instead of leaving the reader to guess which of them answers "how much do I get
//! back": `du` freed, logical, shared-but-still-allocated, and the inflation ratio
//! between the last two.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::account::{self, ItemBytes, Totals};
use crate::gate::Probe;
use crate::judge::{Kind, PlanItem};
use crate::ledger::J;
use crate::VERSION;

/// One planned (or performed) deletion, with its own numbers.
#[derive(Debug, Clone)]
pub struct ReportItem {
    pub kind: Kind,
    pub path: PathBuf,
    pub reason: String,
    pub bytes: ItemBytes,
}

/// Everything a renderer needs. Assembled by `main` so that both renderers stay dumb.
#[derive(Debug, Clone)]
pub struct Report {
    pub op: &'static str,
    pub verdict: &'static str,
    pub target: PathBuf,
    pub since: Option<i64>,
    pub since_source: Option<String>,
    pub cap: Option<u64>,
    pub keep_newest_roots: usize,
    pub gate: Probe,
    pub items: Vec<ReportItem>,
    pub totals: Totals,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    /// Pre-rendered reconciliation lines (P1's `--reconcile`), empty when not requested.
    pub reconcile: Vec<String>,
}

impl Report {
    pub fn from_plan(
        op: &'static str,
        verdict: &'static str,
        target: PathBuf,
        plan: &[PlanItem],
        per_item: &[ItemBytes],
        totals: Totals,
    ) -> Report {
        let items = plan
            .iter()
            .enumerate()
            .map(|(index, item)| ReportItem {
                kind: item.kind,
                path: item.path.clone(),
                reason: item.reason.clone(),
                bytes: per_item.get(index).copied().unwrap_or_default(),
            })
            .collect();
        Report {
            op,
            verdict,
            target,
            since: None,
            since_source: None,
            cap: None,
            keep_newest_roots: 1,
            gate: Probe::Free,
            items,
            totals,
            failures: Vec::new(),
            notes: Vec::new(),
            reconcile: Vec::new(),
        }
    }
}

/// class_totals — per-class sums; they add up to the run totals by construction.
pub fn class_totals(items: &[ReportItem]) -> Vec<(Kind, usize, ItemBytes)> {
    let mut by_kind: BTreeMap<Kind, (usize, ItemBytes)> = BTreeMap::new();
    for item in items {
        let entry = by_kind
            .entry(item.kind)
            .or_insert((0, ItemBytes::default()));
        entry.0 += 1;
        entry.1.freed_du += item.bytes.freed_du;
        entry.1.logical += item.bytes.logical;
        entry.1.shared_du += item.bytes.shared_du;
    }
    Kind::ALL
        .iter()
        .map(|kind| {
            let (count, bytes) = by_kind
                .get(kind)
                .copied()
                .unwrap_or((0, ItemBytes::default()));
            (*kind, count, bytes)
        })
        .collect()
}

/// format_ts_utc — `YYYY-MM-DDTHH:MM:SSZ` for a unix timestamp.
///
/// Deliberately UTC and labelled as such: the tool has no timezone database (adding one
/// would mean a dependency), and an unlabelled local time is exactly the ambiguity that
/// makes a build record unreproducible.
pub fn format_ts_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// civil_from_days — Howard Hinnant's days-to-civil algorithm (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// format_epoch_utc — the same timestamp, in the more readable form used in prose.
pub fn format_epoch_utc(epoch: i64) -> String {
    let text = format_ts_utc(epoch);
    format!("{} {}", &text[..10], &text[11..19])
}

fn display_path(path: &Path, target: &Path) -> String {
    match path.strip_prefix(target) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    }
}

/// render_human — the report a developer reads.
pub fn render_human(report: &Report, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "cargogc {VERSION} — {} — target: {}",
        report.op,
        report.target.display()
    )?;
    match (report.since, &report.since_source) {
        (Some(since), Some(source)) => writeln!(
            out,
            "  since: {since} ({})   [{source}]",
            format_epoch_utc(since)
        )?,
        _ => writeln!(
            out,
            "  since: (no build record) — the superseded classes do nothing without one"
        )?,
    }
    if let Some(cap) = report.cap {
        writeln!(
            out,
            "  live incremental cap: {}   keep newest roots per crate: {}",
            account::human(cap),
            report.keep_newest_roots
        )?;
    } else {
        writeln!(
            out,
            "  keep newest roots per crate: {}",
            report.keep_newest_roots
        )?;
    }
    writeln!(out, "  gate: {}", report.gate.describe())?;
    for note in &report.notes {
        writeln!(out, "  note: {note}")?;
    }
    writeln!(out)?;

    for item in &report.items {
        let verb = if report.verdict == "applied" || report.verdict == "partial" {
            "deleted"
        } else {
            "delete"
        };
        writeln!(
            out,
            "  {verb:<7} {:<19} {:>10}  {}   ({})",
            item.kind.as_str(),
            account::human(item.bytes.freed_du),
            display_path(&item.path, &report.target),
            item.reason
        )?;
    }
    if !report.items.is_empty() {
        writeln!(out)?;
    }

    for (kind, count, bytes) in class_totals(&report.items) {
        writeln!(
            out,
            "  {:<19} {:>5} items {:>12} du {:>12} logical {:>12} shared",
            kind.as_str(),
            count,
            account::human(bytes.freed_du),
            account::human(bytes.logical),
            account::human(bytes.shared_du),
        )?;
    }

    let inflation = account::inflation(report.totals.logical, report.totals.freed_du);
    writeln!(out, "  {}", "-".repeat(66))?;
    writeln!(
        out,
        "  freed (du, inode-honest)  {:>12}",
        account::human(report.totals.freed_du)
    )?;
    let inflation_text = if inflation.is_finite() {
        format!("{inflation:.2}x")
    } else {
        "n/a".to_string()
    };
    writeln!(
        out,
        "  logical (per path)        {:>12}   inflation {inflation_text}",
        account::human(report.totals.logical)
    )?;
    writeln!(
        out,
        "  shared, still allocated   {:>12}   ({} inodes hardlinked outside the delete set)",
        account::human(report.totals.shared_du),
        report.totals.shared_inodes
    )?;

    for failure in &report.failures {
        writeln!(out, "  FAILED: {failure}")?;
    }
    if !report.reconcile.is_empty() {
        writeln!(out)?;
        for line in &report.reconcile {
            writeln!(out, "  {line}")?;
        }
    }

    writeln!(out)?;
    match report.verdict {
        "preview" => {
            if report.items.is_empty() {
                writeln!(out, "  nothing to reclaim.")?;
            } else {
                writeln!(
                    out,
                    "  dry-run: nothing was deleted. Re-run as `cargogc apply` with the same arguments."
                )?;
            }
        }
        "applied" => writeln!(out, "  applied: {} item(s) deleted.", report.items.len())?,
        "partial" => writeln!(
            out,
            "  applied with failures: {} item(s) deleted, {} failed (see FAILED above).",
            report.items.len() - report.failures.len(),
            report.failures.len()
        )?,
        "refused" => writeln!(
            out,
            "  refused: nothing was deleted. Re-run once the build has finished."
        )?,
        other => writeln!(out, "  verdict: {other}")?,
    }
    Ok(())
}

/// render_json — the same numbers, for a hook or a dashboard.
pub fn render_json(report: &Report, out: &mut impl Write) -> io::Result<()> {
    let classes = class_totals(&report.items)
        .into_iter()
        .map(|(kind, count, bytes)| {
            J::obj(vec![
                ("kind", J::S(kind.as_str().to_string())),
                ("items", J::U(count as u64)),
                ("bytes_du", J::U(bytes.freed_du)),
                ("bytes_logical", J::U(bytes.logical)),
                ("shared_du", J::U(bytes.shared_du)),
            ])
        })
        .collect();

    let items = report
        .items
        .iter()
        .map(|item| {
            J::obj(vec![
                ("kind", J::S(item.kind.as_str().to_string())),
                ("path", J::S(item.path.to_string_lossy().into_owned())),
                ("reason", J::S(item.reason.clone())),
                ("bytes_du", J::U(item.bytes.freed_du)),
                ("bytes_logical", J::U(item.bytes.logical)),
                ("shared_du", J::U(item.bytes.shared_du)),
            ])
        })
        .collect();

    let gate = match &report.gate {
        Probe::Free => J::obj(vec![("state", J::S("free".into()))]),
        Probe::Held(paths) => J::obj(vec![
            ("state", J::S("held".into())),
            (
                "locks",
                J::A(
                    paths
                        .iter()
                        .map(|p| J::S(p.to_string_lossy().into_owned()))
                        .collect(),
                ),
            ),
        ]),
        Probe::Unavailable(why) => J::obj(vec![
            ("state", J::S("unavailable".into())),
            ("detail", J::S(why.clone())),
        ]),
    };

    let inflation = account::inflation(report.totals.logical, report.totals.freed_du);
    let document = J::obj(vec![
        ("tool", J::S("cargogc".into())),
        ("version", J::S(VERSION.into())),
        ("op", J::S(report.op.into())),
        ("verdict", J::S(report.verdict.into())),
        ("target", J::S(report.target.to_string_lossy().into_owned())),
        (
            "since",
            match report.since {
                Some(since) => J::I(since),
                None => J::I(0),
            },
        ),
        (
            "since_source",
            match &report.since_source {
                Some(source) => J::S(source.clone()),
                None => J::S(String::new()),
            },
        ),
        (
            "cap_bytes",
            match report.cap {
                Some(cap) => J::U(cap),
                None => J::U(0),
            },
        ),
        ("keep_newest_roots", J::U(report.keep_newest_roots as u64)),
        ("gate", gate),
        ("classes", J::A(classes)),
        (
            "totals",
            J::obj(vec![
                ("items", J::U(report.totals.items as u64)),
                ("bytes_du", J::U(report.totals.freed_du)),
                ("bytes_logical", J::U(report.totals.logical)),
                ("shared_du", J::U(report.totals.shared_du)),
                ("shared_inodes", J::U(report.totals.shared_inodes as u64)),
                ("inflation", J::F(inflation)),
            ]),
        ),
        ("items", J::A(items)),
        (
            "failures",
            J::A(report.failures.iter().map(|f| J::S(f.clone())).collect()),
        ),
        (
            "notes",
            J::A(report.notes.iter().map(|n| J::S(n.clone())).collect()),
        ),
    ]);
    writeln!(out, "{}", document.render())
}

// ── batch mode (`--scan-root`) ──────────────────────────────────────────────
//
// Scan mode shares every judgement and every number with single-target mode; what it
// changes is what happens to a *problem* and how the round is summarised. Two rules are
// specific to it and are stated wherever a reader meets the mode (help, README, report):
//
//   * a held lock skips that target and the round continues (single-target mode refuses
//     the whole run with exit 3) — a nightly batch must not be voided by one repository
//     that happens to be building, but a skipped target is always named, never silent;
//   * `dead` does not participate, because the dead class is derived from a repository's
//     own manifest and a scan has no manifest per target.

/// The sentence scan mode prints in place of the `dead` class. Constants so the help, the
/// report and the tests cannot drift apart.
pub const SCAN_DEAD_NOTE: &str = "skipped (scan mode has no manifest)";

/// One target directory inside a scan round.
#[derive(Debug, Clone)]
pub struct TargetReport {
    pub target: PathBuf,
    pub items: Vec<ReportItem>,
    pub totals: Totals,
    pub gate: Probe,
    /// `Some(reason)` when this target was skipped. The string is the human reason; the
    /// machine reason is derived from the gate ([`skip_reason_code`]).
    pub skipped: Option<String>,
    pub failures: Vec<String>,
}

/// skip_reason_code — the machine-readable why, for JSON and the ledger.
pub fn skip_reason_code(gate: &Probe) -> &'static str {
    match gate {
        Probe::Held(_) => "lock-held",
        Probe::Unavailable(_) => "probe-unavailable",
        Probe::Free => "skipped",
    }
}

/// A whole `--scan-root` round.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub op: &'static str,
    pub verdict: &'static str,
    pub roots: Vec<PathBuf>,
    pub targets: Vec<TargetReport>,
    pub scan_totals: Totals,
    pub since: Option<i64>,
    pub since_source: Option<String>,
    pub cap: Option<u64>,
    pub keep_newest_roots: usize,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    /// Pre-rendered reconciliation lines (P1's `--reconcile`).
    pub reconcile: Vec<String>,
}

impl ScanReport {
    pub fn applied(&self) -> Vec<&TargetReport> {
        self.targets.iter().filter(|t| t.skipped.is_none()).collect()
    }

    pub fn skipped(&self) -> Vec<&TargetReport> {
        self.targets.iter().filter(|t| t.skipped.is_some()).collect()
    }

    /// How many items the round covers **after** skipping **and gating**: a skipped target
    /// contributes nothing (none of its plan was carried out), and a target a build is holding
    /// contributes nothing either — in apply mode it is skipped, and in a preview it is shown
    /// for information but must not inflate the total, or `plan` would promise bytes that
    /// `apply` will not return (measured: preview "2 items" where apply deleted 1).
    ///
    /// A target's own row still shows what its plan *would* have reclaimed — that is the
    /// number a reader asks for when the round missed something, so it is kept and marked.
    pub fn item_count(&self) -> usize {
        self.applied()
            .iter()
            .filter(|t| t.gate.is_free())
            .map(|t| t.items.len())
            .sum()
    }
}

fn note_line(report: &ScanReport, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "  dead: {SCAN_DEAD_NOTE}",
    )?;
    match (report.since, &report.since_source) {
        (Some(since), Some(source)) => writeln!(
            out,
            "  since: {since} ({})   [{source}]",
            format_epoch_utc(since)
        ),
        _ => writeln!(
            out,
            "  since: (no build record) — the superseded classes do nothing without one"
        ),
    }
}

/// render_scan_human — one line per target, then the round totals and the skipped list.
///
/// Per-item lines are deliberately absent: a nightly batch over twenty repositories would
/// print thousands of them, and the question a reader has at that scale is "which
/// repository, how much, and which ones were skipped".
pub fn render_scan_human(report: &ScanReport, out: &mut impl Write) -> io::Result<()> {
    let deleted = report.verdict == "applied" || report.verdict == "partial";
    writeln!(
        out,
        "cargogc {VERSION} — {} — scan mode: {} target(s) under {} root(s)",
        report.op,
        report.targets.len(),
        report.roots.len()
    )?;
    writeln!(
        out,
        "  roots: {}",
        report
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    note_line(report, out)?;
    if let Some(cap) = report.cap {
        writeln!(
            out,
            "  live incremental cap: {}   keep newest roots per crate: {}",
            account::human(cap),
            report.keep_newest_roots
        )?;
    } else {
        writeln!(
            out,
            "  keep newest roots per crate: {}",
            report.keep_newest_roots
        )?;
    }
    for note in &report.notes {
        writeln!(out, "  note: {note}")?;
    }
    writeln!(out)?;

    for target in &report.targets {
        let skipped = target.skipped.is_some();
        let verb = if skipped {
            "skip "
        } else if deleted {
            "apply"
        } else {
            "plan "
        };
        // A skipped target's numbers are what its plan *would* have reclaimed, and they are
        // excluded from the round totals — marked, because "8 KiB freed" next to a skip
        // reads like 8 KiB came back.
        // A gated row in a preview is marked like a skip: its numbers are not in the total.
        let mark = if skipped || (!deleted && !target.gate.is_free()) {
            "*"
        } else {
            " "
        };
        writeln!(
            out,
            "  {verb}  {:<48} {:>5} items {:>12} freed{mark}  gate {}",
            target.target.display(),
            target.items.len(),
            account::human(target.totals.freed_du),
            target.gate.describe()
        )?;
    }

    writeln!(out)?;
    // A preview must not say "applied": nothing was applied, and in a preview a held gate is
    // reported as information rather than as a skip (the plan is still shown for that target).
    // Saying "4 applied" over a run that deleted nothing is the same class of misstatement the
    // four-numbers rule exists to prevent.
    if deleted {
        writeln!(
            out,
            "  target(s): {} ({} applied, {} skipped)",
            report.targets.len(),
            report.applied().len(),
            report.skipped().len()
        )?;
    } else {
        let gated = report
            .targets
            .iter()
            .filter(|target| target.skipped.is_none() && !target.gate.is_free())
            .count();
        writeln!(
            out,
            "  target(s): {} ({} would be swept, {} held by a build)",
            report.targets.len(),
            report.applied().len() - gated,
            gated
        )?;
    }
    let inflation = account::inflation(report.scan_totals.logical, report.scan_totals.freed_du);
    let inflation_text = if inflation.is_finite() {
        format!("{inflation:.2}x")
    } else {
        "n/a".to_string()
    };
    writeln!(
        out,
        "  freed (du, inode-honest)  {:>12}",
        account::human(report.scan_totals.freed_du)
    )?;
    writeln!(
        out,
        "  logical (per path)        {:>12}   inflation {inflation_text}",
        account::human(report.scan_totals.logical)
    )?;
    writeln!(
        out,
        "  shared, still allocated   {:>12}   ({} inodes hardlinked outside the delete set)",
        account::human(report.scan_totals.shared_du),
        report.scan_totals.shared_inodes
    )?;

    let skipped = report.skipped();
    if !skipped.is_empty() {
        writeln!(out)?;
        writeln!(out, "  skipped targets (nothing was deleted in these):")?;
        for target in skipped {
            writeln!(
                out,
                "    {}   {}",
                target.target.display(),
                target.skipped.clone().unwrap_or_default()
            )?;
        }
        writeln!(
            out,
            "  * the numbers on a marked row are what its plan would have reclaimed, and they are \
             not in the round totals above."
        )?;
    }
    for failure in &report.failures {
        writeln!(out, "  FAILED: {failure}")?;
    }
    if !report.reconcile.is_empty() {
        writeln!(out)?;
        for line in &report.reconcile {
            writeln!(out, "  {line}")?;
        }
    }

    writeln!(out)?;
    match report.verdict {
        "preview" => {
            if report.item_count() == 0 {
                writeln!(out, "  nothing to reclaim.")?;
            } else {
                writeln!(
                    out,
                    "  dry-run: nothing was deleted. Re-run as `cargogc apply` with the same arguments."
                )?;
            }
        }
        "applied" => writeln!(
            out,
            "  applied: {} item(s) deleted across {} target(s).",
            report.item_count(),
            report.applied().len()
        )?,
        "partial" => writeln!(
            out,
            "  applied with failures: {} item(s) deleted, {} failed (see FAILED above).",
            report.item_count(),
            report.failures.len()
        )?,
        "skipped" => writeln!(
            out,
            "  every target was skipped; nothing was deleted. Re-run once those builds finish."
        )?,
        other => writeln!(out, "  verdict: {other}")?,
    }
    Ok(())
}

/// render_scan_json — the round aggregate: `targets[]` plus `scan_totals`.
pub fn render_scan_json(report: &ScanReport, out: &mut impl Write) -> io::Result<()> {
    let targets = report
        .targets
        .iter()
        .map(|target| {
            let classes = class_totals(&target.items)
                .into_iter()
                .map(|(kind, count, bytes)| {
                    J::obj(vec![
                        ("kind", J::S(kind.as_str().to_string())),
                        ("items", J::U(count as u64)),
                        ("bytes_du", J::U(bytes.freed_du)),
                        ("bytes_logical", J::U(bytes.logical)),
                        ("shared_du", J::U(bytes.shared_du)),
                    ])
                })
                .collect();
            let gate = J::obj(vec![
                ("state", J::S(target.gate.state().into())),
                ("detail", J::S(target.gate.describe())),
            ]);
            let mut fields = vec![
                ("target", J::S(target.target.to_string_lossy().into_owned())),
                (
                    "verdict",
                    J::S(if target.skipped.is_some() { "skipped" } else { "applied" }.into()),
                ),
                ("gate", gate),
                ("classes", J::A(classes)),
                (
                    "totals",
                    J::obj(vec![
                        ("items", J::U(target.items.len() as u64)),
                        ("bytes_du", J::U(target.totals.freed_du)),
                        ("bytes_logical", J::U(target.totals.logical)),
                        ("shared_du", J::U(target.totals.shared_du)),
                        ("shared_inodes", J::U(target.totals.shared_inodes as u64)),
                    ]),
                ),
            ];
            if let Some(reason) = &target.skipped {
                fields.push((
                    "skipped",
                    J::obj(vec![
                        ("reason", J::S(skip_reason_code(&target.gate).into())),
                        ("detail", J::S(reason.clone())),
                    ]),
                ));
            }
            J::obj(fields)
        })
        .collect();

    let skipped_paths: Vec<J> = report
        .skipped()
        .iter()
        .map(|t| J::S(t.target.to_string_lossy().into_owned()))
        .collect();

    let inflation = account::inflation(report.scan_totals.logical, report.scan_totals.freed_du);
    let document = J::obj(vec![
        ("tool", J::S("cargogc".into())),
        ("version", J::S(VERSION.into())),
        ("op", J::S(report.op.into())),
        ("mode", J::S("scan".into())),
        ("verdict", J::S(report.verdict.into())),
        (
            "roots",
            J::A(
                report
                    .roots
                    .iter()
                    .map(|r| J::S(r.to_string_lossy().into_owned()))
                    .collect(),
            ),
        ),
        (
            "since",
            J::I(report.since.unwrap_or(0)),
        ),
        (
            "since_source",
            J::S(report.since_source.clone().unwrap_or_default()),
        ),
        ("cap_bytes", J::U(report.cap.unwrap_or(0))),
        ("keep_newest_roots", J::U(report.keep_newest_roots as u64)),
        ("dead", J::S(SCAN_DEAD_NOTE.into())),
        ("targets", J::A(targets)),
        ("skipped", J::A(skipped_paths)),
        (
            "scan_totals",
            J::obj(vec![
                ("targets", J::U(report.targets.len() as u64)),
                ("applied", J::U(report.applied().len() as u64)),
                ("skipped", J::U(report.skipped().len() as u64)),
                ("items", J::U(report.item_count() as u64)),
                ("bytes_du", J::U(report.scan_totals.freed_du)),
                ("bytes_logical", J::U(report.scan_totals.logical)),
                ("shared_du", J::U(report.scan_totals.shared_du)),
                ("shared_inodes", J::U(report.scan_totals.shared_inodes as u64)),
                ("inflation", J::F(inflation)),
            ]),
        ),
        (
            "failures",
            J::A(report.failures.iter().map(|f| J::S(f.clone())).collect()),
        ),
        (
            "notes",
            J::A(report.notes.iter().map(|n| J::S(n.clone())).collect()),
        ),
    ]);
    writeln!(out, "{}", document.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::PlanItem;

    #[test]
    fn utc_formatting_covers_known_timestamps_and_negative_time() {
        assert_eq!(format_ts_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_ts_utc(1_791_618_600), "2026-10-10T07:50:00Z");
        assert_eq!(format_ts_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_epoch_utc(1_791_618_600), "2026-10-10 07:50:00");
        // A leap day, to catch an off-by-one in the civil conversion.
        assert_eq!(format_ts_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn class_totals_sum_to_the_run_totals() {
        let items = vec![
            ReportItem {
                kind: Kind::SupersededRoot,
                path: PathBuf::from("/t/a"),
                reason: String::new(),
                bytes: ItemBytes {
                    freed_du: 100,
                    logical: 120,
                    shared_du: 20,
                },
            },
            ReportItem {
                kind: Kind::SupersededLock,
                path: PathBuf::from("/t/b"),
                reason: String::new(),
                bytes: ItemBytes {
                    freed_du: 0,
                    logical: 0,
                    shared_du: 0,
                },
            },
        ];
        let classes = class_totals(&items);
        let freed: u64 = classes.iter().map(|(_, _, b)| b.freed_du).sum();
        assert_eq!(freed, 100);
        let items_count: usize = classes.iter().map(|(_, c, _)| c).sum();
        assert_eq!(items_count, 2);
    }

    #[test]
    fn rendering_a_preview_says_nothing_was_deleted() {
        let plan = vec![PlanItem {
            kind: Kind::SupersededRoot,
            path: PathBuf::from("/tmp/x/target/debug/incremental/lib-aaa"),
            reason: "no session at or after T".into(),
        }];
        let per_item = vec![ItemBytes {
            freed_du: 2048,
            logical: 4096,
            shared_du: 1024,
        }];
        let mut report = Report::from_plan(
            "plan",
            "preview",
            PathBuf::from("/tmp/x/target"),
            &plan,
            &per_item,
            Totals {
                items: 1,
                freed_du: 2048,
                logical: 4096,
                shared_du: 1024,
                shared_inodes: 3,
            },
        );
        report.since = Some(1_791_618_600);
        report.since_source = Some("--superseded-since".into());

        let mut human = Vec::new();
        render_human(&report, &mut human).unwrap();
        let text = String::from_utf8(human).unwrap();
        assert!(text.contains("dry-run: nothing was deleted"));
        assert!(
            text.contains("debug/incremental/lib-aaa"),
            "path is shown relative to the target"
        );
        assert!(text.contains("inflation 2.00x"));
        assert!(
            text.contains("2026-10-10 07:50:00"),
            "the since line is rendered in UTC"
        );

        let mut json = Vec::new();
        render_json(&report, &mut json).unwrap();
        let parsed = crate::ledger::parse(String::from_utf8(json).unwrap().trim()).unwrap();
        assert_eq!(parsed.get("verdict").unwrap().as_str(), Some("preview"));
        assert_eq!(
            parsed
                .get("totals")
                .unwrap()
                .get("bytes_du")
                .unwrap()
                .as_u64(),
            Some(2048)
        );
        assert_eq!(parsed.get("items").unwrap().as_array().unwrap().len(), 1);
    }
}
