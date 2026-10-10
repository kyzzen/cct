//! What the typed round trip of a transcript line loses, and how to put it
//! back.
//!
//! [`residual`] walks a raw JSON value beside the value the typed structs
//! serialise back (`raw -> Entry -> Value`) and returns a sparse nested copy
//! of the raw value that holds only what the round trip lost. [`merge`] is
//! its inverse: merging a round trip with its residual reproduces every leaf
//! of the raw value, which [`unreproduced`] checks.
//!
//! Shape of a residual. Object keys nest under their real names, so a key
//! that contains a dot or a bracket is an ordinary key. A stored element of
//! an array sits under its parent as the key `[i]`. A value is stored whole,
//! without descending into it, when:
//!
//! - the round trip lacks its key (a raw `null` included);
//! - it is an object and the round trip there is an object whose only key is
//!   `type` with a different value (what the unit fallbacks of the typed
//!   enums serialise to);
//! - it is an array and the round trip there is an array of another length;
//! - the two sides hold different scalars, or values of different kinds.
//!
//! Numbers are compared by [`numbers_equal`]. Keys only the round trip holds
//! are not losses and are ignored.
//!
//! One shape cannot be told apart on the way back: a raw object whose keys
//! all read `[i]`, at a place where the round trip holds an array, is stored
//! whole like any value of a different kind, and [`merge`] then reads it as
//! per-element entries. [`unreproduced`] reports that place, so the case is
//! counted instead of lost silently.
//!
//! Every function here is total: no input makes one fail or panic. Recursion
//! depth follows the depth of the raw value, which `serde_json` caps at 128
//! levels when it parses text.

#![deny(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

/// Why the walk stored a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StoreReason {
    /// The round trip lacks the key.
    MissingKey,
    /// The round trip holds a unit fallback there: an object with no key but
    /// `type`, and another `type`.
    Fallback,
    /// Both sides hold an array, of different lengths.
    ArrayLength,
    /// Both sides hold a value and the two differ.
    Differs,
}

/// A place where the walk stored a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPlace {
    /// The place in the path syntax `diff_values` prints (`message.content[]`
    /// with array indices written `[]`); `(root)` for the whole value.
    pub path: String,
    /// Length in bytes of the stored value as compact JSON.
    pub bytes: usize,
    pub reason: StoreReason,
}

#[derive(Clone, Copy)]
enum Seg<'a> {
    Key(&'a str),
    Index(usize),
}

/// The part of `raw` that its typed round trip `rt` lost, or `None` when the
/// round trip lost nothing.
pub fn residual(raw: &Value, rt: &Value) -> Option<Value> {
    walk(raw, rt, &mut Vec::new(), &mut None)
}

/// [`residual`], also appending one [`StoredPlace`] per stored value.
pub fn residual_with_places(
    raw: &Value,
    rt: &Value,
    places: &mut Vec<StoredPlace>,
) -> Option<Value> {
    walk(raw, rt, &mut Vec::new(), &mut Some(places))
}

fn walk<'a>(
    raw: &'a Value,
    rt: &Value,
    path: &mut Vec<Seg<'a>>,
    places: &mut Option<&mut Vec<StoredPlace>>,
) -> Option<Value> {
    match (raw, rt) {
        (Value::Object(a), Value::Object(b)) => {
            if is_fallback(a, b) {
                return store(raw, path, places, StoreReason::Fallback);
            }
            let mut out: Option<Map<String, Value>> = None;
            for (key, va) in a {
                path.push(Seg::Key(key));
                let child = match b.get(key) {
                    None => store(va, path, places, StoreReason::MissingKey),
                    Some(vb) => walk(va, vb, path, places),
                };
                path.pop();
                if let Some(child) = child {
                    out.get_or_insert_with(Map::new).insert(key.clone(), child);
                }
            }
            out.map(Value::Object)
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return store(raw, path, places, StoreReason::ArrayLength);
            }
            let mut out: Option<Map<String, Value>> = None;
            for (i, (va, vb)) in a.iter().zip(b).enumerate() {
                path.push(Seg::Index(i));
                let child = walk(va, vb, path, places);
                path.pop();
                if let Some(child) = child {
                    out.get_or_insert_with(Map::new).insert(index_key(i), child);
                }
            }
            out.map(Value::Object)
        }
        (Value::Number(a), Value::Number(b)) => {
            if numbers_equal(a, b) {
                None
            } else {
                store(raw, path, places, StoreReason::Differs)
            }
        }
        _ => {
            if raw == rt {
                None
            } else {
                store(raw, path, places, StoreReason::Differs)
            }
        }
    }
}

/// True when `rt` is what a unit fallback serialises to (an object with no
/// key but `type`) and its `type` is not the raw one.
fn is_fallback(raw: &Map<String, Value>, rt: &Map<String, Value>) -> bool {
    rt.len() == 1 && rt.contains_key("type") && raw.get("type") != rt.get("type")
}

