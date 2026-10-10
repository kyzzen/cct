//! End-to-end test over generated synthetic input: more distinct catch-all
//! places than `ingest_drift` keeps, and values large enough to matter for
//! size. One `run_ingest` call, because it builds the global thread pool.

use std::fmt::Write as _;
use std::fs;

use claude_code_transcripts_ingest::cli::IngestArgs;
use claude_code_transcripts_ingest::run::run_ingest;
use duckdb::Connection;
use tempfile::TempDir;

const ENVELOPE: &str = r#""parentUuid":null,"isSidechain":false,"sessionId":"gx-session","timestamp":"2026-01-01T00:00:00.000Z""#;

/// Distinct unmodelled keys on `user` lines; with the other places of the
/// input this is nine more than the 2,000 rows `ingest_drift` keeps.
const USER_KEYS: usize = 2003;
const LIST_ITEMS: usize = 20_000;
const BIG: usize = 2_000_000;

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

#[test]
fn drift_cap_and_large_values() {
    let tmp = TempDir::new().expect("tempdir");
    let in_dir = tmp.path().join("in");
    let db_path = tmp.path().join("out.duckdb");
    fs::create_dir(&in_dir).unwrap();

    let big = "g".repeat(BIG);
    let mut text = String::new();
    // Line 1: an entry type the parser does not know, about 2 MB.
    writeln!(text, r#"{{"type":"zz-big-entry","blob":"{big}"}}"#).unwrap();
    // Line 2: a second unknown entry type, with an escaped NUL character in
    // a string, a raw NUL character between two members, spaces around the
    // line and a CRLF line end.
    text.push_str("  ");
    text.push_str(r#"{"type":"cost-state","totalCost":1.5,"#);
    text.push('\0');
    text.push_str(r#""note":"gx\u0000nul"}"#);
    text.push_str("  \r\n");
    // Line 3: a key spelled like the row that collects the places past the cap.
    writeln!(
        text,
        r#"{{"type":"mode","mode":"normal","sessionId":"gx-session","<other>":"gx\u0000collision padding"}}"#
    )
    .unwrap();
    // Lines 4 to 6: three places on `mode` lines, 1 byte each.
    for i in 0..3 {
        writeln!(
            text,
            r#"{{"type":"mode","mode":"normal","sessionId":"gx-session","zzM{i}":1}}"#
        )
        .unwrap();
    }
    // 2,003 places on `user` lines, 2 bytes each.
    for i in 0..USER_KEYS {
        writeln!(
            text,
            r#"{{"type":"user","uuid":"gx-u{i:04}",{ENVELOPE},"message":{{"role":"user","content":"gx"}},"zzK{i:04}":10}}"#
        )
        .unwrap();
    }
    // A `rendered` list with a 2 MB item.
    writeln!(
        text,
        r#"{{"type":"attachment","uuid":"gx-a1",{ENVELOPE},"attachment":{{"type":"date_change","newDate":"2026-01-02"}},"rendered":[{{"content":"{big}"}}],"renderedRole":"user"}}"#
    )
    .unwrap();
    // A `prompt_snapshot` with a 2 MB system prompt.
    writeln!(
        text,
        r#"{{"type":"attachment","uuid":"gx-a2",{ENVELOPE},"attachment":{{"type":"prompt_snapshot","model":"gx-model","systemPrompt":["{big}"],"tools":[]}}}}"#
    )
    .unwrap();
    // A list of 20,000 transformations.
    let items: Vec<String> = (0..LIST_ITEMS)
        .map(|i| format!(r#"{{"type":"gx_rewrite","path":"messages[{i}]","reason":"gx"}}"#))
        .collect();
    writeln!(
        text,
        r#"{{"type":"assistant","uuid":"gx-s1",{ENVELOPE},"requestId":"gx-req","message":{{"id":"gx-msg","type":"message","role":"assistant","model":"gx-model","content":[{{"type":"text","text":"gx"}}],"stop_reason":"end_turn","stop_sequence":null,"usage":{{"input_tokens":1,"output_tokens":2}},"input_transformations":[{}]}}}}"#,
        items.join(",")
    )
    .unwrap();
    // Last line: promoted keys on an entry type that does not carry them in
    // real input. Promotion goes by key, so each gets its typed row.
    writeln!(
        text,
        r#"{{"type":"user","uuid":"gx-foreign",{ENVELOPE},"message":{{"role":"user","content":"gx"}},"thinkingDurationMs":5,"renderedRole":"user"}}"#
    )
    .unwrap();
    fs::write(in_dir.join("generated.jsonl"), &text).unwrap();
    let lines = text.lines().count() as i64;

    let args = IngestArgs {
        input_dir: in_dir,
        jobs: 2,
        output: db_path.clone(),
        pricing: None,
        no_progress: true,
    };
    run_ingest(args).expect("ingest succeeds");
    let conn = Connection::open(&db_path).expect("open db");

    // Every line is an entry or an unparsed line.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM entries"), lines - 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM unparsed_lines"), 2);
    assert_eq!(
        count(
            &conn,
            "SELECT value::BIGINT FROM ingest_meta WHERE key = 'lines_read'"
        ),
        lines
    );

    // The cap. 2,009 distinct places: two `(line)` rows, the `<other>` key,
    // three 1-byte `mode` places and 2,003 2-byte `user` places. The 2,000
    // largest are kept (ties by entry type, attachment type and path), so the
    // three `mode` places and the last six `user` places fold into one
    // `<other>` row per entry type; the `mode` one meets the key spelled
    // `<other>` and the two share a row.
    let stored: i64 = 2 + 1 + 3 + USER_KEYS as i64;
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM ingest_drift"), 2001);
    assert_eq!(
        count(&conn, "SELECT SUM(lines)::BIGINT FROM ingest_drift"),
        stored
    );
    let other: Vec<(String, i64, i64)> = conn
        .prepare(
            "SELECT entry_type, lines, bytes FROM ingest_drift \
             WHERE path = '<other>' AND attachment_type = '' ORDER BY entry_type",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let padding = r#""gx\u0000collision padding""#.len() as i64;
    assert_eq!(
        other,
        vec![
            ("mode".to_string(), 4, padding + 3),
            ("user".to_string(), 6, 12),
        ]
    );
    let kept = |path: &str| {
        count(
            &conn,
            &format!("SELECT COUNT(*) FROM ingest_drift WHERE path = '{path}'"),
        )
    };
    assert_eq!(kept("zzK0000"), 1);
    assert_eq!(kept("zzK1996"), 1);
    assert_eq!(kept("zzK1997"), 0);
    assert_eq!(kept("zzK2002"), 0);
    assert_eq!(kept("zzM0"), 0);
    assert_eq!(
        count(
            &conn,
            "SELECT bytes FROM ingest_drift WHERE entry_type = 'zz-big-entry' AND path = '(line)'"
        ),
        text.lines().next().unwrap().len() as i64
    );
    // The catch-all itself is not capped: every place is still in its row.
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM entry_extras"),
        1 + 3 + USER_KEYS as i64
    );

    // An escaped NUL character is stored as written, in both places. The
    // payload of an unparsed line is the text the parser read: upstream
    // removes raw NUL characters and the surrounding white space first.
    let stored_text = |sql: &str| -> String {
        conn.query_row(sql, [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    assert_eq!(
        stored_text("SELECT payload FROM unparsed_lines WHERE line_no = 2"),
        r#"{"type":"cost-state","totalCost":1.5,"note":"gx\u0000nul"}"#
    );
    assert!(text.lines().nth(1).unwrap().contains('\0'));
    assert_eq!(
        stored_text(
            "SELECT x.extra::VARCHAR FROM entry_extras x JOIN entries e USING (entry_id) \
             WHERE e.line_no = 3"
        ),
        r#"{"<other>":"gx\u0000collision padding"}"#
    );

    // Large values: whole in their typed table, and not in the catch-all.
    assert_eq!(
        count(
            &conn,
            "SELECT length(payload::VARCHAR) FROM unparsed_lines WHERE raw_type = 'zz-big-entry'"
        ),
        text.lines().next().unwrap().len() as i64
    );
    assert_eq!(
        count(
            &conn,
            "SELECT length(rendered::VARCHAR) FROM attachment_rendered WHERE rendered IS NOT NULL"
        ),
        (r#"[{"content":""}]"#.len() + BIG) as i64
    );
    assert_eq!(
        count(
            &conn,
            "SELECT length(system_prompt::VARCHAR) FROM attachment_prompt_snapshots WHERE model = 'gx-model'"
        ),
        (r#"[""]"#.len() + BIG) as i64
    );
    assert_eq!(
        count(
            &conn,
            "SELECT input_transformations_count FROM assistant_turn_fields \
             WHERE input_transformations_count IS NOT NULL"
        ),
        LIST_ITEMS as i64
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM assistant_input_transformations \
             WHERE transformation_type = 'gx_rewrite' AND reason = 'gx' \
               AND path = 'messages[' || position || ']'"
        ),
        LIST_ITEMS as i64
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM entry_extras x JOIN entries e USING (entry_id) \
             WHERE e.type IN ('attachment', 'assistant')"
        ),
        0
    );

    // The line of another entry type that carries promoted keys.
    let foreign: (String, i64, String, i64) = conn
        .query_row(
            "SELECT e.type, t.thinking_duration_ms, r.rendered_role, \
                    (SELECT COUNT(*) FROM entry_extras x WHERE x.entry_id = e.entry_id) \
             FROM entries e JOIN assistant_turn_fields t USING (entry_id) \
                  JOIN attachment_rendered r USING (entry_id) \
             WHERE e.line_no = ? AND t.advisor_model IS NULL AND r.rendered IS NULL",
            [lines],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("typed rows of the last line");
    assert_eq!(foreign, ("user".to_string(), 5, "user".to_string(), 0));
}
