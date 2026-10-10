//! What the fork stores beside upstream's tables (DDL in `fork_schema.rs`).
//!
//! [`attach`] runs once per line of a known entry type, after upstream's rows
//! are built, and reads the raw line only:
//!
//! 1. Promotion. The fields that have a typed table are read from the raw
//!    value with lenient accessors: a `BIGINT` column is filled only from an
//!    integer that fits 64 signed bits, a `TEXT` column only from a JSON
//!    string, a `BOOLEAN` column only from a JSON boolean, and a `JSON` column
//!    from any value through `json_str`. Anything else leaves the column NULL.
//!    A row is written when at least one of its columns is not NULL.
//! 2. Exact or the raw stays. For each promoted place the value is rebuilt
//!    from the row (a NULL column contributes no key) and compared with the
//!    raw value. Only an equal value is laid over the typed round trip.
//! 3. Catch-all. The residual of the raw line against that round trip is the
//!    line's `entry_extras` row. A promoted value that was not exact is
//!    therefore still in it, whole, beside the columns that could be read.
//!
//! Promotion goes by key, not by entry type: a line of any type that carries
//! a promoted key gets the row.
//!
//! [`Drift::unparsed`] builds the `unparsed_lines` row of a line whose `type`
//! the parser does not know. [`Drift`] counts, per file and then per ingest,
//! what went into the catch-all; [`write_summary`] stores those counts in
//! `ingest_drift` and `ingest_meta`, and [`Drift::report`] prints the largest.
//!
//! Nothing here can fail a line: no function returns an error or panics, and
//! if the typed entry cannot be serialised back the whole raw line is stored
//! and counted. The one exception is asked for: with `CCT_VERIFY_RESIDUAL=1`
//! every line is checked (the round trip with the promoted values, merged
//! with the line's `extra`, must reproduce every leaf of the raw line) and a
//! line that fails the check ends the process with exit code 3.

#![deny(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use claude_code_transcripts::residual::{
    json_len, merge, numbers_equal, residual_with_places, unreproduced, StoreReason, StoredPlace,
};
use claude_code_transcripts::types::Entry;
use duckdb::{params, Connection};
use serde_json::{Map, Value};

use crate::parse::{json_str, EntryRows};

// ─── Cells ───────────────────────────────────────────────────────────────

fn text(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(s)) => Value::String(s.clone()),
        _ => Value::Null,
    }
}

fn int(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_i64) {
        Some(i) => Value::from(i),
        None => Value::Null,
    }
}

fn boolean(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Bool(b)) => Value::Bool(*b),
        _ => Value::Null,
    }
}

/// A JSON column: SQL NULL for an absent key, JSON text for any value.
fn json(value: Option<&Value>) -> Value {
    value.map_or(Value::Null, json_str)
}

/// What a scalar cell gives back when its row is read: nothing for NULL.
fn present(cell: &Value) -> Option<Value> {
    (!cell.is_null()).then(|| cell.clone())
}

/// The object a row gives back: one key per member that is there. `None`
/// when no member is, because such a row cannot tell an empty object from an
/// absent key.
fn object_of<const N: usize>(members: [(&str, Option<Value>); N]) -> Option<Value> {
    let map: Map<String, Value> = members
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key.to_owned(), v)))
        .collect();
    (!map.is_empty()).then_some(Value::Object(map))
}

/// Equality of a raw value and the value rebuilt from a row: the same keys,
/// the same lengths, numbers by the Numbers rule.
fn same(raw: &Value, rebuilt: &Value) -> bool {
    match (raw, rebuilt) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, va)| b.get(key).is_some_and(|vb| same(va, vb)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(va, vb)| same(va, vb))
        }
        (Value::Number(a), Value::Number(b)) => numbers_equal(a, b),
        _ => raw == rebuilt,
    }
}

// ─── Promotion ───────────────────────────────────────────────────────────

