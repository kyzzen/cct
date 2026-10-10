//! Round trip, residual and merge over synthetic inputs.
//!
//! - the ingest crate's `catch_all` fixture, through the typed parser, with
//!   each line's residual compared to `fixtures/catch_all_residuals.json`;
//! - `fixtures/residual_pairs.json`: hand-built (raw, round trip) pairs for
//!   shapes no line can produce through the typed parser;
//! - `fixtures/review_cases.tsv`: further synthetic lines, checked for the
//!   merge invariant only;
//! - generated lines large enough to matter for size.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use claude_code_transcripts::residual::{
    merge, numbers_equal, residual, residual_with_places, unreproduced, StoredPlace,
};
use claude_code_transcripts::types::Entry;
use serde_json::{json, Value};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn catch_all_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../claude-code-transcripts-ingest/tests/fixtures/catch_all/catch_all.jsonl")
}

fn read_json(path: PathBuf) -> Value {
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

/// The typed round trip of a line, or `None` for an unknown entry type.
fn round_trip(raw: &Value) -> Option<Value> {
    let entry: Entry = serde_json::from_value(raw.clone()).expect("typed parse");
    if matches!(entry, Entry::Unknown) {
        return None;
    }
    Some(serde_json::to_value(&entry).expect("re-serialise"))
}

/// Every leaf of `value` (scalars and empty containers) with its JSON pointer.
fn leaves<'a>(value: &'a Value, pointer: &mut String, out: &mut Vec<(String, &'a Value)>) {
    fn escaped(key: &str) -> String {
        key.replace('~', "~0").replace('/', "~1")
    }
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&escaped(key));
                leaves(child, pointer, out);
                pointer.truncate(len);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (i, child) in items.iter().enumerate() {
                let len = pointer.len();
                pointer.push_str(&format!("/{i}"));
                leaves(child, pointer, out);
                pointer.truncate(len);
            }
        }
        leaf => out.push((pointer.clone(), leaf)),
    }
}

/// The merge invariant, checked without `unreproduced`: every raw leaf is
/// found in the merge by JSON pointer with an equal value.
fn assert_every_leaf_present(name: &str, raw: &Value, merged: &Value) {
    let mut found = Vec::new();
    leaves(raw, &mut String::new(), &mut found);
    assert!(!found.is_empty(), "{name}: no leaves");
    for (pointer, leaf) in found {
        let got = merged
            .pointer(&pointer)
            .unwrap_or_else(|| panic!("{name}: {pointer} absent from the merge"));
        let equal = match (leaf, got) {
            (Value::Number(a), Value::Number(b)) => numbers_equal(a, b),
            // Keys the round trip adds inside an empty raw object are allowed.
            (Value::Object(a), Value::Object(_)) if a.is_empty() => true,
            (a, b) => a == b,
        };
        assert!(equal, "{name}: {pointer} is {got} in the merge, {leaf} raw");
    }
}

/// Residual and merge of one line; asserts the invariant both ways.
fn residual_checked(name: &str, raw: &Value, rt: Value) -> Option<Value> {
    let mut places = Vec::new();
    let res = residual_with_places(raw, &rt, &mut places);
    assert_eq!(res, residual(raw, &rt), "{name}: with and without places");
    assert_eq!(res.is_some(), !places.is_empty(), "{name}: places");
    let merged = merge(rt, res.as_ref());
    assert_eq!(unreproduced(raw, &merged), Vec::<String>::new(), "{name}");
    assert_every_leaf_present(name, raw, &merged);
    res
}

#[test]
fn fixture_residuals_match_expected_and_merge_reproduces_every_leaf() {
    let text = fs::read_to_string(catch_all_fixture()).expect("read fixture");
    let expected: BTreeMap<u64, Value> = read_json(fixtures().join("catch_all_residuals.json"))
        .as_array()
        .expect("array")
        .iter()
        .map(|row| (row["line"].as_u64().expect("line"), row["residual"].clone()))
        .collect();

    let mut known = 0;
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx as u64 + 1;
        let name = format!("line {line_no}");
        let raw: Value = serde_json::from_str(line).expect("fixture line is json");
        let Some(rt) = round_trip(&raw) else {
            assert!(!expected.contains_key(&line_no), "{name}: unknown type");
            continue;
        };
        known += 1;

        // An unknown attachment serialises back as a bare `{"type":"unknown"}`.
        if line_no == 14 {
            assert_eq!(rt["attachment"], json!({ "type": "unknown" }));
        }

        let res = residual_checked(&name, &raw, rt).unwrap_or(Value::Null);
        let want = expected
            .get(&line_no)
            .unwrap_or_else(|| panic!("{name}: no expected residual"));
        assert_eq!(&res, want, "{name}");
    }
    assert_eq!(known, 34);
    assert_eq!(expected.len(), 34);
}

