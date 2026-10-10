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
