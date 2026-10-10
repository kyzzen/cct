//! End-to-end test: ingest the synthetic `catch_all` fixture directory and
//! assert what lands in the existing relations and in the fork's tables. The
//! fixture holds one line per way a transcript line can carry something the
//! typed parser does not model (see `tests/fixtures/catch_all/`):
//! `catch_all.jsonl` mixes known and unknown entry types, `unknown_only.jsonl`
//! holds unknown entry types only.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use claude_code_transcripts::residual::{merge, unreproduced};
use claude_code_transcripts::types::Entry;
use claude_code_transcripts_ingest::cli::IngestArgs;
use claude_code_transcripts_ingest::run::run_ingest;
use duckdb::Connection;
use serde_json::{json, Map, Value};
use tempfile::TempDir;

/// Entry types in the fixture that the typed parser does not know.
const UNKNOWN_ENTRY_TYPES: [&str; 3] = ["cost-state", "zz-future-entry", "continued-in"];

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catch_all")
}

/// 1-based line numbers of the main fixture file whose `type` is known.
fn known_line_numbers() -> Vec<i64> {
    let text = fs::read_to_string(fixture_dir().join("catch_all.jsonl")).expect("read fixture");
    text.lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("fixture line is json");
            let ty = value.get("type").and_then(|t| t.as_str()).expect("type");
            (!UNKNOWN_ENTRY_TYPES.contains(&ty)).then_some(idx as i64 + 1)
        })
        .collect()
}

