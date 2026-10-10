//! The CLI: argument parsing, exit codes, and the one place that knows the order of
//! operations (judge → account → gate → act → ledger → report).
//!
//! Exit codes are a contract this tool's callers branch on:
//!
//! | code | meaning |
//! |---|---|
//! | 0 | ok (including "nothing to reclaim") |
//! | 1 | runtime error (unreadable tree, bad build-record *content*, delete failure) |
//! | 2 | usage error (bad option, unparseable timestamp, bare size, no target directory) |
//! | 3 | refused: a build holds a lock, or the gate cannot probe — **nothing deleted** |
//!
//! Nothing in this file reads `Cargo.toml`: which profile trees are dead is the
//! repository's knowledge and arrives as `--dead-dir`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use cargogc::account;
use cargogc::gate::{self, Probe};
use cargogc::judge::{self, Inputs};
use cargogc::ledger::{self, J};
use cargogc::report::{self, Report};
use cargogc::VERSION;

const EXIT_OK: u8 = 0;
const EXIT_RUNTIME: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Plan,
    Apply,
    LedgerView,
}

#[derive(Debug, Clone)]
struct Opts {
    op: Op,
    target_dir: Option<PathBuf>,
    superseded_since: Option<String>,
    last_build_file: Option<PathBuf>,
    cap: Option<u64>,
    keep_newest_roots: usize,
    dead_dirs: Vec<PathBuf>,
    ledger: Option<PathBuf>,
    json: bool,
    reconcile: bool,
}

/// Fail — every failure the CLI reports, tagged with its exit code.
#[derive(Debug)]
enum Fail {
    Usage(String),
    Runtime(String),
    Refused(String),
}

impl Fail {
    fn code(&self) -> u8 {
        match self {
            Fail::Usage(_) => EXIT_USAGE,
            Fail::Runtime(_) => EXIT_RUNTIME,
            Fail::Refused(_) => EXIT_REFUSED,
        }
    }

    fn message(&self) -> &str {
        match self {
            Fail::Usage(message) | Fail::Runtime(message) | Fail::Refused(message) => message,
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{}", usage());
        return ExitCode::from(EXIT_OK);
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        println!("cargogc {VERSION}");
        return ExitCode::from(EXIT_OK);
    }

    let opts = match parse_args(&args) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("cargogc: {message}");
            eprintln!("try `cargogc --help`");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    match run(&opts) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(fail) => {
            eprintln!("cargogc: {}", fail.message());
            ExitCode::from(fail.code())
        }
    }
}