fn store(
    value: &Value,
    path: &[Seg<'_>],
    places: &mut Option<&mut Vec<StoredPlace>>,
    reason: StoreReason,
) -> Option<Value> {
    if let Some(places) = places.as_deref_mut() {
        places.push(StoredPlace {
            path: render(path, false),
            bytes: json_len(value),
            reason,
        });
    }
    Some(value.clone())
}

/// The Numbers rule: exact equality first; otherwise equal only if both
/// convert to the same `f64` and `raw` is not an integer beyond 2^53 in
/// magnitude. So `5` read into a float field equals `5.0`, and
/// `9007199254740993` read into a float field does not equal what comes back.
pub fn numbers_equal(raw: &Number, other: &Number) -> bool {
    if raw == other {
        return true;
    }
    if is_integer_beyond_2p53(raw) {
        return false;
    }
    match (raw.as_f64(), other.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn is_integer_beyond_2p53(n: &Number) -> bool {
    const LIMIT: u64 = 1 << 53;
    if let Some(u) = n.as_u64() {
        return u > LIMIT;
    }
    if let Some(i) = n.as_i64() {
        return i.unsigned_abs() > LIMIT;
    }
    false
}

/// Puts a residual back into the round trip it was taken from.
///
/// Where both hold an object they are merged key by key. Where the residual
/// holds an object whose keys are all `[i]` and the round trip holds an array
/// that has those elements, they are merged element by element. Anything else
/// in the residual replaces the round trip's value.
pub fn merge(rt: Value, residual: Option<&Value>) -> Value {
    match residual {
        None => rt,
        Some(residual) => merge_value(rt, residual),
    }
}

fn merge_value(rt: Value, residual: &Value) -> Value {
    match (rt, residual) {
        (Value::Object(mut base), Value::Object(extra)) => {
            for (key, value) in extra {
                let merged = match base.remove(key) {
                    Some(old) => merge_value(old, value),
                    None => value.clone(),
                };
                base.insert(key.clone(), merged);
            }
            Value::Object(base)
        }
        (Value::Array(mut items), Value::Object(extra)) if is_index_object(extra, items.len()) => {
            for (key, value) in extra {
                let slot = parse_index_key(key).and_then(|i| items.get_mut(i));
                if let Some(slot) = slot {
                    let old = std::mem::take(slot);
                    *slot = merge_value(old, value);
                }
            }
            Value::Array(items)
        }
        (_, other) => other.clone(),
    }
}

fn index_key(i: usize) -> String {
    format!("[{i}]")
}

/// The index of a key spelled exactly as [`index_key`] spells it.
fn parse_index_key(key: &str) -> Option<usize> {
    let digits = key.strip_prefix('[')?.strip_suffix(']')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

fn is_index_object(map: &Map<String, Value>, len: usize) -> bool {
    !map.is_empty()
        && map
            .keys()
            .all(|key| parse_index_key(key).is_some_and(|i| i < len))
}

/// Places of `raw` that `merged` does not reproduce, in the path syntax
/// `diff_values` prints. Empty when every leaf of `raw` is present in
/// `merged` with an equal value by [`numbers_equal`]. Keys only `merged`
/// holds are allowed.
pub fn unreproduced(raw: &Value, merged: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(raw, Some(merged), &mut Vec::new(), &mut out);
    out
}

fn check<'a>(
    raw: &'a Value,
    merged: Option<&Value>,
    path: &mut Vec<Seg<'a>>,
    out: &mut Vec<String>,
) {
    match raw {
        Value::Object(a) => match merged.and_then(Value::as_object) {
            Some(b) => {
                for (key, va) in a {
                    path.push(Seg::Key(key));
                    check(va, b.get(key), path, out);
                    path.pop();
                }
            }
            None => out.push(render(path, true)),
        },
        Value::Array(a) => match merged.and_then(Value::as_array) {
            Some(b) if a.len() == b.len() => {
                for (i, (va, vb)) in a.iter().zip(b).enumerate() {
                    path.push(Seg::Index(i));
                    check(va, Some(vb), path, out);
                    path.pop();
                }
            }
            _ => out.push(render(path, true)),
        },
        Value::Number(n) => {
            if !matches!(merged, Some(Value::Number(m)) if numbers_equal(n, m)) {
                out.push(render(path, true));
            }
        }
        other => {
            if merged != Some(other) {
                out.push(render(path, true));
            }
        }
    }
}

fn render(path: &[Seg<'_>], with_indices: bool) -> String {
    if path.is_empty() {
        return "(root)".to_owned();
    }
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Seg::Index(i) => {
                if with_indices {
                    let _ = write!(out, "[{i}]");
                } else {
                    out.push_str("[]");
                }
            }
        }
    }
    out
}

struct ByteCounter(usize);

impl std::fmt::Write for ByteCounter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// Length in bytes of `value` as compact JSON, without building the text.
pub fn json_len(value: &Value) -> usize {
    let mut counter = ByteCounter(0);
    let _ = write!(counter, "{value}");
    counter.0
}
