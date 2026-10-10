//! The ledger: an append-only event log, plus the JSON writer and the minimal reader
//! that the `--json` output and `ledger --view` share.
//!
//! JSON is hand-written on purpose. This crate has exactly one dependency (`libc`, for
//! `flock`), because the published surface should be small enough to audit in one
//! sitting — a serialization framework is a lot of code to take on in order to print a
//! dozen fields. The reader only has to understand what the writer emits, and it fails
//! loudly on anything else instead of guessing.
//!
//! Event sourcing (`runs.jsonl`): one `start` per apply, one `finish` (or `skipped` for
//! a refusal). A `start` without a matching `finish` is reported as `interrupted` by
//! `ledger --view`, which is the honest reading — the process died, or was killed, in
//! the middle of a sweep.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

// ── writing ─────────────────────────────────────────────────────────────────

/// A JSON value, rendered compactly. Field order is insertion order, so the output is
/// stable and diffable.
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    S(String),
    I(i64),
    U(u64),
    B(bool),
    F(f64),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

impl J {
    pub fn obj(fields: Vec<(&str, J)>) -> J {
        J::O(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            J::S(value) => {
                out.push('"');
                out.push_str(&escape(value));
                out.push('"');
            }
            J::I(value) => out.push_str(&value.to_string()),
            J::U(value) => out.push_str(&value.to_string()),
            J::B(value) => out.push_str(if *value { "true" } else { "false" }),
            J::F(value) => {
                if value.is_finite() {
                    out.push_str(&format!("{value}"));
                } else {
                    // JSON has no infinity; a non-finite ratio is reported as null and
                    // explained in the human output instead of poisoning the document.
                    out.push_str("null");
                }
            }
            J::A(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    value.write(out);
                }
                out.push(']');
            }
            J::O(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape(key));
                    out.push_str("\":");
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// escape — the JSON string escape rules, applied to one string body.
pub fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

/// append — add one JSON line to the ledger, creating the parent directory if needed.
///
/// `path.parent()` is created because the documented location is a file inside a
/// repository's scratch directory, which may not exist on the first run.
pub fn append(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    if !line.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.flush()
}

// ── reading ─────────────────────────────────────────────────────────────────

/// A parsed JSON value, for reading back what [`J`] wrote.
#[derive(Debug, Clone, PartialEq)]
pub enum P {
    S(String),
    N(f64),
    B(bool),
    Null,
    A(Vec<P>),
    O(Vec<(String, P)>),
}

impl P {
    pub fn get(&self, key: &str) -> Option<&P> {
        match self {
            P::O(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            P::S(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            P::N(value) if *value >= 0.0 => Some(*value as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            P::N(value) => Some(*value as i64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[P]> {
        match self {
            P::A(values) => Some(values),
            _ => None,
        }
    }
}

/// parse — a strict reader for the subset this crate writes.
pub fn parse(input: &str) -> Result<P, String> {
    let bytes = input.as_bytes();
    let mut at = 0usize;
    let value = parse_value(bytes, &mut at)?;
    skip_ws(bytes, &mut at);
    if at != bytes.len() {
        return Err(format!("trailing bytes at offset {at}"));
    }
    Ok(value)
}

fn skip_ws(bytes: &[u8], at: &mut usize) {
    while *at < bytes.len() && matches!(bytes[*at], b' ' | b'\t' | b'\n' | b'\r') {
        *at += 1;
    }
}

fn parse_value(bytes: &[u8], at: &mut usize) -> Result<P, String> {
    skip_ws(bytes, at);
    let Some(&byte) = bytes.get(*at) else {
        return Err("unexpected end of input".to_string());
    };
    match byte {
        b'{' => parse_object(bytes, at),
        b'[' => parse_array(bytes, at),
        b'"' => Ok(P::S(parse_string(bytes, at)?)),
        b't' => expect(bytes, at, "true").map(|_| P::B(true)),
        b'f' => expect(bytes, at, "false").map(|_| P::B(false)),
        b'n' => expect(bytes, at, "null").map(|_| P::Null),
        _ => parse_number(bytes, at),
    }
}

fn expect(bytes: &[u8], at: &mut usize, literal: &str) -> Result<(), String> {
    if bytes[*at..].starts_with(literal.as_bytes()) {
        *at += literal.len();
        Ok(())
    } else {
        Err(format!("expected {literal} at offset {at}", at = *at))
    }
}

fn parse_object(bytes: &[u8], at: &mut usize) -> Result<P, String> {
    *at += 1; // '{'
    let mut fields = Vec::new();
    skip_ws(bytes, at);
    if bytes.get(*at) == Some(&b'}') {
        *at += 1;
        return Ok(P::O(fields));
    }
    loop {
        skip_ws(bytes, at);
        let key = parse_string(bytes, at)?;
        skip_ws(bytes, at);
        if bytes.get(*at) != Some(&b':') {
            return Err(format!("expected ':' at offset {}", *at));
        }
        *at += 1;
        let value = parse_value(bytes, at)?;
        fields.push((key, value));
        skip_ws(bytes, at);
        match bytes.get(*at) {
            Some(&b',') => {
                *at += 1;
            }
            Some(&b'}') => {
                *at += 1;
                return Ok(P::O(fields));
            }
            _ => return Err(format!("expected ',' or '}}' at offset {}", *at)),
        }
    }
}

fn parse_array(bytes: &[u8], at: &mut usize) -> Result<P, String> {
    *at += 1; // '['
    let mut values = Vec::new();
    skip_ws(bytes, at);
    if bytes.get(*at) == Some(&b']') {
        *at += 1;
        return Ok(P::A(values));
    }
    loop {
        values.push(parse_value(bytes, at)?);
        skip_ws(bytes, at);
        match bytes.get(*at) {
            Some(&b',') => {
                *at += 1;
            }
            Some(&b']') => {
                *at += 1;
                return Ok(P::A(values));
            }
            _ => return Err(format!("expected ',' or ']' at offset {}", *at)),
        }
    }
}

fn parse_string(bytes: &[u8], at: &mut usize) -> Result<String, String> {
    if bytes.get(*at) != Some(&b'"') {
        return Err(format!("expected a string at offset {}", *at));
    }
    *at += 1;
    let mut out = String::new();
    loop {
        let Some(&byte) = bytes.get(*at) else {
            return Err("unterminated string".to_string());
        };
        match byte {
            b'"' => {
                *at += 1;
                return Ok(out);
            }
            b'\\' => {
                *at += 1;
                let Some(&escaped) = bytes.get(*at) else {
                    return Err("unterminated escape".to_string());
                };
                match escaped {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{08}'),
                    b'f' => out.push('\u{0c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let hex = bytes
                            .get(*at + 1..*at + 5)
                            .ok_or_else(|| "truncated \\u escape".to_string())?;
                        let text =
                            std::str::from_utf8(hex).map_err(|_| "bad \\u escape".to_string())?;
                        let code = u32::from_str_radix(text, 16)
                            .map_err(|_| "bad \\u escape".to_string())?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        *at += 4;
                    }
                    other => return Err(format!("unknown escape \\{}", other as char)),
                }
                *at += 1;
            }
            _ => {
                // Copy the whole UTF-8 sequence: find its length from the leading byte.
                let len = utf8_len(byte);
                let end = (*at + len).min(bytes.len());
                let text = std::str::from_utf8(&bytes[*at..end])
                    .map_err(|_| "invalid utf-8".to_string())?;
                out.push_str(text);
                *at = end;
            }
        }
    }
}

fn utf8_len(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte >> 5 == 0b110 {
        2
    } else if byte >> 4 == 0b1110 {
        3
    } else if byte >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn parse_number(bytes: &[u8], at: &mut usize) -> Result<P, String> {
    let start = *at;
    while *at < bytes.len() && matches!(bytes[*at], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
    {
        *at += 1;
    }
    let text = std::str::from_utf8(&bytes[start..*at]).map_err(|_| "bad number".to_string())?;
    text.parse::<f64>()
        .map(P::N)
        .map_err(|_| format!("bad number at offset {start}: {text:?}"))
}

// ── the event log ───────────────────────────────────────────────────────────

/// One row of `ledger --view`.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    pub run_id: String,
    pub op: String,
    pub target: String,
    pub started: String,
    pub finished: Option<String>,
    pub verdict: String,
    pub items: u64,
    pub freed_du: u64,
    pub logical: u64,
    pub shared_du: u64,
    pub since: Option<i64>,
    /// A `start` with no matching `finish`: the process died mid-sweep.
    pub interrupted: bool,
}

/// What a view found, including what it could not use.
///
/// `foreign` and `unparsable` exist so "no runs recorded" can never be printed over a file
/// that demonstrably contains records: a ledger shared with another writer (measured
/// 2026-10-10: the repo's build hook wrote summary lines next to cargogc's events, and the
/// view reported "no runs recorded" while two sweeps had in fact reclaimed 10.81 GiB) must
/// say so instead of reading as "nothing ever ran".
#[derive(Debug, Default)]
pub struct ViewOutcome {
    pub runs: Vec<RunView>,
    /// Parseable JSON lines that are not cargogc events (another writer's records).
    pub foreign: usize,
    /// Lines that are not JSON at all — a killed process leaves a truncated last line.
    pub unparsable: usize,
}

/// view — fold the append-only log into one row per run.
///
/// Malformed lines are counted, not fatal: a truncated final line is what a killed process
/// leaves behind, and that is exactly the case this view exists to show.
pub fn view(path: &Path) -> io::Result<ViewOutcome> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(ViewOutcome::default())
        }
        Err(err) => return Err(err),
    };

    let mut order: Vec<String> = Vec::new();
    let mut rows: HashMap<String, RunView> = HashMap::new();
    let mut foreign = 0usize;
    let mut unparsable = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = parse(line) else {
            unparsable += 1;
            continue;
        };
        let evt = value
            .get("evt")
            .and_then(P::as_str)
            .unwrap_or("")
            .to_string();
        let run_id = value
            .get("run_id")
            .and_then(P::as_str)
            .unwrap_or("")
            .to_string();
        match evt.as_str() {
            "start" => {
                let row = RunView {
                    run_id: run_id.clone(),
                    op: value
                        .get("op")
                        .and_then(P::as_str)
                        .unwrap_or("apply")
                        .to_string(),
                    target: value
                        .get("target")
                        .and_then(P::as_str)
                        .unwrap_or("")
                        .to_string(),
                    started: value
                        .get("ts")
                        .and_then(P::as_str)
                        .unwrap_or("")
                        .to_string(),
                    finished: None,
                    verdict: "interrupted".to_string(),
                    items: 0,
                    freed_du: 0,
                    logical: 0,
                    shared_du: 0,
                    since: value.get("since").and_then(P::as_i64),
                    interrupted: true,
                };
                if !rows.contains_key(&run_id) {
                    order.push(run_id.clone());
                }
                rows.insert(run_id, row);
            }
            "finish" => {
                if let Some(row) = rows.get_mut(&run_id) {
                    row.finished = Some(
                        value
                            .get("ts")
                            .and_then(P::as_str)
                            .unwrap_or("")
                            .to_string(),
                    );
                    row.verdict = value
                        .get("verdict")
                        .and_then(P::as_str)
                        .unwrap_or("ok")
                        .to_string();
                    row.interrupted = false;
                    let mut items = 0u64;
                    let mut freed = 0u64;
                    let mut logical = 0u64;
                    let mut shared = 0u64;
                    if let Some(classes) = value.get("classes").and_then(P::as_array) {
                        for class in classes {
                            items += class.get("items").and_then(P::as_u64).unwrap_or(0);
                            freed += class.get("bytes_du").and_then(P::as_u64).unwrap_or(0);
                            logical += class.get("bytes_logical").and_then(P::as_u64).unwrap_or(0);
                            shared += class.get("shared_du").and_then(P::as_u64).unwrap_or(0);
                        }
                    }
                    row.items = items;
                    row.freed_du = freed;
                    row.logical = logical;
                    row.shared_du = shared;
                }
            }
            "skipped" => {
                let row = RunView {
                    run_id: run_id.clone(),
                    op: value
                        .get("op")
                        .and_then(P::as_str)
                        .unwrap_or("apply")
                        .to_string(),
                    target: value
                        .get("target")
                        .and_then(P::as_str)
                        .unwrap_or("")
                        .to_string(),
                    started: value
                        .get("ts")
                        .and_then(P::as_str)
                        .unwrap_or("")
                        .to_string(),
                    finished: Some(
                        value
                            .get("ts")
                            .and_then(P::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    verdict: "skipped".to_string(),
                    items: 0,
                    freed_du: 0,
                    logical: 0,
                    shared_du: 0,
                    since: value.get("since").and_then(P::as_i64),
                    interrupted: false,
                };
                if !rows.contains_key(&run_id) {
                    order.push(run_id.clone());
                }
                rows.insert(run_id, row);
            }
            "" => foreign += 1,
            _ => foreign += 1,
        }
    }

    Ok(ViewOutcome {
        runs: order
            .into_iter()
            .filter_map(|id| rows.remove(&id))
            .collect(),
        foreign,
        unparsable,
    })
}

/// default_ledger_hint — the path a caller most likely wants, for error messages only.
pub fn default_ledger_hint() -> PathBuf {
    PathBuf::from(".local/cargogc-runs.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_covers_the_json_escapes() {
        assert_eq!(escape("a\"b\\c\nd\te"), "a\\\"b\\\\c\\nd\\te");
        assert_eq!(escape("\u{1}"), "\\u0001");
        assert_eq!(escape("成本"), "成本", "non-ASCII passes through as UTF-8");
    }

    #[test]
    fn writer_and_reader_round_trip() {
        let value = J::obj(vec![
            ("evt", J::S("finish".into())),
            ("items", J::U(3)),
            ("since", J::I(-5)),
            ("ok", J::B(true)),
            ("ratio", J::F(1.5)),
            (
                "classes",
                J::A(vec![J::obj(vec![("kind", J::S("dead".into()))])]),
            ),
        ]);
        let text = value.render();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.get("evt").unwrap().as_str(), Some("finish"));
        assert_eq!(parsed.get("items").unwrap().as_u64(), Some(3));
        assert_eq!(parsed.get("since").unwrap().as_i64(), Some(-5));
        assert_eq!(parsed.get("classes").unwrap().as_array().unwrap().len(), 1);
        assert_eq!(
            parsed.get("classes").unwrap().as_array().unwrap()[0]
                .get("kind")
                .unwrap()
                .as_str(),
            Some("dead")
        );
    }

    #[test]
    fn the_reader_rejects_garbage_instead_of_guessing() {
        assert!(parse("{").is_err());
        assert!(parse("{\"a\":1} trailing").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("\"unterminated").is_err());
    }

    #[test]
    fn view_pairs_start_with_finish_and_flags_a_dangling_start() {
        let dir = std::env::temp_dir().join(format!("cargogc-ledger-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runs.jsonl");

        append(
            &path,
            &J::obj(vec![
                ("evt", J::S("start".into())),
                ("run_id", J::S("run-a".into())),
                ("ts", J::S("2026-10-10T00:00:00Z".into())),
                ("op", J::S("apply".into())),
                ("target", J::S("/tmp/target".into())),
                ("since", J::I(100)),
            ])
            .render(),
        )
        .unwrap();
        append(
            &path,
            &J::obj(vec![
                ("evt", J::S("finish".into())),
                ("run_id", J::S("run-a".into())),
                ("ts", J::S("2026-10-10T00:00:02Z".into())),
                ("verdict", J::S("applied".into())),
                (
                    "classes",
                    J::A(vec![J::obj(vec![
                        ("kind", J::S("superseded-root".into())),
                        ("items", J::U(4)),
                        ("bytes_du", J::U(1000)),
                        ("bytes_logical", J::U(1200)),
                        ("shared_du", J::U(200)),
                    ])]),
                ),
            ])
            .render(),
        )
        .unwrap();
        // A dangling start: the shape a killed process leaves behind.
        append(
            &path,
            &J::obj(vec![
                ("evt", J::S("start".into())),
                ("run_id", J::S("run-b".into())),
                ("ts", J::S("2026-10-10T00:01:00Z".into())),
                ("op", J::S("apply".into())),
                ("target", J::S("/tmp/target".into())),
            ])
            .render(),
        )
        .unwrap();

        let rows = view(&path).unwrap().runs;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].verdict, "applied");
        assert_eq!(rows[0].items, 4);
        assert_eq!(rows[0].freed_du, 1000);
        assert_eq!(rows[0].logical, 1200);
        assert!(!rows[0].interrupted);
        assert_eq!(rows[1].verdict, "interrupted");
        assert!(rows[1].interrupted);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shared_ledger_reports_foreign_records_instead_of_looking_empty() {
        // Measured 2026-10-10: a repo's build hook wrote its own summary lines next to
        // cargogc's events, and the view printed "no runs recorded" while two sweeps had in
        // fact reclaimed 10.81 GiB. "Nothing ran" and "nothing I wrote ran" must not look
        // alike — that is the entire lesson of this project.
        let dir = std::env::temp_dir().join(format!("cargogc-ledger-foreign-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runs.jsonl");
        let foreign = r#"{"ts":"2026-10-10T15:35:10Z","op":"sweep","target":"/x","superseded":{"items":164,"bytes_du":9176412160},"verdict":"ok"}"#;
        fs::write(&path, format!("{foreign}\n{{\"evt\":\"start\"\n")).unwrap();

        let outcome = view(&path).unwrap();
        assert!(outcome.runs.is_empty(), "no cargogc events were written");
        assert_eq!(outcome.foreign, 1, "the foreign summary line must be counted");
        assert_eq!(outcome.unparsable, 1, "the truncated line must be counted");
    }

    #[test]
    fn view_of_a_missing_ledger_is_empty_not_an_error() {
        let path = std::env::temp_dir().join("cargogc-ledger-does-not-exist/runs.jsonl");
        let outcome = view(&path).unwrap();
        assert!(outcome.runs.is_empty());
        assert_eq!(outcome.foreign, 0);
        assert_eq!(outcome.unparsable, 0);
    }
}