fn usage() -> String {
    format!(
        "cargogc {VERSION} — reclaim the cargo build caches cargo will never read again

USAGE:
    cargogc plan  [OPTIONS]     read-only: show what apply would delete
    cargogc apply [OPTIONS]     delete it (refuses while a build holds a lock)
    cargogc ledger --view --ledger PATH [--json]

OPTIONS:
    --target-dir DIR            the cargo target directory (e.g. ./target).
                                Default: search the current directory and its parents
                                for one that contains `target/`.
    --superseded-since T        the build record: unix seconds, or ISO-8601 with an
                                explicit offset (2026-10-10T15:50:00+08:00). Relative
                                forms (\"2h ago\") and bare local times are refused:
                                they mean something different every time they are read.
    --last-build-file PATH      read the build record (unix seconds) from a file, e.g.
                                one written by a build hook. A missing or empty file
                                means \"no record\": the superseded classes then do
                                nothing, which is the only safe answer.
    --live-incremental-max SIZE opt-in cap on a live incremental directory, e.g. 8GiB
                                (1024-based) or 8GB (1000-based). A bare number is
                                refused: the unit is not guessable.
    --keep-newest-roots N       protect the newest N roots of each crate name
                                (default 1; 0 disables the protection).
    --dead-dir DIR              a tree cargo can never read (repeatable). The caller
                                decides this from its own manifest; cargogc does not
                                parse Cargo.toml.
    --ledger PATH               append apply/skip events as JSONL.
    --json                      machine-readable report.
    --reconcile                 additionally compare against the ledger's last apply.
    -h, --help / -V, --version

EXIT CODES:
    0 ok · 1 runtime error · 2 usage error · 3 refused (lock held or cannot probe)
"
    )
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let Some(first) = args.first() else {
        return Err("missing subcommand (plan, apply, ledger)".to_string());
    };
    let (op, rest) = match first.as_str() {
        "plan" => (Op::Plan, &args[1..]),
        "apply" => (Op::Apply, &args[1..]),
        "ledger" => (Op::LedgerView, &args[1..]),
        other => {
            return Err(format!(
                "unknown subcommand {other:?} (expected plan, apply, ledger)"
            ))
        }
    };

    let mut opts = Opts {
        op,
        target_dir: None,
        superseded_since: None,
        last_build_file: None,
        cap: None,
        keep_newest_roots: 1,
        dead_dirs: Vec::new(),
        ledger: None,
        json: false,
        reconcile: false,
    };

    let mut index = 0usize;
    while index < rest.len() {
        let arg = rest[index].clone();
        let take_value = |name: &str, index: &mut usize| -> Result<String, String> {
            *index += 1;
            rest.get(*index)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--target-dir" => {
                opts.target_dir = Some(PathBuf::from(take_value("--target-dir", &mut index)?))
            }
            "--superseded-since" => {
                opts.superseded_since = Some(take_value("--superseded-since", &mut index)?);
            }
            "--last-build-file" => {
                opts.last_build_file =
                    Some(PathBuf::from(take_value("--last-build-file", &mut index)?));
            }
            "--live-incremental-max" => {
                let raw = take_value("--live-incremental-max", &mut index)?;
                opts.cap = Some(parse_size(&raw)?);
            }
            "--keep-newest-roots" => {
                let raw = take_value("--keep-newest-roots", &mut index)?;
                opts.keep_newest_roots = raw.parse::<usize>().map_err(|_| {
                    format!("--keep-newest-roots needs a non-negative integer, got {raw:?}")
                })?;
            }
            "--dead-dir" => opts
                .dead_dirs
                .push(PathBuf::from(take_value("--dead-dir", &mut index)?)),
            "--ledger" => opts.ledger = Some(PathBuf::from(take_value("--ledger", &mut index)?)),
            "--json" => opts.json = true,
            "--reconcile" => opts.reconcile = true,
            "--view" => {
                if opts.op != Op::LedgerView {
                    return Err("--view belongs to the `ledger` subcommand".to_string());
                }
            }
            "--apply" => {
                if opts.op == Op::LedgerView {
                    return Err("--apply does not belong to the `ledger` subcommand".to_string());
                }
                opts.op = Op::Apply;
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        index += 1;
    }

    match opts.op {
        Op::LedgerView => {
            for (present, name) in [
                (opts.target_dir.is_some(), "--target-dir"),
                (opts.superseded_since.is_some(), "--superseded-since"),
                (opts.last_build_file.is_some(), "--last-build-file"),
                (opts.cap.is_some(), "--live-incremental-max"),
                (!opts.dead_dirs.is_empty(), "--dead-dir"),
                (opts.reconcile, "--reconcile"),
            ] {
                if present {
                    return Err(format!("{name} does not belong to the `ledger` subcommand"));
                }
            }
        }
        Op::Plan | Op::Apply => {
            if opts.superseded_since.is_some() && opts.last_build_file.is_some() {
                return Err(
                    "--superseded-since and --last-build-file are two answers to one question; \
                     give one"
                        .to_string(),
                );
            }
            if opts.reconcile && opts.ledger.is_none() {
                return Err("--reconcile reads the ledger; give --ledger PATH".to_string());
            }
        }
    }

    Ok(opts)
}

/// parse_size — `KiB/MiB/GiB` are 1024-based, `KB/MB/GB` are 1000-based, and a bare
/// number is refused: the unit decides whether 8 means 8 GB or 8 GiB.
fn parse_size(raw: &str) -> Result<u64, String> {
    let text = raw.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?} has no unit; write e.g. 8GiB or 8GB"))?;
    let (digits, unit) = text.split_at(split);
    if digits.is_empty() {
        return Err(format!("{text:?} does not start with a number"));
    }
    let number: u64 = digits
        .parse()
        .map_err(|_| format!("{digits:?} is not a number"))?;
    let multiplier: u64 = match unit {
        "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        other => {
            return Err(format!(
                "unknown unit {other:?}; use B, KB/MB/GB (1000-based) or KiB/MiB/GiB (1024-based)"
            ))
        }
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{text:?} overflows"))
}

/// parse_timestamp — unix seconds, or ISO-8601 with an explicit offset.
///
/// Two refusals are deliberate, and both exist because the alternative is a silent
/// wrong answer:
///
/// * relative forms ("2h ago") evaluate differently every time they are read, while the
///   whole judgement rests on "which builds happened after T";
/// * a bare local time does not name an instant without a timezone database, which this
///   crate deliberately does not link. Add the offset, or pass the unix seconds that a
///   build hook already has.
fn parse_timestamp(raw: &str) -> Result<i64, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("empty timestamp".to_string());
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        return text
            .parse::<i64>()
            .map_err(|err| format!("bad unix timestamp {text:?}: {err}"));
    }

    let hint = "pass unix seconds (e.g. 1791618600) or ISO-8601 with an explicit offset \
                (e.g. 2026-10-10T15:50:00+08:00); relative forms such as \"2h ago\" are refused \
                because they mean something different every time they are read";

    let (body, offset_seconds) = split_offset(text).ok_or_else(|| {
        format!("{text:?} has no UTC offset, so it does not name an instant; {hint}")
    })?;

    let (date, time) = match body.split_once(['T', ' ']) {
        Some((date, time)) => (date, Some(time)),
        None => (body, None),
    };
    let date_parts: Vec<&str> = date.split('-').collect();
    if date_parts.len() != 3 {
        return Err(format!("cannot read {text:?} as a date; {hint}"));
    }
    let year = parse_component(date_parts[0], "year")?;
    let month = parse_component(date_parts[1], "month")?;
    let day = parse_component(date_parts[2], "day")?;
    if !(1..=12).contains(&month) {
        return Err(format!("month {month} is out of range in {text:?}"));
    }
    if day < 1 || day > days_in_month(year, month) {
        return Err(format!(
            "day {day} is out of range for {year:04}-{month:02}"
        ));
    }

    let (mut hour, mut minute, mut second) = (0i64, 0i64, 0i64);
    if let Some(time) = time {
        let parts: Vec<&str> = time.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(format!("cannot read the time in {text:?}; {hint}"));
        }
        hour = parse_component(parts[0], "hour")?;
        minute = parse_component(parts[1], "minute")?;
        if parts.len() == 3 {
            second = parse_component(parts[2], "second")?;
        }
        if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
            return Err(format!("time out of range in {text:?}"));
        }
    }

    Ok(
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_seconds,
    )
}