#[test]
fn stored_places_use_the_diff_path_syntax() {
    let text = fs::read_to_string(catch_all_fixture()).expect("read fixture");
    let places_of = |line_no: usize| -> Vec<(String, usize)> {
        let line = text.lines().nth(line_no - 1).expect("line");
        let raw: Value = serde_json::from_str(line).expect("json");
        let rt = round_trip(&raw).expect("known type");
        let mut places: Vec<StoredPlace> = Vec::new();
        residual_with_places(&raw, &rt, &mut places);
        let mut out: Vec<(String, usize)> = places.into_iter().map(|p| (p.path, p.bytes)).collect();
        out.sort();
        out
    };
    let place = |path: &str, bytes: usize| (path.to_string(), bytes);

    // One unknown block stored whole, one key inside a known block.
    assert_eq!(
        places_of(16),
        vec![
            place(
                "message.content[]",
                r#"{"payload":{"k":1},"type":"zz_block"}"#.len()
            ),
            place("message.content[].caller.zzNew", 1),
        ]
    );
    // An unknown attachment is one place, whatever its payload.
    assert_eq!(
        places_of(15),
        vec![place("attachment", r#"{"type":"zz_empty_type"}"#.len())]
    );
    // A nested key and a top-level key with a dot print as the same path.
    assert_eq!(
        places_of(30),
        vec![
            place("message.zzNew", r#""nested""#.len()),
            place("message.zzNew", r#""toplevel""#.len()),
            place("zz[0]", 1),
        ]
    );
    assert_eq!(places_of(1), vec![]);
}

#[test]
fn hand_built_pairs() {
    let pairs = read_json(fixtures().join("residual_pairs.json"));
    let pairs = pairs.as_array().expect("array");
    assert_eq!(pairs.len(), 35);
    for pair in pairs {
        let name = pair["name"].as_str().expect("name");
        let raw = &pair["raw"];
        let rt = pair["rt"].clone();
        let want_unreproduced: Vec<String> = pair
            .get("unreproduced")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .map(|s| s.as_str().expect("path").to_string())
                    .collect()
            })
            .unwrap_or_default();

        let res = residual(raw, &rt);
        assert_eq!(
            res.clone().unwrap_or(Value::Null),
            pair["residual"],
            "{name}"
        );

        let merged = merge(rt, res.as_ref());
        assert_eq!(unreproduced(raw, &merged), want_unreproduced, "{name}");
        if want_unreproduced.is_empty() {
            assert_every_leaf_present(name, raw, &merged);
        }
    }
}

#[test]
fn review_cases_merge_reproduces_every_leaf() {
    let text = fs::read_to_string(fixtures().join("review_cases.tsv")).expect("read cases");
    let (mut known, mut unknown, mut rejected) = (0, 0, Vec::new());
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (name, json) = line.split_once('\t').expect("name<TAB>json");
        let raw: Value = serde_json::from_str(json).expect("case is json");
        let entry: Entry = match serde_json::from_value(raw.clone()) {
            Ok(entry) => entry,
            Err(_) => {
                rejected.push(name);
                continue;
            }
        };
        if matches!(entry, Entry::Unknown) {
            unknown += 1;
            continue;
        }
        known += 1;
        let rt = serde_json::to_value(&entry).expect("re-serialise");
        residual_checked(name, &raw, rt);
    }
    // The typed parser refuses a float where it models an unsigned integer.
    assert_eq!(rejected, vec!["float_5.0_on_modelled_u64"]);
    assert_eq!((known, unknown), (29, 2));
}

#[test]
fn large_lines() {
    let envelope = |n: u32| {
        json!({
            "type": "user",
            "uuid": format!("fx-large-{n}"),
            "parentUuid": null,
            "isSidechain": false,
            "sessionId": "fx-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
        })
    };

    // A 5 MB string under an unmodelled key is stored whole.
    let mut raw = envelope(1);
    raw["message"] = json!({ "role": "user", "content": "synthetic" });
    raw["zzHuge"] = Value::String("x".repeat(5_000_000));
    let rt = round_trip(&raw).expect("known type");
    let res = residual_checked("huge string", &raw, rt).expect("residual");
    assert_eq!(res, json!({ "zzHuge": raw["zzHuge"] }));

    // 50,000 content blocks, the last one of an unknown type: one stored
    // element, not the whole array.
    let mut blocks: Vec<Value> = (0..49_999)
        .map(|i| json!({ "type": "text", "text": format!("t{i}") }))
        .collect();
    blocks.push(json!({ "type": "zz_block", "k": 1 }));
    let mut raw = envelope(2);
    raw["message"] = json!({ "role": "user", "content": blocks });
    let rt = round_trip(&raw).expect("known type");
    let res = residual_checked("long array", &raw, rt).expect("residual");
    assert_eq!(
        res,
        json!({ "message": { "content": { "[49999]": { "type": "zz_block", "k": 1 } } } })
    );

    // 120 levels of nesting under an unmodelled key, close to the parser's
    // limit of 128.
    let mut deep = json!("leaf");
    for _ in 0..120 {
        deep = json!({ "k": deep });
    }
    let line = serde_json::to_string(
        &json!({ "type": "mode", "mode": "normal", "sessionId": "s", "zzDeep": deep }),
    )
    .expect("serialise");
    let raw: Value = serde_json::from_str(&line).expect("parse deep line");
    let rt = round_trip(&raw).expect("known type");
    let res = residual_checked("deep nesting", &raw, rt).expect("residual");
    assert_eq!(res, json!({ "zzDeep": raw["zzDeep"] }));
}
