//! The merge check behind `CCT_VERIFY_RESIDUAL=1`, driven through the built
//! `cct` binary: a violation ends the process, which would end the test
//! harness if the ingest ran inside it.
//!
//! `CCT_VERIFY_DROP_PATH` is a switch that only the check reads. It removes
//! one place from a line's catch-all value before the check, which is the
//! proof that the check can fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use duckdb::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("catch_all")
}

/// Runs `cct ingest` over the fixture with exactly the given variables of
/// the check set.
fn ingest(db: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cct"));
    command
        .arg("ingest")
        .arg("-i")
        .arg(fixture_dir())
        .arg("-o")
        .arg(db)
        .args(["-j", "2", "--no-progress"])
        .env_remove("CCT_VERIFY_RESIDUAL")
        .env_remove("CCT_VERIFY_DROP_PATH");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run cct")
}

fn stderr_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The fixture line that carries `key` at the top level, and its number.
fn line_with(key: &str) -> usize {
    let text = fs::read_to_string(fixture_dir().join("catch_all.jsonl")).expect("read fixture");
    let found: Vec<usize> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let value: Value = serde_json::from_str(line).expect("fixture line is json");
            value.get(key).is_some()
        })
        .map(|(idx, _)| idx + 1)
        .collect();
    assert_eq!(found.len(), 1, "one fixture line carries {key}");
    found[0]
}

fn violations(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|line| line.starts_with("error: residual invariant violated in "))
        .collect()
}

fn meta(conn: &Connection, key: &str) -> String {
    conn.query_row("SELECT value FROM ingest_meta WHERE key = ?", [key], |r| {
        r.get(0)
    })
    .unwrap_or_else(|e| panic!("ingest_meta {key}: {e}"))
}

#[test]
fn check_passes_on_every_fixture_line() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("verify.duckdb");
    let output = ingest(&db, &[("CCT_VERIFY_RESIDUAL", "1")]);
    let lines = stderr_lines(&output);
    assert_eq!(output.status.code(), Some(0), "{lines:#?}");
    assert!(lines.iter().all(|line| !line.starts_with("error:")));

    // The stderr block of the ingest report.
    let at = lines
        .iter()
        .position(|line| line.starts_with("Catch-all:"))
        .expect("catch-all line");
    assert_eq!(
        lines[at],
        "Catch-all:    rows=49  unparsed_lines=7  reserialize_errors=0"
    );
    assert_eq!(lines[at + 1], "Catch-all places (top 20 of 48 by bytes):");
    let listed: Vec<&String> = lines[at + 2..]
        .iter()
        .take_while(|line| line.starts_with("  - "))
        .collect();
    assert_eq!(listed.len(), 20);
    // Largest first, and the bytes are those of `ingest_drift`.
    let conn = Connection::open(&db).expect("open db");
    let (entry_type, attachment_type, path, rows, bytes): (String, String, String, i64, i64) = conn
        .query_row(
            "SELECT entry_type, attachment_type, path, lines, bytes FROM ingest_drift \
             ORDER BY bytes DESC, entry_type, attachment_type, path LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("largest place");
    let separator = if attachment_type.is_empty() { "" } else { "/" };
    assert_eq!(
        *listed[0],
        format!(
            "  - {entry_type}{separator}{attachment_type}  {path}  (lines={rows}, bytes={bytes})"
        )
    );
    assert!(lines[at + 22].starts_with("Elapsed:"));

    // The database says which build wrote it: the string `--version` prints.
    let version = Command::new(env!("CARGO_BIN_EXE_cct"))
        .arg("--version")
        .output()
        .expect("run cct --version");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("cct {}\n", meta(&conn, "cct_version"))
    );
    assert_eq!(meta(&conn, "lines_read"), "78");
}