fn parse_component(text: &str, what: &str) -> Result<i64, String> {
    text.parse::<i64>()
        .map_err(|_| format!("{text:?} is not a valid {what}"))
}

/// split_offset — separate the offset suffix from the rest, or `None` when absent.
fn split_offset(text: &str) -> Option<(&str, i64)> {
    if let Some(body) = text.strip_suffix('Z').or_else(|| text.strip_suffix('z')) {
        return Some((body, 0));
    }
    // An offset sign can only appear after the date part: positions 0..10 are the date,
    // whose dashes are separators, not signs.
    let bytes = text.as_bytes();
    for index in (11..bytes.len()).rev() {
        let byte = bytes[index];
        if byte == b'+' || byte == b'-' {
            let body = &text[..index];
            let offset = &text[index..];
            return parse_offset(offset).map(|seconds| (body, seconds));
        }
    }
    None
}

fn parse_offset(text: &str) -> Option<i64> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1i64, &text[1..]),
        b'-' => (-1i64, &text[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if rest.len() == 4 => rest.split_at(2),
        None => return None,
    };
    let hours = hours.parse::<i64>().ok()?;
    let minutes = minutes.parse::<i64>().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// days_from_civil — Howard Hinnant's civil-to-days algorithm, days since 1970-01-01.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ── the run ─────────────────────────────────────────────────────────────────

fn run(opts: &Opts) -> Result<(), Fail> {
    match opts.op {
        Op::LedgerView => run_ledger_view(opts),
        Op::Plan | Op::Apply => run_sweep(opts),
    }
}

fn run_sweep(opts: &Opts) -> Result<(), Fail> {
    let target_dir = resolve_target_dir(opts)?;
    let record = resolve_since(opts)?;
    let (since, since_source) = (record.since, record.source);
    let mut notes = record.notes;

    let mut input = Inputs::new(target_dir.clone());
    input.since = since;
    input.cap = opts.cap;
    input.keep_newest_roots = opts.keep_newest_roots;
    input.dead_dirs = opts.dead_dirs.clone();

    let plan = judge::plan(&input)
        .map_err(|err| Fail::Runtime(format!("cannot read {}: {err}", target_dir.display())))?;
    let paths: Vec<PathBuf> = plan.iter().map(|item| item.path.clone()).collect();
    let (per_item, totals) = account::account(&paths).map_err(|err| {
        Fail::Runtime(format!(
            "cannot account for the plan under {}: {err}",
            target_dir.display()
        ))
    })?;

    let apply = opts.op == Op::Apply;
    let probe = gate::probe(&gate::candidate_locks(&target_dir));
    let refused = apply && !probe.is_free();
    if !apply && !probe.is_free() {
        notes.push(format!(
            "gate: {} — `apply` would refuse until that finishes",
            probe.describe()
        ));
    }

    let verdict = if refused {
        "refused"
    } else if apply {
        "applied"
    } else {
        "preview"
    };
    let op_name = if apply { "apply" } else { "plan" };

    let mut report = Report::from_plan(
        op_name,
        verdict,
        target_dir.clone(),
        &plan,
        &per_item,
        totals,
    );
    report.since = since;
    report.since_source = since_source;
    report.cap = opts.cap;
    report.keep_newest_roots = opts.keep_newest_roots;
    report.gate = probe.clone();
    report.notes = notes;

    if refused {
        if let Some(path) = &opts.ledger {
            let reason = match probe {
                Probe::Held(_) => "lock-held",
                Probe::Unavailable(_) => "probe-unavailable",
                Probe::Free => "unreachable",
            };
            append_ledger(
                path,
                &J::obj(vec![
                    ("evt", J::S("skipped".into())),
                    ("run_id", J::S(new_run_id())),
                    ("ts", J::S(now_iso())),
                    ("op", J::S("apply".into())),
                    ("target", J::S(target_dir.to_string_lossy().into_owned())),
                    ("reason", J::S(reason.into())),
                    ("detail", J::S(probe.describe())),
                    ("since", J::I(since.unwrap_or(0))),
                    ("tool", J::S("cargogc".into())),
                    ("version", J::S(VERSION.into())),
                ]),
            )?;
        }
        render(&report, opts.json)?;
        return Err(Fail::Refused(format!(
            "{}; nothing was deleted ({} item(s) stay)",
            probe.describe(),
            report.items.len()
        )));
    }

    if apply {
        let run_id = new_run_id();
        if let Some(path) = &opts.ledger {
            append_ledger(
                path,
                &J::obj(vec![
                    ("evt", J::S("start".into())),
                    ("run_id", J::S(run_id.clone())),
                    ("ts", J::S(now_iso())),
                    ("op", J::S("apply".into())),
                    ("target", J::S(target_dir.to_string_lossy().into_owned())),
                    ("since", J::I(since.unwrap_or(0))),
                    ("cap_bytes", J::U(opts.cap.unwrap_or(0))),
                    ("keep_newest_roots", J::U(opts.keep_newest_roots as u64)),
                    ("pid", J::U(std::process::id() as u64)),
                    ("tool", J::S("cargogc".into())),
                    ("version", J::S(VERSION.into())),
                ]),
            )?;
        }

        for item in report.items.iter_mut() {
            if let Err(err) = delete_path(&item.path) {
                report
                    .failures
                    .push(format!("{}: {err}", item.path.display()));
                item.reason = format!("{} (FAILED)", item.reason);
            }
        }
        let failed = report.failures.len();
        if failed > 0 {
            report.verdict = "partial";
        }

        if let Some(path) = &opts.ledger {
            append_ledger(
                path,
                &finish_record(&run_id, &report, target_dir.as_path(), since, failed),
            )?;
        }
    }

    if opts.reconcile {
        report.reconcile = reconcile_lines(opts, &report)?;
    }

    render(&report, opts.json)?;
    if report.verdict == "partial" {
        return Err(Fail::Runtime(format!(
            "{} of {} items could not be deleted",
            report.failures.len(),
            report.items.len()
        )));
    }
    Ok(())
}

fn finish_record(
    run_id: &str,
    report: &Report,
    target: &Path,
    since: Option<i64>,
    failed: usize,
) -> J {
    let classes = report::class_totals(&report.items)
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
    J::obj(vec![
        ("evt", J::S("finish".into())),
        ("run_id", J::S(run_id.to_string())),
        ("ts", J::S(now_iso())),
        ("verdict", J::S(report.verdict.to_string())),
        ("target", J::S(target.to_string_lossy().into_owned())),
        ("since", J::I(since.unwrap_or(0))),
        ("classes", J::A(classes)),
        ("items", J::U(report.totals.items as u64)),
        (
            "removed",
            J::U(report.totals.items.saturating_sub(failed) as u64),
        ),
        ("failed", J::U(failed as u64)),
        ("bytes_du", J::U(report.totals.freed_du)),
        ("bytes_logical", J::U(report.totals.logical)),
        ("shared_du", J::U(report.totals.shared_du)),
        ("shared_inodes", J::U(report.totals.shared_inodes as u64)),
        ("tool", J::S("cargogc".into())),
        ("version", J::S(VERSION.into())),
    ])
}

fn append_ledger(path: &Path, record: &J) -> Result<(), Fail> {
    ledger::append(path, &record.render()).map_err(|err| {
        Fail::Runtime(format!(
            "cannot append to the ledger {}: {err}",
            path.display()
        ))
    })
}

fn delete_path(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        // A symlink is removed as a link; its target is never followed.
        fs::remove_file(path)
    }
}

