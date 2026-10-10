//! Census of what the typed round trip loses, over a directory of transcripts.
//!
//! Reads every `.jsonl` file under the input directory the way the ingest
//! does, computes the residual of each line of a known entry type, checks
//! that merging it back reproduces the raw line, and prints one JSON object.
//! The output holds file paths, key names, type names and counts, never a
//! value. Files are opened for reading only.
//!
//! Usage: residual_census [--no-residual] <input-dir>
//!
//! `--no-residual` stops after the typed parse of each line; it is the
//! timing control.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::{json, Value};
use walkdir::WalkDir;

use claude_code_transcripts::residual::{json_len, merge, residual_with_places, unreproduced};
use claude_code_transcripts::types::Entry;

const MAX_LOCATIONS: usize = 20;

/// A count plus the first `MAX_LOCATIONS` places (file, line) in sort order.
#[derive(Default)]
struct Located {
    count: u64,
    first: Vec<(String, usize)>,
}

impl Located {
    fn add(&mut self, file: &str, line: usize) {
        self.count += 1;
        self.first.push((file.to_owned(), line));
        self.trim();
    }

    fn absorb(&mut self, other: Located) {
        self.count += other.count;
        self.first.extend(other.first);
        self.trim();
    }

    fn trim(&mut self) {
        if self.first.len() > MAX_LOCATIONS {
            self.first.sort();
            self.first.truncate(MAX_LOCATIONS);
        }
    }

    fn to_json(&self) -> Value {
        let mut first = self.first.clone();
        first.sort();
        let locations: Vec<Value> = first
            .iter()
            .map(|(file, line)| json!({ "file": file, "line": line }))
            .collect();
        json!({ "count": self.count, "locations": locations })
    }
}

#[derive(Default, Clone, Copy)]
struct Counts {
    lines: u64,
    bytes: u64,
}

#[derive(Default)]
struct Tally {
    files: u64,
    input_bytes: u64,
    lines: u64,
    open_errors: Located,
    read_errors: Located,
    not_json: Located,
    typed_parse_errors: Located,
    reserialize_errors: Located,
    merge_failures: Located,
    unknown_entry_lines: BTreeMap<String, u64>,
    typed_lines: BTreeMap<String, u64>,
    residual_rows: u64,
    residual_bytes: u64,
    null_only_rows: u64,
    /// (entry type or `attachment:<type>`, stored place) -> lines and bytes.
    records: BTreeMap<(String, String), Counts>,
}

impl Tally {
    fn absorb(mut self, other: Tally) -> Tally {
        self.files += other.files;
        self.input_bytes += other.input_bytes;
        self.lines += other.lines;
        self.open_errors.absorb(other.open_errors);
        self.read_errors.absorb(other.read_errors);
        self.not_json.absorb(other.not_json);
        self.typed_parse_errors.absorb(other.typed_parse_errors);
        self.reserialize_errors.absorb(other.reserialize_errors);
        self.merge_failures.absorb(other.merge_failures);
        for (key, n) in other.unknown_entry_lines {
            *self.unknown_entry_lines.entry(key).or_default() += n;
        }
        for (key, n) in other.typed_lines {
            *self.typed_lines.entry(key).or_default() += n;
        }
        self.residual_rows += other.residual_rows;
        self.residual_bytes += other.residual_bytes;
        self.null_only_rows += other.null_only_rows;
        for (key, counts) in other.records {
            let mine = self.records.entry(key).or_default();
            mine.lines += counts.lines;
            mine.bytes += counts.bytes;
        }
        self
    }
}

fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    match map.get_mut(key) {
        Some(n) => *n += 1,
        None => {
            map.insert(key.to_owned(), 1);
        }
    }
}

fn type_name(value: &Value) -> &str {
    match value.get("type") {
        Some(Value::String(s)) => s,
        Some(_) => "(type is not a string)",
        None => "(no type)",
    }
}

/// The entry type, or `attachment:<raw type name>` for an attachment line.
fn line_label(raw: &Value) -> String {
    let ty = type_name(raw);
    if ty == "attachment" {
        let inner = raw.get("attachment").map_or("(no attachment)", type_name);
        format!("attachment:{inner}")
    } else {
        ty.to_owned()
    }
}

/// True when the residual holds nothing but nulls.
fn is_null_only(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => !map.is_empty() && map.values().all(is_null_only),
        _ => false,
    }
}