#[test]
fn dropped_place_exits_3() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("control.duckdb");
    let output = ingest(
        &db,
        &[
            ("CCT_VERIFY_RESIDUAL", "1"),
            ("CCT_VERIFY_DROP_PATH", "wireToolInputs"),
        ],
    );
    let lines = stderr_lines(&output);
    assert_eq!(output.status.code(), Some(3), "{lines:#?}");
    let expected = format!(
        "error: residual invariant violated in {}:{}",
        fixture_dir().join("catch_all.jsonl").display(),
        line_with("wireToolInputs")
    );
    assert_eq!(violations(&lines), vec![&expected]);
    // The refresh deletes the transcript a parse-failure message names.
    assert!(lines
        .iter()
        .all(|line| !line.starts_with("error: parse failure in")));
}

#[test]
fn dropped_nested_place_exits_3() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("control.duckdb");
    let output = ingest(
        &db,
        &[
            ("CCT_VERIFY_RESIDUAL", "1"),
            ("CCT_VERIFY_DROP_PATH", "attachment.toolUseID"),
        ],
    );
    let lines = stderr_lines(&output);
    assert_eq!(output.status.code(), Some(3), "{lines:#?}");
    assert_eq!(violations(&lines).len(), 1);
    assert!(lines
        .iter()
        .all(|line| !line.starts_with("error: parse failure in")));
}

/// A place that stays in the catch-all although the typed parser reads it is
/// held by nothing else the check sees: dropping it is a violation, on the
/// first fixture line that holds the place.
#[test]
fn dropped_kept_place_exits_3() {
    let tmp = TempDir::new().expect("tempdir");
    for (place, first_line) in [
        ("attachment.addedTypes", 60),
        ("attachment.showConcurrencyNote", 60),
        ("attachment.blockingError", 63),
        ("attachment.imagePasteIds", 67),
        ("attachment.prompt", 67),
        ("attachment.content", 72),
    ] {
        let db = tmp.path().join(format!("{place}.duckdb"));
        let output = ingest(
            &db,
            &[
                ("CCT_VERIFY_RESIDUAL", "1"),
                ("CCT_VERIFY_DROP_PATH", place),
            ],
        );
        let lines = stderr_lines(&output);
        assert_eq!(output.status.code(), Some(3), "{place}: {lines:#?}");
        let expected = format!(
            "error: residual invariant violated in {}:{first_line}",
            fixture_dir().join("catch_all.jsonl").display()
        );
        assert_eq!(violations(&lines), vec![&expected], "{place}");
        assert!(lines
            .iter()
            .all(|line| !line.starts_with("error: parse failure in")));
    }
}

/// The switches change nothing unless the check itself is on: a drop path
/// alone is not read, and a place no line stores is not a violation.
#[test]
fn drop_path_is_read_by_the_check_only() {
    let tmp = TempDir::new().expect("tempdir");
    for (name, env) in [
        ("plain", &[][..]),
        (
            "drop-only",
            &[("CCT_VERIFY_DROP_PATH", "wireToolInputs")][..],
        ),
        (
            "off",
            &[
                ("CCT_VERIFY_RESIDUAL", "0"),
                ("CCT_VERIFY_DROP_PATH", "wireToolInputs"),
            ][..],
        ),
        (
            "nowhere",
            &[
                ("CCT_VERIFY_RESIDUAL", "1"),
                ("CCT_VERIFY_DROP_PATH", "zzNoSuchPlace.zzNor"),
            ][..],
        ),
    ] {
        let db = tmp.path().join(format!("{name}.duckdb"));
        let output = ingest(&db, env);
        let lines = stderr_lines(&output);
        assert_eq!(output.status.code(), Some(0), "{name}: {lines:#?}");
        assert!(violations(&lines).is_empty(), "{name}");
        let conn = Connection::open(&db).expect("open db");
        let extras: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entry_extras WHERE extra::VARCHAR LIKE '%wireToolInputs%'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(extras, 1, "{name}: the stored value is untouched");
    }
}