fn resolve_target_dir(opts: &Opts) -> Result<PathBuf, Fail> {
    if let Some(dir) = &opts.target_dir {
        if !dir.is_dir() {
            return Err(Fail::Runtime(format!(
                "--target-dir is not a directory: {}",
                dir.display()
            )));
        }
        return Ok(dir.clone());
    }
    let cwd = std::env::current_dir()
        .map_err(|err| Fail::Runtime(format!("cannot read the current directory: {err}")))?;
    for dir in cwd.ancestors() {
        let candidate = dir.join("target");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(Fail::Usage(format!(
        "no --target-dir given, and no `target/` directory was found in {} or any parent; \
         pass --target-dir DIR (the cargo target directory)",
        cwd.display()
    )))
}

/// Where the build record came from, and what the caller should be told about it.
#[derive(Debug, Default)]
struct BuildRecord {
    since: Option<i64>,
    source: Option<String>,
    notes: Vec<String>,
}

fn resolve_since(opts: &Opts) -> Result<BuildRecord, Fail> {
    let mut record = BuildRecord::default();
    if let Some(raw) = &opts.superseded_since {
        let since = parse_timestamp(raw).map_err(Fail::Usage)?;
        record.since = Some(since);
        record.source = Some("--superseded-since".to_string());
        return Ok(record);
    }
    if let Some(path) = &opts.last_build_file {
        let source = format!("--last-build-file {}", path.display());
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                record.notes.push(format!(
                    "no build record at {} yet: the superseded classes do nothing",
                    path.display()
                ));
                record.source = Some(source);
                return Ok(record);
            }
            Err(err) => {
                return Err(Fail::Runtime(format!(
                    "cannot read --last-build-file {}: {err}",
                    path.display()
                )))
            }
        };
        let text = text.trim();
        if text.is_empty() {
            record.notes.push(format!(
                "the build record at {} is empty: the superseded classes do nothing",
                path.display()
            ));
            record.source = Some(source);
            return Ok(record);
        }
        let since = parse_timestamp(text).map_err(|err| {
            Fail::Runtime(format!(
                "--last-build-file {} does not hold a usable build record: {err}",
                path.display()
            ))
        })?;
        record.since = Some(since);
        record.source = Some(source);
        return Ok(record);
    }
    Ok(record)
}