#[test]
fn catch_all_fixture_ingests() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("out.duckdb");

    let args = IngestArgs {
        input_dir: fixture_dir(),
        jobs: 1,
        output: db_path.clone(),
        pricing: None,
        no_progress: true,
    };

    let summary = run_ingest(args).expect("ingest succeeds");

    let known = known_line_numbers();
    assert_eq!(known.len(), 57, "fixture: lines of known type");

    let conn = Connection::open(&db_path).expect("open db");

    // One `entries` row per line of known type, at that line's number.
    let line_nos: Vec<i64> = conn
        .prepare("SELECT line_no FROM entries ORDER BY line_no")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(line_nos, known);

    let by_type: BTreeMap<String, i64> = conn
        .prepare("SELECT type, COUNT(*) FROM entries GROUP BY type")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let expected_by_type: BTreeMap<String, i64> = [
        ("assistant", 14),
        ("attachment", 14),
        ("mode", 2),
        ("system", 3),
        ("user", 24),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(by_type, expected_by_type);

    // Only the file that holds a line of known type gets a `transcripts` row.
    let transcripts: Vec<(String, i64)> = conn
        .prepare("SELECT file_path, entry_count FROM transcripts")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(transcripts.len(), 1, "transcripts: {transcripts:?}");
    assert!(transcripts[0].0.ends_with("catch_all.jsonl"));
    assert_eq!(transcripts[0].1, 57);

    let file_paths: i64 = conn
        .query_row("SELECT COUNT(DISTINCT file_path) FROM entries", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(file_paths, 1);

    // Attachment types the parser does not know are written as `unknown`.
    let attachments: Vec<(i64, String)> = conn
        .prepare(
            "SELECT e.line_no, a.attachment_type \
             FROM attachment_entries a JOIN entries e USING (entry_id) \
             ORDER BY e.line_no",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let expected_attachments: Vec<(i64, String)> = [
        (10, "date_change"),
        (11, "date_change"),
        (12, "unknown"),
        (13, "unknown"),
        (14, "unknown"),
        (15, "unknown"),
        (31, "unknown"),
        (37, "unknown"),
        (39, "unknown"),
        (55, "date_change"),
        (56, "unknown"),
        (57, "unknown"),
        (58, "unknown"),
        (59, "date_change"),
    ]
    .into_iter()
    .map(|(line, ty)| (line, ty.to_string()))
    .collect();
    assert_eq!(attachments, expected_attachments);

    // The parser's own counters for the fallbacks it took.
    let mut unknown_variants: Vec<(String, u64)> = summary.unknown_variants.into_iter().collect();
    unknown_variants.sort();
    let expected_variants: Vec<(String, u64)> = [
        ("AssistantContentBlock", 1),
        ("AttachmentData", 10),
        ("DocumentSource", 1),
        ("ImageSource", 1),
        ("UserContentBlock", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(unknown_variants, expected_variants);

    fork_tables_exist(&conn);

    let raw = fixture_lines();
    catch_all_rows(&conn, &raw);
    typed_rows(&conn, &raw);
    nothing_is_lost(&conn, &raw);
    unparsed_rows(&conn, &transcripts[0].0);
    drift_and_meta(&conn, &raw);
}

/// Every cell of a query result as text; SQL NULL reads `NULL`.
fn query(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
    use duckdb::types::Value as Cell;
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut rows = stmt.query([]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        let mut cells = Vec::new();
        let mut i = 0;
        while let Ok(cell) = row.get::<_, Cell>(i) {
            cells.push(match cell {
                Cell::Null => "NULL".to_string(),
                Cell::Boolean(b) => b.to_string(),
                Cell::Int(n) => n.to_string(),
                Cell::BigInt(n) => n.to_string(),
                Cell::HugeInt(n) => n.to_string(),
                Cell::Text(s) => s,
                other => panic!("{sql}: column {i} is {other:?}"),
            });
            i += 1;
        }
        out.push(cells);
    }
    out
}

fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|cell| cell.to_string()).collect())
        .collect()
}

/// The twelve tables the fork adds, their columns in order, and one unique
/// index per table on its key.
fn fork_tables_exist(conn: &Connection) {
    let columns = query(
        conn,
        "SELECT table_name, string_agg(column_name || ' ' || data_type, ', ' ORDER BY ordinal_position) \
         FROM information_schema.columns \
         WHERE table_name IN ('entry_extras', 'unparsed_lines', 'attachment_raw_types', \
             'assistant_turn_fields', 'assistant_input_transformations', 'user_turn_fields', \
             'user_classifier_contexts', 'attachment_rendered', 'attachment_thinking_drops', \
             'attachment_prompt_snapshots', 'ingest_drift', 'ingest_meta') \
         GROUP BY table_name ORDER BY table_name",
    );
    let expected = table(&[
        &[
            "assistant_input_transformations",
            "entry_id BIGINT, position INTEGER, transformation_type VARCHAR, path VARCHAR, \
             reason VARCHAR",
        ],
        &[
            "assistant_turn_fields",
            "entry_id BIGINT, thinking_duration_ms BIGINT, advisor_model VARCHAR, \
             input_transformations_count INTEGER",
        ],
        &[
            "attachment_prompt_snapshots",
            "entry_id BIGINT, model VARCHAR, system_prompt JSON, tools JSON, settings JSON",
        ],
        &["attachment_raw_types", "entry_id BIGINT, raw_type VARCHAR"],
        &[
            "attachment_rendered",
            "entry_id BIGINT, rendered_role VARCHAR, rendered JSON",
        ],
        &[
            "attachment_thinking_drops",
            "entry_id BIGINT, model VARCHAR, request_id VARCHAR, query_source VARCHAR, \
             thinking_blocks_sent BIGINT, thinking_turns_sent BIGINT, \
             newly_dropped_block_count BIGINT, newly_dropped_turn_count BIGINT, \
             newly_dropped_reason VARCHAR, payload JSON",
        ],
        &["entry_extras", "entry_id BIGINT, extra JSON"],
        &[
            "ingest_drift",
            "entry_type VARCHAR, attachment_type VARCHAR, path VARCHAR, lines BIGINT, bytes BIGINT",
        ],
        &["ingest_meta", "key VARCHAR, value VARCHAR"],
        &[
            "unparsed_lines",
            "file_path VARCHAR, line_no BIGINT, raw_type VARCHAR, session_id VARCHAR, payload JSON",
        ],
        &[
            "user_classifier_contexts",
            "entry_id BIGINT, request_id VARCHAR, live_cwd VARCHAR, platform VARCHAR, \
             git_root VARCHAR, git_branch VARCHAR, git_status_clean BOOLEAN, context JSON",
        ],
        &[
            "user_turn_fields",
            "entry_id BIGINT, turn_origin VARCHAR, turn_prompt_index BIGINT, turn_index BIGINT",
        ],
    ]);
    assert_eq!(columns, expected);

    let indexes = query(
        conn,
        "SELECT table_name, index_name, regexp_replace(expressions::VARCHAR, '[\"'']', '', 'g') \
         FROM duckdb_indexes() \
         WHERE is_unique AND table_name IN (SELECT DISTINCT table_name FROM information_schema.columns \
             WHERE table_name IN ('entry_extras', 'unparsed_lines', 'attachment_raw_types', \
             'assistant_turn_fields', 'assistant_input_transformations', 'user_turn_fields', \
             'user_classifier_contexts', 'attachment_rendered', 'attachment_thinking_drops', \
             'attachment_prompt_snapshots', 'ingest_drift', 'ingest_meta')) \
         ORDER BY table_name",
    );
    let expected = table(&[
        &[
            "assistant_input_transformations",
            "uq_assistant_input_transformations_pk",
            "[entry_id, position]",
        ],
        &[
            "assistant_turn_fields",
            "uq_assistant_turn_fields_pk",
            "[entry_id]",
        ],
        &[
            "attachment_prompt_snapshots",
            "uq_attachment_prompt_snapshots_pk",
            "[entry_id]",
        ],
        &[
            "attachment_raw_types",
            "uq_attachment_raw_types_pk",
            "[entry_id]",
        ],
        &[
            "attachment_rendered",
            "uq_attachment_rendered_pk",
            "[entry_id]",
        ],
        &[
            "attachment_thinking_drops",
            "uq_attachment_thinking_drops_pk",
            "[entry_id]",
        ],
        &["entry_extras", "uq_entry_extras_pk", "[entry_id]"],
        &[
            "ingest_drift",
            "uq_ingest_drift_pk",
            "[entry_type, attachment_type, path]",
        ],
        &["ingest_meta", "uq_ingest_meta_pk", "[key]"],
        &[
            "unparsed_lines",
            "uq_unparsed_lines_pk",
            "[file_path, line_no]",
        ],
        &[
            "user_classifier_contexts",
            "uq_user_classifier_contexts_pk",
            "[entry_id]",
        ],
        &["user_turn_fields", "uq_user_turn_fields_pk", "[entry_id]"],
    ]);
    assert_eq!(indexes, expected);
}

/// The lines of the main fixture file by 1-based line number.
fn fixture_lines() -> BTreeMap<i64, Value> {
    let text = fs::read_to_string(fixture_dir().join("catch_all.jsonl")).expect("read fixture");
    text.lines()
        .enumerate()
        .map(|(idx, line)| {
            (
                idx as i64 + 1,
                serde_json::from_str(line).expect("fixture line is json"),
            )
        })
        .collect()
}

/// The named top-level keys of a fixture line, as an object.
fn keep(line: &Value, keys: &[&str]) -> Value {
    let map: Map<String, Value> = keys
        .iter()
        .map(|key| {
            let value = line.get(*key).unwrap_or_else(|| panic!("no key {key}"));
            (key.to_string(), value.clone())
        })
        .collect();
    Value::Object(map)
}

/// A JSON column: `None` for SQL NULL, otherwise the parsed text.
fn parsed(cell: &str) -> Option<Value> {
    (cell != "NULL").then(|| serde_json::from_str(cell).unwrap_or_else(|e| panic!("{cell}: {e}")))
}

/// What `entry_extras` must hold for each fixture line: the part of the line
/// the typed round trip loses, without what a typed table gives back exactly.
/// A line that is absent here must have no row.
fn expected_extras(raw: &BTreeMap<i64, Value>) -> BTreeMap<i64, Value> {
    let line = |n: i64| &raw[&n];
    let list = |n: i64| json!({ "message": { "input_transformations": line(n)["message"]["input_transformations"] } });
    let mut want: BTreeMap<i64, Value> = BTreeMap::new();

    // 3: the scalars promoted exactly leave; the list stays, because its
    // second item carries a member beyond the three.
    let mut three = keep(
        line(3),
        &[
            "perTurnEffort",
            "apiBlockIndex",
            "serverClassifierRequest",
            "wireToolInputs",
        ],
    );
    three["message"] = list(3)["message"].clone();
    want.insert(3, three);
    // Values of a kind the column does not store stay.
    for n in [5, 6, 8] {
        want.insert(n, keep(line(n), &["thinkingDurationMs"]));
    }
    want.insert(9, keep(line(9), &["turnPosition"]));
    want.insert(
        11,
        json!({ "attachment": { "toolUseID": line(11)["attachment"]["toolUseID"] } }),
    );
    // Attachment types the parser does not know, whole and under their real name.
    for n in [14, 15, 58] {
        want.insert(n, keep(line(n), &["attachment"]));
    }
    want.insert(
        16,
        json!({ "message": { "content": {
            "[1]": line(16)["message"]["content"][1],
            "[2]": { "caller": { "zzNew": 1 } },
        } } }),
    );
    want.insert(17, json!({ "zzNull": null }));
    want.insert(18, json!({ "cwd": null, "gitBranch": null }));
    want.insert(20, json!({ "durationMs": 9007199254740993u64 }));
    want.insert(21, keep(line(21), &["zzBig", "zzFloat"]));
    // The real names behind the string `unknown`.
    want.insert(22, json!({ "message": { "role": "zz_role" } }));
    want.insert(23, json!({ "message": { "role": "zz_role" } }));
    want.insert(24, json!({ "subtype": "zz_subtype" }));
    for n in [25, 26] {
        want.insert(
            n,
            json!({ "message": { "content": { "[0]": {
                "source": line(n)["message"]["content"][0]["source"],
            } } } }),
        );
    }
    want.insert(
        27,
        json!({ "message": { "content": { "[1]": line(27)["message"]["content"][1] } } }),
    );
    want.insert(28, json!({ "mode": "zz_mode" }));
    // A key with a dot or a bracket is one ordinary key.
    want.insert(29, keep(line(29), &["zz.dotted"]));
    want.insert(
        30,
        json!({ "message": { "zzNew": "nested" }, "message.zzNew": "toplevel", "zz[0]": 1 }),
    );
    want.insert(32, keep(line(32), &["zzÜñí"]));
    want.insert(33, keep(line(33), &["zzDeep"]));
    want.insert(36, keep(line(36), &["zzNew"]));
    // Lists that their rows do not rebuild exactly stay whole.
    want.insert(40, list(40));
    want.insert(41, list(41));
    let mut forty_three = keep(line(43), &["thinkingDurationMs", "advisorModel"]);
    forty_three["message"] = list(43)["message"].clone();
    want.insert(43, forty_three);
    want.insert(45, list(45));
    want.insert(46, keep(line(46), &["turnOrigin"]));
    for n in [47, 48, 49] {
        want.insert(n, keep(line(n), &["turnPosition"]));
    }
    for n in [50, 51, 52] {
        want.insert(n, keep(line(n), &["serverClassifierContext"]));
    }
    want.insert(55, keep(line(55), &["renderedRole"]));
    want
}

/// Layer 1: `entry_extras` and `attachment_raw_types`.
fn catch_all_rows(conn: &Connection, raw: &BTreeMap<i64, Value>) {
    let extras: BTreeMap<i64, Value> = query(
        conn,
        "SELECT e.line_no, x.extra FROM entry_extras x JOIN entries e USING (entry_id) \
         ORDER BY e.line_no",
    )
    .into_iter()
    .map(|row| (row[0].parse().unwrap(), parsed(&row[1]).expect("extra")))
    .collect();
    let want = expected_extras(raw);
    assert_eq!(want.len(), 38);
    for n in raw.keys() {
        assert_eq!(extras.get(n), want.get(n), "entry_extras of line {n}");
    }
    assert_eq!(extras, want);
    // One row per entry at most, and never an empty one.
    let rows = query(
        conn,
        "SELECT COUNT(*), COUNT(DISTINCT entry_id) FROM entry_extras",
    );
    assert_eq!(rows, table(&[&["38", "38"]]));
    assert!(extras.values().all(|extra| extra != &json!({})));

    // The real type name of every attachment row written as `unknown`.
    let raw_types = query(
        conn,
        "SELECT e.line_no, r.raw_type FROM attachment_raw_types r JOIN entries e USING (entry_id) \
         ORDER BY e.line_no",
    );
    let expected = table(&[
        &["12", "prompt_snapshot"],
        &["13", "thinking_drop"],
        &["14", "zz_future_type"],
        &["15", "zz_empty_type"],
        &["31", "prompt_snapshot"],
        &["37", "thinking_drop"],
        &["39", "prompt_snapshot"],
        &["56", "thinking_drop"],
        &["57", "prompt_snapshot"],
        &["58", "zz_future_type"],
    ]);
    assert_eq!(raw_types, expected);
    // Exactly the rows written as `unknown`, no other.
    let both = query(
        conn,
        "SELECT COUNT(*) FILTER (WHERE a.attachment_type = 'unknown'), \
                COUNT(*) FILTER (WHERE r.entry_id IS NOT NULL), \
                COUNT(*) FILTER (WHERE (a.attachment_type = 'unknown') <> (r.entry_id IS NOT NULL)) \
         FROM attachment_entries a LEFT JOIN attachment_raw_types r USING (entry_id)",
    );
    assert_eq!(both, table(&[&["10", "10", "0"]]));
}

/// Layer 2: the typed tables, row by row.
fn typed_rows(conn: &Connection, raw: &BTreeMap<i64, Value>) {
    let line = |n: &str| &raw[&n.parse::<i64>().unwrap()];

    // A value of a kind the column does not store leaves the column NULL: no
    // row for lines 5, 6, 8 and 43, whose only values are of other kinds.
    let turn_fields = query(
        conn,
        "SELECT e.line_no, t.thinking_duration_ms, t.advisor_model, t.input_transformations_count \
         FROM assistant_turn_fields t JOIN entries e USING (entry_id) ORDER BY e.line_no",
    );
    let expected = table(&[
        &["3", "1234", "fx-advisor", "2"],
        &["4", "NULL", "fx-advisor", "0"],
        &["7", "-5", "NULL", "NULL"],
        &["40", "NULL", "NULL", "2"],
        &["41", "NULL", "NULL", "2"],
        &["42", "NULL", "NULL", "2"],
        &["44", "9223372036854775807", "fx-rådgivare ✓ 顧問", "NULL"],
        &["45", "-9223372036854775808", "NULL", "NULL"],
    ]);
    assert_eq!(turn_fields, expected);

    // One row per item that is an object, at its position in the raw list.
    let items = query(
        conn,
        "SELECT e.line_no, t.position, t.transformation_type, t.path, t.reason \
         FROM assistant_input_transformations t JOIN entries e USING (entry_id) \
         ORDER BY e.line_no, t.position",
    );
    let expected = table(&[
        &["3", "0", "fx_rewrite", "messages[0]", "fx reason one"],
        &["3", "1", "fx_rewrite", "messages[1]", "fx reason two"],
        &["40", "0", "fx_rewrite", "messages[0]", "fx reason"],
        &["41", "0", "fx_rewrite", "NULL", "fx reason"],
        &["41", "1", "NULL", "messages[1]", "fx reason"],
        &["42", "0", "fx_only_type", "NULL", "NULL"],
        &["42", "1", "NULL", "NULL", "NULL"],
    ]);
    assert_eq!(items, expected);

    let user_fields = query(
        conn,
        "SELECT e.line_no, t.turn_origin, t.turn_prompt_index, t.turn_index \
         FROM user_turn_fields t JOIN entries e USING (entry_id) ORDER BY e.line_no",
    );
    let expected = table(&[
        &["2", "typed", "3", "7"],
        &["46", "NULL", "4", "NULL"],
        &["47", "NULL", "1", "2"],
        &["48", "fx-ürsprung ✓", "NULL", "NULL"],
    ]);
    assert_eq!(user_fields, expected);

    let contexts = query(
        conn,
        "SELECT e.line_no, t.request_id, t.live_cwd, t.platform, t.git_root, t.git_branch, \
                t.git_status_clean, t.context \
         FROM user_classifier_contexts t JOIN entries e USING (entry_id) ORDER BY e.line_no",
    );
    let scalars: Vec<Vec<String>> = contexts.iter().map(|row| row[..7].to_vec()).collect();
    let expected = table(&[
        &[
            "2",
            "fx-classifier-request-02",
            "/fx/work",
            "linux",
            "/fx",
            "main",
            "true",
        ],
        // The git fields are null in the raw line: the columns stay NULL and
        // `context` keeps the object.
        &[
            "38",
            "fx-classifier-request-38",
            "/fx/work",
            "linux",
            "NULL",
            "NULL",
            "NULL",
        ],
        &["50", "NULL", "/fx/w", "NULL", "/fx", "NULL", "NULL"],
        &[
            "51",
            "fx-classifier-request-51",
            "/fx",
            "NULL",
            "NULL",
            "NULL",
            "NULL",
        ],
        &[
            "53",
            "fx-classifier-request-53",
            "NULL",
            "NULL",
            "NULL",
            "NULL",
            "NULL",
        ],
        &["54", "NULL", "/fx/ü ✓", "linux", "NULL", "NULL", "NULL"],
    ]);
    assert_eq!(scalars, expected);
    for row in &contexts {
        let want = &line(&row[0])["serverClassifierContext"]["context"];
        assert_eq!(
            parsed(&row[7]).as_ref(),
            Some(want),
            "context of line {}",
            row[0]
        );
    }

    // `rendered` is JSON text whatever its kind: a list, a string in its
    // quoted form, a raw null as the text `null`; SQL NULL when the key is absent.
    let rendered = query(
        conn,
        "SELECT e.line_no, t.rendered_role, t.rendered \
         FROM attachment_rendered t JOIN entries e USING (entry_id) ORDER BY e.line_no",
    );
    let seen: Vec<(String, String, Option<Value>)> = rendered
        .iter()
        .map(|row| (row[0].clone(), row[1].clone(), parsed(&row[2])))
        .collect();
    let want = |n: &str, role: &str, value: Option<Value>| (n.to_string(), role.to_string(), value);
    let expected = vec![
        want("10", "user", Some(json!("fx rendered as text"))),
        want("11", "user", Some(line("11")["rendered"].clone())),
        want("55", "NULL", Some(Value::Null)),
        want("58", "system", Some(line("58")["rendered"].clone())),
        want("59", "user", None),
    ];
    assert_eq!(seen, expected);
    assert_eq!(rendered[0][2], "\"fx rendered as text\"");
    assert_eq!(rendered[2][2], "null");

    let drops = query(
        conn,
        "SELECT e.line_no, t.model, t.request_id, t.query_source, t.thinking_blocks_sent, \
                t.thinking_turns_sent, t.newly_dropped_block_count, t.newly_dropped_turn_count, \
                t.newly_dropped_reason, t.payload \
         FROM attachment_thinking_drops t JOIN entries e USING (entry_id) ORDER BY e.line_no",
    );
    let scalars: Vec<Vec<String>> = drops.iter().map(|row| row[..9].to_vec()).collect();
    let expected = table(&[
        // Flat member names, which real input does not use: the three
        // `newly_dropped_*` columns stay NULL and the payload is still whole.
        &[
            "13",
            "fx-model",
            "fx-req-13",
            "fx_source",
            "4",
            "2",
            "NULL",
            "NULL",
            "NULL",
        ],
        &[
            "37",
            "fx-model",
            "fx-req-37",
            "fx_source",
            "6",
            "3",
            "2",
            "1",
            "fx_reason",
        ],
        &[
            "56", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL",
        ],
    ]);
    assert_eq!(scalars, expected);
    for row in &drops {
        let want = &line(&row[0])["attachment"];
        assert_eq!(
            parsed(&row[9]).as_ref(),
            Some(want),
            "payload of line {}",
            row[0]
        );
    }

    let snapshots = query(
        conn,
        "SELECT e.line_no, t.model, t.system_prompt, t.tools, t.settings \
         FROM attachment_prompt_snapshots t JOIN entries e USING (entry_id) ORDER BY e.line_no",
    );
    type Snapshot = (String, String, Option<Value>, Option<Value>, Option<Value>);
    let seen: Vec<Snapshot> = snapshots
        .iter()
        .map(|row| {
            (
                row[0].clone(),
                row[1].clone(),
                parsed(&row[2]),
                parsed(&row[3]),
                parsed(&row[4]),
            )
        })
        .collect();
    let expected = vec![
        (
            "12".to_string(),
            "fx-model".to_string(),
            Some(json!(["fx system part one", "fx system part two"])),
            Some(json!([{ "name": "fx_tool", "description": "fx tool" }])),
            Some(
                json!({ "type": "prompt_snapshot", "contextRendering": "fx", "inlineTools": true }),
            ),
        ),
        // A string `systemPrompt` is stored in its quoted JSON form.
        (
            "31".to_string(),
            "fx-model".to_string(),
            Some(json!("fx system prompt as one string")),
            None,
            Some(json!({ "type": "prompt_snapshot" })),
        ),
        (
            "39".to_string(),
            "NULL".to_string(),
            Some(json!(["fx system only"])),
            None,
            Some(json!({ "type": "prompt_snapshot", "cliPrefix": "fx", "keptReminders": [] })),
        ),
        // A `model` that is not a string stays in `settings`.
        (
            "57".to_string(),
            "NULL".to_string(),
            Some(Value::Null),
            Some(json!("fx tools as text")),
            Some(json!({ "type": "prompt_snapshot", "model": 7, "echoWireToolInputs": false })),
        ),
    ];
    assert_eq!(seen, expected);
    assert_eq!(snapshots[1][2], "\"fx system prompt as one string\"");
}

/// A fork table's rows by fixture line: the columns after `entry_id` as text.
fn by_line(conn: &Connection, table_name: &str, columns: &str) -> BTreeMap<i64, Vec<String>> {
    let sql = format!(
        "SELECT e.line_no, {columns} FROM {table_name} t JOIN entries e USING (entry_id) \
         ORDER BY e.line_no"
    );
    query(conn, &sql)
        .into_iter()
        .map(|row| (row[0].parse().unwrap(), row[1..].to_vec()))
        .collect()
}

fn text_cell(cell: &str) -> Option<Value> {
    (cell != "NULL").then(|| Value::String(cell.to_string()))
}

fn int_cell(cell: &str) -> Option<Value> {
    (cell != "NULL").then(|| Value::from(cell.parse::<i64>().unwrap()))
}

fn object(members: Vec<(&str, Option<Value>)>) -> Option<Value> {
    let map: Map<String, Value> = members
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key.to_string(), v)))
        .collect();
    (!map.is_empty()).then_some(Value::Object(map))
}

