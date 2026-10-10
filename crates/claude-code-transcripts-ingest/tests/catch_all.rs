//! End-to-end test: ingest the synthetic `catch_all` fixture directory and
//! assert what lands in the existing relations. The fixture holds one line
//! per way a transcript line can carry something the typed parser does not
//! model (see `tests/fixtures/catch_all/`): `catch_all.jsonl` mixes known and
//! unknown entry types, `unknown_only.jsonl` holds unknown entry types only.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use claude_code_transcripts_ingest::cli::IngestArgs;
use claude_code_transcripts_ingest::run::run_ingest;
use duckdb::Connection;
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
    assert_eq!(known.len(), 34, "fixture: lines of known type");

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
        ("assistant", 8),
        ("attachment", 7),
        ("mode", 2),
        ("system", 3),
        ("user", 14),
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
    assert_eq!(transcripts[0].1, 34);

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
        ("AttachmentData", 5),
        ("DocumentSource", 1),
        ("ImageSource", 1),
        ("UserContentBlock", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(unknown_variants, expected_variants);
}