fn reconcile_lines(opts: &Opts, report: &Report) -> Result<Vec<String>, Fail> {
    let Some(path) = &opts.ledger else {
        return Ok(Vec::new());
    };
    let outcome = ledger::view(path).map_err(|err| {
        Fail::Runtime(format!("cannot read the ledger {}: {err}", path.display()))
    })?;
    let runs = outcome.runs;
    let mut lines = vec![format!("reconcile (ledger {}):", path.display())];
    if outcome.foreign > 0 {
        lines.push(format!(
            "note: {} record(s) in this ledger were not written by cargogc (ignored here)",
            outcome.foreign
        ));
    }
    match runs
        .iter()
        .rev()
        .find(|run| run.op == "apply" && (run.verdict == "applied" || run.verdict == "partial"))
    {
        Some(run) => {
            let when = match &run.finished {
                Some(finished) if !finished.is_empty() => finished.clone(),
                _ => run.started.clone(),
            };
            lines.push(format!(
                "last apply: {when} freed {} over {} item(s) [{}]",
                account::human(run.freed_du),
                run.items,
                run.verdict
            ));
        }
        None => lines.push("last apply: none recorded".to_string()),
    }
    let interrupted = runs.iter().filter(|run| run.interrupted).count();
    if interrupted > 0 {
        lines.push(format!(
            "warning: {interrupted} run(s) started and never finished (see `ledger --view`)"
        ));
    }
    lines.push(format!(
        "reclaimable now: {} item(s), {} ⇒ {}",
        report.totals.items,
        account::human(report.totals.freed_du),
        if report.totals.items == 0 {
            "converged"
        } else {
            "reclaimable"
        }
    ));
    Ok(lines)
}