/// The whole fixture, read back from the database alone: for every line of
/// known type, the typed round trip merged with the line's `entry_extras` row
/// and with the values rebuilt from the typed tables reproduces every raw
/// leaf. A rebuilt value is used only where `entry_extras` does not hold the
/// place, so a promoted value that left the catch-all without being exact
/// fails here.
fn nothing_is_lost(conn: &Connection, raw: &BTreeMap<i64, Value>) {
    let extras = by_line(conn, "entry_extras", "t.extra");
    let turn = by_line(
        conn,
        "assistant_turn_fields",
        "t.thinking_duration_ms, t.advisor_model, t.input_transformations_count",
    );
    let items = query(
        conn,
        "SELECT e.line_no, t.position, t.transformation_type, t.path, t.reason \
         FROM assistant_input_transformations t JOIN entries e USING (entry_id) \
         ORDER BY e.line_no, t.position",
    );
    let user = by_line(
        conn,
        "user_turn_fields",
        "t.turn_origin, t.turn_prompt_index, t.turn_index",
    );
    let contexts = by_line(conn, "user_classifier_contexts", "t.request_id, t.context");
    let rendered = by_line(conn, "attachment_rendered", "t.rendered_role, t.rendered");
    let drops = by_line(conn, "attachment_thinking_drops", "t.payload");
    let snapshots = by_line(
        conn,
        "attachment_prompt_snapshots",
        "t.model, t.system_prompt, t.tools, t.settings",
    );

    let mut checked = 0;
    for (n, line) in raw {
        let entry: Entry = serde_json::from_value(line.clone()).expect("typed parse");
        if matches!(entry, Entry::Unknown) {
            continue;
        }
        checked += 1;
        let extra = extras.get(n).and_then(|row| parsed(&row[0]));
        let holds = |key: &str| extra.as_ref().is_some_and(|x| x.get(key).is_some());
        let round_trip = serde_json::to_value(&entry).expect("re-serialise");
        let mut merged = merge(round_trip, extra.as_ref());

        let mut rebuilt: Vec<(&str, Option<Value>)> = Vec::new();
        if let Some(row) = turn.get(n) {
            rebuilt.push(("thinkingDurationMs", int_cell(&row[0])));
            rebuilt.push(("advisorModel", text_cell(&row[1])));
            let in_extra = extra
                .as_ref()
                .is_some_and(|x| x.pointer("/message/input_transformations").is_some());
            if let (Some(count), false) = (int_cell(&row[2]), in_extra) {
                let list: Vec<Value> = items
                    .iter()
                    .filter(|item| item[0] == n.to_string())
                    .map(|item| {
                        object(vec![
                            ("type", text_cell(&item[2])),
                            ("path", text_cell(&item[3])),
                            ("reason", text_cell(&item[4])),
                        ])
                        .unwrap_or_else(|| json!({}))
                    })
                    .collect();
                assert_eq!(Some(list.len() as i64), count.as_i64(), "line {n}: items");
                merged["message"]["input_transformations"] = Value::Array(list);
            }
        }
        if let Some(row) = user.get(n) {
            rebuilt.push(("turnOrigin", text_cell(&row[0])));
            rebuilt.push((
                "turnPosition",
                object(vec![
                    ("promptIndex", int_cell(&row[1])),
                    ("turnIndex", int_cell(&row[2])),
                ]),
            ));
        }
        if let Some(row) = contexts.get(n) {
            rebuilt.push((
                "serverClassifierContext",
                object(vec![
                    ("request", text_cell(&row[0])),
                    ("context", parsed(&row[1])),
                ]),
            ));
        }
        if let Some(row) = rendered.get(n) {
            rebuilt.push(("renderedRole", text_cell(&row[0])));
            rebuilt.push(("rendered", parsed(&row[1])));
        }
        if let Some(row) = drops.get(n) {
            rebuilt.push(("attachment", parsed(&row[0])));
        }
        if let Some(row) = snapshots.get(n) {
            let mut attachment = parsed(&row[3]).expect("settings");
            for (key, value) in [
                ("model", text_cell(&row[0])),
                ("systemPrompt", parsed(&row[1])),
                ("tools", parsed(&row[2])),
            ] {
                if let Some(value) = value {
                    attachment[key] = value;
                }
            }
            rebuilt.push(("attachment", Some(attachment)));
        }
        for (key, value) in rebuilt {
            if let (Some(value), false) = (value, holds(key)) {
                merged[key] = value;
            }
        }
        assert_eq!(
            unreproduced(line, &merged),
            Vec::<String>::new(),
            "line {n} is not reproduced from the database"
        );
    }
    assert_eq!(checked, 57);
}