/// Where a promoted value sits in the line.
enum Place {
    /// A key of the line.
    Top(&'static str),
    /// A key of the line's `message` object.
    Message(&'static str),
}

struct Promotion<'a> {
    raw: &'a Value,
    /// Rows for the fork's tables, `entry_id` placeholder at index 0.
    children: Vec<(&'static str, Vec<Vec<Value>>)>,
    /// The values the rows give back exactly.
    exact: Vec<(Place, Value)>,
}

impl<'a> Promotion<'a> {
    /// Records a promoted place as exact when the value rebuilt from its row
    /// equals the raw one.
    fn settle(&mut self, place: Place, raw: Option<&Value>, rebuilt: Option<Value>) {
        if let (Some(raw), Some(rebuilt)) = (raw, rebuilt) {
            if same(raw, &rebuilt) {
                self.exact.push((place, rebuilt));
            }
        }
    }

    /// Adds a one-row table entry unless every cell is NULL.
    fn row(&mut self, table: &'static str, cells: Vec<Value>) {
        if cells.iter().all(Value::is_null) {
            return;
        }
        let mut row = Vec::with_capacity(cells.len() + 1);
        row.push(Value::Null);
        row.extend(cells);
        self.children.push((table, vec![row]));
    }

    /// `assistant_turn_fields` and `assistant_input_transformations`.
    fn turn_fields(&mut self) {
        let raw = self.raw;
        let duration_raw = raw.get("thinkingDurationMs");
        let duration = int(duration_raw);
        self.settle(
            Place::Top("thinkingDurationMs"),
            duration_raw,
            present(&duration),
        );
        let advisor_raw = raw.get("advisorModel");
        let advisor = text(advisor_raw);
        self.settle(Place::Top("advisorModel"), advisor_raw, present(&advisor));

        // The count is the length of the raw list whatever its items are. One
        // row per item that is an object, at its position in the raw list.
        let list_raw = raw
            .get("message")
            .and_then(|message| message.get("input_transformations"));
        let mut count = Value::Null;
        if let Some(Value::Array(items)) = list_raw {
            count = Value::from(items.len());
            let mut item_rows = Vec::new();
            let mut rebuilt = Vec::new();
            for (position, item) in items.iter().enumerate() {
                if !item.is_object() {
                    continue;
                }
                let cells = [
                    ("type", text(item.get("type"))),
                    ("path", text(item.get("path"))),
                    ("reason", text(item.get("reason"))),
                ];
                let mut row = vec![Value::Null, Value::from(position)];
                let mut members = Map::new();
                for (key, cell) in cells {
                    if !cell.is_null() {
                        members.insert(key.to_owned(), cell.clone());
                    }
                    row.push(cell);
                }
                item_rows.push(row);
                rebuilt.push(Value::Object(members));
            }
            if !item_rows.is_empty() {
                self.children
                    .push(("assistant_input_transformations", item_rows));
            }
            self.settle(
                Place::Message("input_transformations"),
                list_raw,
                Some(Value::Array(rebuilt)),
            );
        }
        self.row("assistant_turn_fields", vec![duration, advisor, count]);
    }

    /// `user_turn_fields`.
    fn user_fields(&mut self) {
        let raw = self.raw;
        let origin_raw = raw.get("turnOrigin");
        let origin = text(origin_raw);
        self.settle(Place::Top("turnOrigin"), origin_raw, present(&origin));

        let position_raw = raw.get("turnPosition");
        let member = |key: &str| position_raw.and_then(|position| position.get(key));
        let prompt_index = int(member("promptIndex"));
        let turn_index = int(member("turnIndex"));
        self.settle(
            Place::Top("turnPosition"),
            position_raw,
            object_of([
                ("promptIndex", present(&prompt_index)),
                ("turnIndex", present(&turn_index)),
            ]),
        );
        self.row("user_turn_fields", vec![origin, prompt_index, turn_index]);
    }

    /// `user_classifier_contexts`: `request_id` and `context` rebuild the
    /// raw object; the other columns are projections of `context`.
    fn classifier_context(&mut self) {
        let Some(whole) = self.raw.get("serverClassifierContext") else {
            return;
        };
        let request = text(whole.get("request"));
        let context = whole.get("context");
        let member = |key: &str| context.and_then(|c| c.get(key));
        let git = member("git_state");
        let git_member = |key: &str| git.and_then(|g| g.get(key));
        let clean = git_member("status").and_then(|status| status.get("clean"));
        self.settle(
            Place::Top("serverClassifierContext"),
            Some(whole),
            object_of([
                ("request", present(&request)),
                ("context", context.cloned()),
            ]),
        );
        self.row(
            "user_classifier_contexts",
            vec![
                request,
                text(member("live_cwd")),
                text(member("platform")),
                text(git_member("root")),
                text(git_member("branch")),
                boolean(clean),
                json(context),
            ],
        );
    }

    /// `attachment_rendered`.
    fn rendered(&mut self) {
        let raw = self.raw;
        let role_raw = raw.get("renderedRole");
        let role = text(role_raw);
        self.settle(Place::Top("renderedRole"), role_raw, present(&role));
        let rendered = raw.get("rendered");
        self.settle(Place::Top("rendered"), rendered, rendered.cloned());
        self.row("attachment_rendered", vec![role, json(rendered)]);
    }

    /// `attachment_thinking_drops` and `attachment_prompt_snapshots`: the
    /// two attachment types whose whole payload lives in a typed table.
    fn attachment(&mut self) {
        let Some(whole @ Value::Object(members)) = self.raw.get("attachment") else {
            return;
        };
        match members.get("type").and_then(Value::as_str) {
            Some("thinking_drop") => {
                // `payload` is the whole object; the other columns are
                // projections of it.
                let member = |key: &str| members.get(key);
                let dropped = |key: &str| member("newlyDropped").and_then(|d| d.get(key));
                self.row(
                    "attachment_thinking_drops",
                    vec![
                        text(member("model")),
                        text(member("requestId")),
                        text(member("querySource")),
                        int(member("thinkingBlocksSent")),
                        int(member("thinkingTurnsSent")),
                        int(dropped("blockCount")),
                        int(dropped("turnCount")),
                        text(dropped("reason")),
                        json_str(whole),
                    ],
                );
                self.settle(Place::Top("attachment"), Some(whole), Some(whole.clone()));
            }
            Some("prompt_snapshot") => {
                // `settings` is the object without the members promoted
                // beside it, so the four columns rebuild it.
                let model = text(members.get("model"));
                let system_prompt = members.get("systemPrompt");
                let tools = members.get("tools");
                let beside = |key: &str| {
                    key == "systemPrompt" || key == "tools" || (key == "model" && !model.is_null())
                };
                let rest: Map<String, Value> = members
                    .iter()
                    .filter(|(key, _)| !beside(key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                let mut rebuilt = Value::Object(rest);
                let settings = json_str(&rebuilt);
                for (key, value) in [
                    ("model", present(&model)),
                    ("systemPrompt", system_prompt.cloned()),
                    ("tools", tools.cloned()),
                ] {
                    if let (Value::Object(map), Some(value)) = (&mut rebuilt, value) {
                        map.insert(key.to_owned(), value);
                    }
                }
                self.row(
                    "attachment_prompt_snapshots",
                    vec![model, json(system_prompt), json(tools), settings],
                );
                self.settle(Place::Top("attachment"), Some(whole), Some(rebuilt));
            }
            _ => {}
        }
    }

    /// `attachment_raw_types`: the real type name of an attachment that
    /// upstream's row calls `unknown`.
    fn raw_type(&mut self, rows: &EntryRows) {
        let written = match &rows.variant {
            Some(("attachment_entries", row)) => row.get(1).and_then(Value::as_str),
            _ => None,
        };
        if written != Some("unknown") {
            return;
        }
        let name = self.raw.get("attachment").and_then(|a| a.get("type"));
        self.children
            .push(("attachment_raw_types", vec![vec![Value::Null, text(name)]]));
    }
}

/// Lays a value the typed tables hold over the round trip, so that the
/// residual walk finds nothing lost at that place.
fn lay(round_trip: &mut Value, place: Place, value: Value) {
    let (parent, key) = match place {
        Place::Top(key) => (Some(round_trip), key),
        Place::Message(key) => (round_trip.get_mut("message"), key),
    };
    if let Some(Value::Object(map)) = parent {
        map.insert(key.to_owned(), value);
    }
}

/// Adds the fork's rows of one line to `rows` and counts what it stored.
pub fn attach(raw: &Value, entry: &Entry, rows: &mut EntryRows, drift: &mut Drift) {
    drift.lines_read += 1;
    let mut promotion = Promotion {
        raw,
        children: Vec::new(),
        exact: Vec::new(),
    };
    promotion.turn_fields();
    promotion.user_fields();
    promotion.classifier_context();
    promotion.rendered();
    promotion.attachment();
    promotion.raw_type(rows);
    let Promotion {
        mut children,
        exact,
        ..
    } = promotion;

    let round_trip = serde_json::to_value(entry).ok().map(|mut round_trip| {
        for (place, value) in exact {
            lay(&mut round_trip, place, value);
        }
        round_trip
    });
    let (extra, places) = extra_of(raw, round_trip.as_ref(), drift);
    if let (Some(verify), Some(round_trip)) = (verify(), round_trip) {
        verify.line(raw, round_trip, extra.as_ref(), rows);
    }
    if let Some(extra) = &extra {
        children.push(("entry_extras", vec![vec![Value::Null, json_str(extra)]]));
        drift.extras_rows += 1;
    }

    let entry_type = rows.entry.get(3).and_then(Value::as_str).unwrap_or("");
    let attachment_type = if entry_type == "attachment" {
        raw.get("attachment")
            .and_then(|a| a.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
    } else {
        ""
    };
    drift.line(entry_type, attachment_type, &places);
    rows.children.append(&mut children);
}

/// The `extra` of a line and the places it stores. Without a round trip (the
/// typed entry could not be serialised back) that is the whole raw line.
fn extra_of(
    raw: &Value,
    round_trip: Option<&Value>,
    drift: &mut Drift,
) -> (Option<Value>, Vec<StoredPlace>) {
    let mut places = Vec::new();
    match round_trip {
        Some(round_trip) => {
            let extra = residual_with_places(raw, round_trip, &mut places);
            (extra, places)
        }
        None => {
            drift.reserialize_errors += 1;
            places.push(StoredPlace {
                path: "(root)".to_owned(),
                bytes: json_len(raw),
                reason: StoreReason::Differs,
            });
            (Some(raw.clone()), places)
        }
    }
}

// ─── Verify ──────────────────────────────────────────────────────────────

/// The merge check, on when the environment holds `CCT_VERIFY_RESIDUAL=1`.
struct Verify {
    /// `CCT_VERIFY_DROP_PATH`: object keys, joined by dots, of one place that
    /// is removed from a line's `extra` before the check. Read by nothing
    /// else; it exists to show that the check can fail.
    drop_path: Option<String>,
}

fn verify() -> Option<&'static Verify> {
    static VERIFY: OnceLock<Option<Verify>> = OnceLock::new();
    VERIFY
        .get_or_init(|| {
            let on = std::env::var("CCT_VERIFY_RESIDUAL").is_ok_and(|value| value == "1");
            on.then(|| Verify {
                drop_path: std::env::var("CCT_VERIFY_DROP_PATH").ok(),
            })
        })
        .as_ref()
}

impl Verify {
    /// Ends the process with exit code 3 unless merging `extra` into the
    /// round trip reproduces every leaf of the raw line. The message is not
    /// the parse-failure one, which callers read as "this file is bad".
    fn line(&self, raw: &Value, round_trip: Value, extra: Option<&Value>, rows: &EntryRows) {
        let mut stored = extra.cloned();
        if let (Some(path), Some(stored)) = (&self.drop_path, &mut stored) {
            drop_place(stored, path);
        }
        let merged = merge(round_trip, stored.as_ref());
        if unreproduced(raw, &merged).is_empty() {
            return;
        }
        let file = rows.entry.get(1).and_then(Value::as_str).unwrap_or("?");
        let line_no = rows.entry.get(2).and_then(Value::as_i64).unwrap_or(0);
        eprintln!("error: residual invariant violated in {file}:{line_no}");
        std::process::exit(3);
    }
}

/// Removes the value at `path` (object keys joined by dots) when it is there.
fn drop_place(value: &mut Value, path: &str) {
    let (parents, last) = match path.rsplit_once('.') {
        Some((parents, last)) => (Some(parents), last),
        None => (None, path),
    };
    let mut at = Some(value);
    for key in parents.into_iter().flat_map(|parents| parents.split('.')) {
        at = at.and_then(|value| value.get_mut(key));
    }
    if let Some(Value::Object(map)) = at {
        map.remove(last);
    }
}

// ─── Drift ───────────────────────────────────────────────────────────────

/// Entry type, attachment type (empty when the line is not an attachment)
/// and stored place: the key of `ingest_drift`.
type PlaceKey = (String, String, String);

/// Places `ingest_drift` keeps under their own name.
const DRIFT_ROWS: usize = 2000;
/// The place that collects, per entry type, what is past `DRIFT_ROWS`.
const OTHER_PLACES: &str = "<other>";
/// Places the stderr report lists.
const REPORT_ROWS: usize = 20;

/// Counts of what the catch-all holds: lines and bytes per entry type,
/// attachment type and stored place, and the line counters of `ingest_meta`.
#[derive(Default)]
pub struct Drift {
    /// Place to (lines, bytes). Not capped while counting; the cap is
    /// applied when the table is written.
    places: HashMap<PlaceKey, (u64, u64)>,
    lines_read: u64,
    extras_rows: u64,
    reserialize_errors: u64,
    /// `unparsed_lines` rows the writer appended.
    unparsed_rows: u64,
}

impl Drift {
    /// The `unparsed_lines` row of a line whose `type` the parser does not
    /// know: `file_path`, `line_no`, `raw_type`, `session_id`, and the line
    /// text as written in `payload`. Counts the line under the place `(line)`.
    pub fn unparsed(
        &mut self,
        file_path: &str,
        line_no: usize,
        raw: &Value,
        line: &str,
    ) -> Vec<Value> {
        self.lines_read += 1;
        let raw_type = text(raw.get("type"));
        self.add(raw_type.as_str().unwrap_or(""), "", "(line)", line.len());
        vec![
            Value::String(file_path.to_owned()),
            Value::from(line_no),
            raw_type,
            text(raw.get("sessionId")),
            Value::String(line.to_owned()),
        ]
    }

    fn add(&mut self, entry_type: &str, attachment_type: &str, path: &str, bytes: usize) {
        let key = (
            entry_type.to_owned(),
            attachment_type.to_owned(),
            path.to_owned(),
        );
        let slot = self.places.entry(key).or_default();
        slot.0 += 1;
        slot.1 += bytes as u64;
    }

    /// The stored places of one line. A place stored several times on the
    /// line (once per array element) counts as one line and the sum of its
    /// bytes.
    fn line(&mut self, entry_type: &str, attachment_type: &str, places: &[StoredPlace]) {
        let mut seen: Vec<(&str, usize)> = Vec::new();
        for place in places {
            match seen.iter_mut().find(|(path, _)| *path == place.path) {
                Some(slot) => slot.1 += place.bytes,
                None => seen.push((&place.path, place.bytes)),
            }
        }
        for (path, bytes) in seen {
            self.add(entry_type, attachment_type, path, bytes);
        }
    }

    /// Adds the counts of one file, and the number of its `unparsed_lines`
    /// rows that were written.
    pub fn absorb(&mut self, file: Drift, unparsed_rows: usize) {
        for (key, (lines, bytes)) in file.places {
            let slot = self.places.entry(key).or_default();
            slot.0 += lines;
            slot.1 += bytes;
        }
        self.lines_read += file.lines_read;
        self.extras_rows += file.extras_rows;
        self.reserialize_errors += file.reserialize_errors;
        self.unparsed_rows += unparsed_rows as u64;
    }

    /// Every place, most bytes first; ties by entry type, attachment type
    /// and place, so that the order does not depend on the hash map.
    fn ranked(&self) -> Vec<(&PlaceKey, (u64, u64))> {
        let mut rows: Vec<(&PlaceKey, (u64, u64))> = self
            .places
            .iter()
            .map(|(key, counts)| (key, *counts))
            .collect();
        rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then_with(|| a.0.cmp(b.0)));
        rows
    }

    /// The rows of `ingest_drift`: the `DRIFT_ROWS` largest places under
    /// their own key, the rest summed into one `<other>` row per entry type.
    /// Keyed by the table's key, so a real place spelled `<other>` shares
    /// that row instead of repeating the key.
    fn capped(&self) -> BTreeMap<PlaceKey, (u64, u64)> {
        let mut rows: BTreeMap<PlaceKey, (u64, u64)> = BTreeMap::new();
        for (rank, (key, (lines, bytes))) in self.ranked().into_iter().enumerate() {
            let key = if rank < DRIFT_ROWS {
                key.clone()
            } else {
                (key.0.clone(), String::new(), OTHER_PLACES.to_owned())
            };
            let slot = rows.entry(key).or_default();
            slot.0 += lines;
            slot.1 += bytes;
        }
        rows
    }

    /// Prints the catch-all counters and its largest places to stderr.
    pub fn report(&self) {
        eprintln!(
            "Catch-all:    rows={}  unparsed_lines={}  reserialize_errors={}",
            self.extras_rows, self.unparsed_rows, self.reserialize_errors
        );
        let ranked = self.ranked();
        if ranked.is_empty() {
            return;
        }
        eprintln!(
            "Catch-all places (top {} of {} by bytes):",
            ranked.len().min(REPORT_ROWS),
            ranked.len()
        );
        for ((entry_type, attachment_type, path), (lines, bytes)) in
            ranked.into_iter().take(REPORT_ROWS)
        {
            let separator = if attachment_type.is_empty() { "" } else { "/" };
            eprintln!(
                "  - {entry_type}{separator}{attachment_type}  {path}  (lines={lines}, bytes={bytes})"
            );
        }
    }
}

fn bigint(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Fills `ingest_drift` and `ingest_meta` once the load is committed.
/// `entries` is the number of `entries` rows the writer appended.
pub fn write_summary(conn: &Connection, drift: &Drift, entries: i64) -> Result<(), String> {
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| format!("begin ingest summary: {e}"))?;
    {
        let mut stmt = conn
            .prepare(
                "INSERT INTO ingest_drift (entry_type, attachment_type, path, lines, bytes)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .map_err(|e| format!("prep ingest_drift: {e}"))?;
        for ((entry_type, attachment_type, path), (lines, bytes)) in drift.capped() {
            stmt.execute(params![
                entry_type,
                attachment_type,
                path,
                bigint(lines),
                bigint(bytes)
            ])
            .map_err(|e| format!("insert ingest_drift: {e}"))?;
        }

        let command = <crate::cli::Cli as clap::CommandFactory>::command();
        let meta = [
            (
                "cct_version",
                command.get_version().unwrap_or("").to_owned(),
            ),
            (
                "upstream_tag",
                concat!("v", env!("CARGO_PKG_VERSION")).to_owned(),
            ),
            ("lines_read", drift.lines_read.to_string()),
            ("entries", entries.to_string()),
            ("extras_rows", drift.extras_rows.to_string()),
            ("unparsed_lines", drift.unparsed_rows.to_string()),
            ("reserialize_errors", drift.reserialize_errors.to_string()),
        ];
        let mut stmt = conn
            .prepare("INSERT INTO ingest_meta (key, value) VALUES (?, ?)")
            .map_err(|e| format!("prep ingest_meta: {e}"))?;
        for (key, value) in meta {
            stmt.execute(params![key, value])
                .map_err(|e| format!("insert ingest_meta: {e}"))?;
        }
    }
    conn.execute_batch("COMMIT")
        .map_err(|e| format!("commit ingest summary: {e}"))
}