fn run_ledger_view(opts: &Opts) -> Result<(), Fail> {
    let Some(path) = &opts.ledger else {
        return Err(Fail::Usage(
            "`ledger --view` needs --ledger PATH (nothing is read from a default location)"
                .to_string(),
        ));
    };
    let outcome = ledger::view(path).map_err(|err| {
        Fail::Runtime(format!("cannot read the ledger {}: {err}", path.display()))
    })?;
    let runs = outcome.runs;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if opts.json {
        let array = J::A(
            runs.iter()
                .map(|run| {
                    J::obj(vec![
                        ("run_id", J::S(run.run_id.clone())),
                        ("op", J::S(run.op.clone())),
                        ("verdict", J::S(run.verdict.clone())),
                        ("target", J::S(run.target.clone())),
                        ("started", J::S(run.started.clone())),
                        ("finished", J::S(run.finished.clone().unwrap_or_default())),
                        ("items", J::U(run.items)),
                        ("bytes_du", J::U(run.freed_du)),
                        ("bytes_logical", J::U(run.logical)),
                        ("shared_du", J::U(run.shared_du)),
                        ("since", J::I(run.since.unwrap_or(0))),
                        ("interrupted", J::B(run.interrupted)),
                    ])
                })
                .collect(),
        );
        writeln!(out, "{}", array.render()).map_err(write_fail)?;
        return Ok(());
    }

    writeln!(out, "cargogc {VERSION} — ledger — {}", path.display()).map_err(write_fail)?;
    // Never let a shared ledger read as an empty one: if records exist that this tool did not
    // write, say how many and that they are not shown (measured 2026-10-10: a repo hook wrote
    // its own summary lines into the same file and the view printed "no runs recorded").
    if outcome.foreign > 0 {
        writeln!(
            out,
            "  ignored: {} record(s) not written by cargogc (another writer shares this ledger; not shown)",
            outcome.foreign
        )
        .map_err(write_fail)?;
    }
    if outcome.unparsable > 0 {
        writeln!(
            out,
            "  skipped: {} unparsable line(s) (a truncated final line is what a killed run leaves)",
            outcome.unparsable
        )
        .map_err(write_fail)?;
    }
    if runs.is_empty() {
        if outcome.foreign > 0 {
            writeln!(out, "  no cargogc runs recorded").map_err(write_fail)?;
        } else {
            writeln!(out, "  no runs recorded").map_err(write_fail)?;
        }
        return Ok(());
    }
    for run in &runs {
        writeln!(
            out,
            "  {:<20} {:<7} {:<11} {:>4} items {:>12} freed   target {}",
            run.started,
            run.op,
            run.verdict,
            run.items,
            account::human(run.freed_du),
            if run.target.is_empty() {
                "-"
            } else {
                &run.target
            }
        )
        .map_err(write_fail)?;
    }
    let interrupted = runs.iter().filter(|run| run.interrupted).count();
    if interrupted > 0 {
        writeln!(
            out,
            "  {interrupted} run(s) started and never finished — the process died mid-sweep; \
             re-check the tree before trusting the totals"
        )
        .map_err(write_fail)?;
    }
    Ok(())
}