fn census_file(path: &Path, with_residual: bool, tally: &mut Tally) {
    let file_name = path.to_string_lossy();
    tally.files += 1;

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            tally.open_errors.add(&file_name, 0);
            return;
        }
    };
    if let Ok(meta) = file.metadata() {
        tally.input_bytes += meta.len();
    }

    // Same line handling as the ingest's `parse_file`.
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line_no = idx + 1;
        let raw_line = match line {
            Ok(l) => l,
            Err(_) => {
                tally.read_errors.add(&file_name, line_no);
                break;
            }
        };
        let cleaned: String = raw_line.chars().filter(|c| *c != '\0').collect();
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            continue;
        }
        tally.lines += 1;

        let raw: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                tally.not_json.add(&file_name, line_no);
                continue;
            }
        };

        let entry: Entry = match serde_json::from_value(raw.clone()) {
            Ok(e) => e,
            Err(_) => {
                tally.typed_parse_errors.add(&file_name, line_no);
                continue;
            }
        };

        if matches!(entry, Entry::Unknown) {
            bump(&mut tally.unknown_entry_lines, type_name(&raw));
            continue;
        }

        let label = line_label(&raw);
        bump(&mut tally.typed_lines, &label);
        if !with_residual {
            continue;
        }

        let rt = match serde_json::to_value(&entry) {
            Ok(v) => v,
            Err(_) => {
                tally.reserialize_errors.add(&file_name, line_no);
                continue;
            }
        };
        drop(entry);

        let mut places = Vec::new();
        let residual = residual_with_places(&raw, &rt, &mut places);
        let merged = merge(rt, residual.as_ref());
        if !unreproduced(&raw, &merged).is_empty() {
            tally.merge_failures.add(&file_name, line_no);
        }

        let Some(residual) = residual else {
            continue;
        };
        tally.residual_rows += 1;
        tally.residual_bytes += json_len(&residual) as u64;
        if is_null_only(&residual) {
            tally.null_only_rows += 1;
        }

        // A place stored several times on one line (once per array element)
        // counts as one line.
        places.sort_by(|a, b| a.path.cmp(&b.path));
        let mut previous: Option<String> = None;
        for place in places {
            let first_on_line = previous.as_deref() != Some(place.path.as_str());
            let counts = tally
                .records
                .entry((label.clone(), place.path.clone()))
                .or_default();
            counts.bytes += place.bytes as u64;
            if first_on_line {
                counts.lines += 1;
            }
            previous = Some(place.path);
        }
    }
}

/// Same file selection as the ingest's `discover`.
fn discover(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && p.file_name().and_then(|s| s.to_str()) != Some("permissions_log.jsonl")
        })
        .collect();
    out.sort();
    out
}

fn usage() -> ! {
    eprintln!("Usage: residual_census [--no-residual] <input-dir>");
    std::process::exit(2);
}

fn main() {
    let mut with_residual = true;
    let mut input: Option<PathBuf> = None;
    for arg in std::env::args_os().skip(1) {
        if arg == "--no-residual" {
            with_residual = false;
        } else if arg.to_string_lossy().starts_with("--") || input.is_some() {
            usage();
        } else {
            input = Some(PathBuf::from(arg));
        }
    }
    let Some(input) = input else {
        usage();
    };
    if !input.is_dir() {
        eprintln!("error: not a directory: {}", input.display());
        std::process::exit(2);
    }

    let files = discover(&input);
    eprintln!(
        "census: {} .jsonl files under {} on {} threads{}",
        files.len(),
        input.display(),
        rayon::current_num_threads(),
        if with_residual { "" } else { " (no residual)" }
    );

    let tally = files
        .par_iter()
        .fold(Tally::default, |mut tally, path| {
            census_file(path, with_residual, &mut tally);
            tally
        })
        .reduce(Tally::default, Tally::absorb);

    let distinct_places: std::collections::BTreeSet<&str> = tally
        .records
        .keys()
        .map(|(_, place)| place.as_str())
        .collect();
    let records: Vec<Value> = tally
        .records
        .iter()
        .map(|((ty, place), counts)| {
            json!({ "type": ty, "place": place, "lines": counts.lines, "bytes": counts.bytes })
        })
        .collect();

    let out = json!({
        "input_dir": input.to_string_lossy(),
        "residual": with_residual,
        "files": tally.files,
        "input_bytes": tally.input_bytes,
        "lines": tally.lines,
        "errors": {
            "open": tally.open_errors.to_json(),
            "read": tally.read_errors.to_json(),
            "not_json": tally.not_json.to_json(),
            "typed_parse": tally.typed_parse_errors.to_json(),
            "reserialize": tally.reserialize_errors.to_json(),
        },
        "unknown_entry_lines": {
            "total": tally.unknown_entry_lines.values().sum::<u64>(),
            "by_type": tally.unknown_entry_lines,
        },
        "typed_lines": {
            "total": tally.typed_lines.values().sum::<u64>(),
            "by_type": tally.typed_lines,
        },
        "residual_rows": tally.residual_rows,
        "residual_bytes": tally.residual_bytes,
        "null_only_rows": tally.null_only_rows,
        "merge_failures": tally.merge_failures.to_json(),
        "distinct_places": distinct_places.len(),
        "record_count": records.len(),
        "records": records,
    });

    match serde_json::to_string_pretty(&out) {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("error: write output: {e}");
            std::process::exit(1);
        }
    }
}