/// The trimmed text of the lines of a fixture file whose `type` the parser
/// does not know, by line number.
fn unknown_lines(file: &str) -> Vec<(i64, String)> {
    let text = fs::read_to_string(fixture_dir().join(file)).expect("read fixture");
    text.lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let value: Value = serde_json::from_str(line).expect("fixture line is json");
            let ty = value.get("type").and_then(|t| t.as_str()).expect("type");
            UNKNOWN_ENTRY_TYPES
                .contains(&ty)
                .then(|| (idx as i64 + 1, line.trim().to_string()))
        })
        .collect()
}

/// `unparsed_lines`: one row per line of an unknown type, in both files. The
/// file that holds nothing else has rows here and no `transcripts` row.
fn unparsed_rows(conn: &Connection, main_file_path: &str) {
    let rows = query(
        conn,
        "SELECT regexp_extract(file_path, '[^/]+$'), line_no, raw_type, session_id, payload \
         FROM unparsed_lines ORDER BY 1, 2",
    );
    let main = unknown_lines("catch_all.jsonl");
    let only = unknown_lines("unknown_only.jsonl");
    assert_eq!(main.len(), 2);
    assert_eq!(only.len(), 5);
    let text_of = |lines: &[(i64, String)], n: i64| -> String {
        lines
            .iter()
            .find(|(line_no, _)| *line_no == n)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("no unknown line {n}"))
    };
    let expected: Vec<Vec<String>> = vec![
        vec![
            "catch_all.jsonl".into(),
            "34".into(),
            "cost-state".into(),
            "fx-session".into(),
            text_of(&main, 34),
        ],
        vec![
            "catch_all.jsonl".into(),
            "35".into(),
            "zz-future-entry".into(),
            "fx-session".into(),
            text_of(&main, 35),
        ],
        vec![
            "unknown_only.jsonl".into(),
            "1".into(),
            "cost-state".into(),
            "fx-session-2".into(),
            text_of(&only, 1),
        ],
        vec![
            "unknown_only.jsonl".into(),
            "2".into(),
            "zz-future-entry".into(),
            "fx-session-2".into(),
            text_of(&only, 2),
        ],
        vec![
            "unknown_only.jsonl".into(),
            "3".into(),
            "continued-in".into(),
            "fx-session-2".into(),
            text_of(&only, 3),
        ],
        // No `sessionId`; the payload keeps the repeated key and the
        // spelling of the numbers, without the spaces around the line.
        vec![
            "unknown_only.jsonl".into(),
            "4".into(),
            "zz-future-entry".into(),
            "NULL".into(),
            "{\"type\":\"zz-future-entry\",\"n\":1.50,\"n\":1e2,\
             \"big\":123456789012345678901234567890,\"zzÜ\":\"✓\"}"
                .into(),
        ],
        // A `sessionId` that is not a string.
        vec![
            "unknown_only.jsonl".into(),
            "5".into(),
            "cost-state".into(),
            "NULL".into(),
            text_of(&only, 5),
        ],
    ];
    assert_eq!(rows, expected);

    // The rows of the main file carry the path its `transcripts` row carries.
    let paths = query(
        conn,
        "SELECT DISTINCT file_path FROM unparsed_lines WHERE file_path LIKE '%catch_all.jsonl'",
    );
    assert_eq!(paths, vec![vec![main_file_path.to_string()]]);
    let files = query(
        conn,
        "SELECT COUNT(DISTINCT u.file_path), COUNT(DISTINCT t.file_path) \
         FROM unparsed_lines u LEFT JOIN transcripts t USING (file_path)",
    );
    assert_eq!(files, table(&[&["2", "1"]]));
}