fn write_fail(err: io::Error) -> Fail {
    Fail::Runtime(format!("cannot write the report: {err}"))
}

fn render(report: &Report, json: bool) -> Result<(), Fail> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let result = if json {
        report::render_json(report, &mut out)
    } else {
        report::render_human(report, &mut out)
    };
    result.map_err(write_fail)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

fn now_iso() -> String {
    report::format_ts_utc(now_epoch())
}

fn new_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_millis())
        .unwrap_or(0);
    format!("{millis}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sizes_require_a_unit_and_know_both_bases() {
        assert_eq!(parse_size("8GiB").unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("8GB").unwrap(), 8_000_000_000);
        assert_eq!(parse_size("512KiB").unwrap(), 512 * 1024);
        assert_eq!(parse_size("1B").unwrap(), 1);
        assert!(parse_size("8000").is_err(), "a bare number is ambiguous");
        assert!(parse_size("8XB").is_err());
        assert!(parse_size("GiB").is_err());
    }

    #[test]
    fn epochs_and_offset_iso_parse_but_ambiguous_forms_do_not() {
        assert_eq!(parse_timestamp("0").unwrap(), 0);
        assert_eq!(parse_timestamp("1791618600").unwrap(), 1_791_618_600);
        assert_eq!(
            parse_timestamp("2026-10-10T07:50:00Z").unwrap(),
            1_791_618_600
        );
        // 15:50 at +08:00 is the same instant as 07:50 UTC.
        assert_eq!(
            parse_timestamp("2026-10-10T15:50:00+08:00").unwrap(),
            1_791_618_600
        );
        assert_eq!(
            parse_timestamp("2026-10-10 15:50+0800").unwrap(),
            1_791_618_600
        );
        assert_eq!(
            parse_timestamp("2026-10-10T15:50:00-08:00").unwrap(),
            1_791_618_600 + 16 * 3600
        );
        assert_eq!(parse_timestamp("2026-10-10Z").unwrap(), 1_791_590_400);

        for bad in [
            "2h ago",
            "now",
            "yesterday",
            "2026-10-10T15:50:00", // local time: no offset and no timezone database
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-10T25:00:00Z",
            "",
        ] {
            let result = parse_timestamp(bad);
            assert!(result.is_err(), "{bad:?} must be refused, got {result:?}");
        }
    }

    #[test]
    fn the_arg_parser_rejects_conflicting_and_misplaced_options() {
        assert!(parse_args(&args(&["plan"])).is_ok());
        assert!(parse_args(&args(&[
            "plan",
            "--superseded-since",
            "1",
            "--last-build-file",
            "f"
        ]))
        .is_err());
        assert!(
            parse_args(&args(&["plan", "--reconcile"])).is_err(),
            "--reconcile needs a ledger"
        );
        assert!(parse_args(&args(&["plan", "--live-incremental-max", "8000"])).is_err());
        assert!(
            parse_args(&args(&["plan", "--superseded-since", "2h ago"])).is_ok(),
            "parsed later"
        );
        assert!(
            parse_args(&args(&["ledger"])).is_ok(),
            "ledger without --view is still a view"
        );
        assert!(parse_args(&args(&["ledger", "--target-dir", "x"])).is_err());
        assert!(parse_args(&args(&["nope"])).is_err());
        assert!(parse_args(&args(&[])).is_err());
    }

    #[test]
    fn plan_apply_is_an_alias_for_apply_and_repeatable_flags_accumulate() {
        let apply = parse_args(&args(&["plan", "--apply"])).unwrap();
        assert_eq!(apply.op, Op::Apply);
        let roots = parse_args(&args(&["apply", "--keep-newest-roots", "2"])).unwrap();
        assert_eq!(roots.keep_newest_roots, 2);
        let dead = parse_args(&args(&["apply", "--dead-dir", "a", "--dead-dir", "b"])).unwrap();
        assert_eq!(dead.dead_dirs.len(), 2);
        assert!(parse_args(&args(&["apply", "--keep-newest-roots", "-1"])).is_err());
    }
}