/// `ingest_drift` (what sits in the catch-all, by entry type and place) and
/// `ingest_meta`.
fn drift_and_meta(conn: &Connection, raw: &BTreeMap<i64, Value>) {
    let drift = query(
        conn,
        "SELECT entry_type, attachment_type, path, lines, bytes FROM ingest_drift \
         ORDER BY entry_type, attachment_type, path",
    );
    let places: Vec<Vec<String>> = drift.iter().map(|row| row[..4].to_vec()).collect();
    let expected = table(&[
        &["assistant", "", "advisorModel", "1"],
        &["assistant", "", "apiBlockIndex", "1"],
        &["assistant", "", "message.content[]", "1"],
        &["assistant", "", "message.content[].caller.zzNew", "1"],
        &["assistant", "", "message.input_transformations", "5"],
        &["assistant", "", "message.role", "1"],
        &["assistant", "", "perTurnEffort", "1"],
        &["assistant", "", "serverClassifierRequest", "1"],
        &["assistant", "", "thinkingDurationMs", "4"],
        &["assistant", "", "wireToolInputs", "1"],
        &["attachment", "date_change", "attachment.toolUseID", "1"],
        &["attachment", "date_change", "renderedRole", "1"],
        // An attachment type the parser does not know: one row under its
        // real name, with the path `attachment`.
        &["attachment", "zz_empty_type", "attachment", "1"],
        &["attachment", "zz_future_type", "attachment", "2"],
        // One row per unknown entry type, both files together.
        &["continued-in", "", "(line)", "1"],
        &["cost-state", "", "(line)", "3"],
        &["mode", "", "mode", "1"],
        &["mode", "", "zzNew", "1"],
        &["system", "", "durationMs", "1"],
        &["system", "", "subtype", "1"],
        &["user", "", "cwd", "1"],
        &["user", "", "gitBranch", "1"],
        &["user", "", "message.content[]", "1"],
        &["user", "", "message.content[].source", "2"],
        &["user", "", "message.role", "1"],
        // Two values on one line that print as the same place: one line.
        &["user", "", "message.zzNew", "1"],
        &["user", "", "serverClassifierContext", "3"],
        &["user", "", "turnOrigin", "1"],
        &["user", "", "turnPosition", "4"],
        &["user", "", "zz.dotted", "1"],
        &["user", "", "zzBig", "1"],
        &["user", "", "zzDeep", "1"],
        &["user", "", "zzFloat", "1"],
        &["user", "", "zzNull", "1"],
        &["user", "", "zz[0]", "1"],
        &["user", "", "zzÜñí", "1"],
        &["zz-future-entry", "", "(line)", "3"],
    ]);
    assert_eq!(places, expected);

    // Bytes are the length of each stored value as compact JSON, or of the
    // line text for an unknown entry type.
    let bytes_of = |entry_type: &str, attachment_type: &str, path: &str| -> usize {
        drift
            .iter()
            .find(|row| row[0] == entry_type && row[1] == attachment_type && row[2] == path)
            .map(|row| row[4].parse().unwrap())
            .unwrap_or_else(|| panic!("no drift row {entry_type} {path}"))
    };
    let len = |value: &Value| serde_json::to_string(value).unwrap().len();
    let lists: usize = [3, 40, 41, 43, 45]
        .iter()
        .map(|n| len(&raw[n]["message"]["input_transformations"]))
        .sum();
    assert_eq!(
        bytes_of("assistant", "", "message.input_transformations"),
        lists
    );
    assert_eq!(
        bytes_of("attachment", "zz_future_type", "attachment"),
        len(&raw[&14]["attachment"]) + len(&raw[&58]["attachment"])
    );
    assert_eq!(
        bytes_of("user", "", "message.zzNew"),
        "\"nested\"".len() + "\"toplevel\"".len()
    );
    assert_eq!(bytes_of("user", "", "zzNull"), "null".len());
    assert_eq!(bytes_of("user", "", "zzÜñí"), len(&raw[&32]["zzÜñí"]));
    let line_bytes = |ty: &str| -> usize {
        ["catch_all.jsonl", "unknown_only.jsonl"]
            .iter()
            .flat_map(|file| unknown_lines(file))
            .filter(|(_, text)| text.starts_with(&format!("{{\"type\":\"{ty}\"")))
            .map(|(_, text)| text.len())
            .sum()
    };
    for ty in UNKNOWN_ENTRY_TYPES {
        assert_eq!(bytes_of(ty, "", "(line)"), line_bytes(ty), "{ty}");
    }

    let meta = query(conn, "SELECT key, value FROM ingest_meta ORDER BY key");
    let version = concat!(env!("CARGO_PKG_VERSION"), "+corpusdb.");
    let cct_version = meta
        .iter()
        .find(|row| row[0] == "cct_version")
        .map(|row| row[1].clone())
        .expect("cct_version");
    let build_number = cct_version.strip_prefix(version).expect("fork version");
    assert!(!build_number.is_empty() && build_number.bytes().all(|b| b.is_ascii_digit()));
    let expected = vec![
        vec!["cct_version".to_string(), cct_version.clone()],
        vec!["entries".to_string(), "57".to_string()],
        vec!["extras_rows".to_string(), "38".to_string()],
        vec!["lines_read".to_string(), "64".to_string()],
        vec!["reserialize_errors".to_string(), "0".to_string()],
        vec!["unparsed_lines".to_string(), "7".to_string()],
        vec![
            "upstream_tag".to_string(),
            concat!("v", env!("CARGO_PKG_VERSION")).to_string(),
        ],
    ];
    assert_eq!(meta, expected);
    // The counters agree with the tables they describe.
    let counts = query(
        conn,
        "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM entry_extras), \
                (SELECT COUNT(*) FROM unparsed_lines)",
    );
    assert_eq!(counts, table(&[&["57", "38", "7"]]));
}
