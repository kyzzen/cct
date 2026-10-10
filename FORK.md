# Fork notes

This repository is a fork of [Alfredvc/cct](https://github.com/Alfredvc/cct). It exists to store the transcript fields that the upstream ingest drops, while keeping the upstream parser and schema untouched so that upstream releases stay easy to merge.

## Base

- Upstream tag: `v0.1.12`
- Upstream commit: `8070f9bc873e32e81730229a8f42a7cff4975c5a`
- Fork branch: `corpusdb`, cut from that tag
- Remotes: `origin` is the fork, `upstream` is `https://github.com/Alfredvc/cct.git`

## What differs from upstream

Two upstream files must never change in this fork: `crates/claude-code-transcripts/src/types.rs` and `crates/claude-code-transcripts-ingest/src/schema.rs`. New logic goes into new files. Changes to other upstream files are kept to a few lines each.

Check at any time:

```bash
git diff --stat v0.1.12..HEAD -- crates/claude-code-transcripts/src/types.rs crates/claude-code-transcripts-ingest/src/schema.rs   # prints nothing
git diff --stat v0.1.12..HEAD
```

| File | Kind | Change |
|---|---|---|
| `FORK.md` | new | This file. |
| `crates/claude-code-transcripts-ingest/src/cli.rs` | changed, 1 line | `cct --version` prints `cct 0.1.12+corpusdb.N`: upstream's version with a fork suffix. `N` goes up for every build that is gated. `Cargo.toml` keeps upstream's version. |
| `crates/claude-code-transcripts-ingest/src/update.rs` | changed, 8 lines added | `cct update` prints that this build does not self-update and exits 2, because an update would replace the fork binary with an upstream release. The subcommand and its help text stay. |
| `crates/claude-code-transcripts/src/lib.rs` | changed, 1 line added | `pub mod residual;` |
| `crates/claude-code-transcripts/src/residual.rs` | new | `residual(raw, round_trip)`: the sparse nested copy of a raw line that holds only what the typed round trip lost. `merge`: its inverse. `unreproduced`: the check that a merge reproduces every raw leaf. Total functions; the file denies `unwrap`, indexing and `panic` through clippy. |
| `crates/claude-code-transcripts/examples/residual_census.rs` | new | Parse-only census over a directory: per entry type and stored place, how many lines and bytes the round trip loses. Prints paths, names and counts, never a value. The input directory is a required argument. |
| `crates/claude-code-transcripts/tests/residual.rs` | new | Residual, merge and stored places over the `catch_all` fixture, hand-built pairs, further synthetic lines and generated large lines. |
| `crates/claude-code-transcripts/tests/fixtures/` | new | `catch_all_residuals.json` (expected residual per fixture line), `residual_pairs.json` (hand-built raw and round-trip pairs), `review_cases.tsv` (further synthetic lines). |
| `crates/claude-code-transcripts-ingest/tests/catch_all.rs` | new | End-to-end test of the fork's behaviour on one synthetic fixture directory. |
| `crates/claude-code-transcripts-ingest/tests/fixtures/catch_all/catch_all.jsonl` | new | Synthetic fixture: one line per way a line can carry something the typed parser does not model. Lines 34 and 35 are entry types the parser does not know. |
| `crates/claude-code-transcripts-ingest/tests/fixtures/catch_all/unknown_only.jsonl` | new | Synthetic fixture: a file that holds unknown entry types only. |
| `crates/claude-code-transcripts-ingest/src/fork_schema.rs` | new | DDL of the twelve tables the fork adds (`FORK_SCHEMA_DDL`) and one unique index per table on its key (`FORK_INDEXES_DDL`). No upstream table gains a column. |
| `crates/claude-code-transcripts-ingest/src/lib.rs` | changed, 2 lines added | `pub mod extras;` and `pub mod fork_schema;` |
| `crates/claude-code-transcripts-ingest/src/extras.rs` | new | The rows of the fork's tables for one line. Typed fields are read from the raw line with lenient accessors; a value leaves the catch-all only when the value rebuilt from its row equals the raw one; the residual of the line against the typed round trip is its `entry_extras` row. Also the `unparsed_lines` row of a line whose `type` the parser does not know, the counters behind `ingest_drift` and `ingest_meta`, the insert of those two tables (`write_summary`) and the stderr lines about the catch-all (`Drift::report`). With `CCT_VERIFY_RESIDUAL=1` in the environment every line is checked: the typed round trip with the promoted values, merged with the line's `extra`, must reproduce every leaf of the raw line, or the process prints `error: residual invariant violated in <file>:<line>` and exits 3. The file denies `unwrap`, indexing and `panic` through clippy. |
| `crates/claude-code-transcripts-ingest/src/parse.rs` | changed, 16 lines added, 2 changed | `ParsedFile` gains `unparsed` and `drift`. A line of an unknown `type` is recorded before it is skipped. After `build_rows` succeeds, `extras::attach` adds the fork's rows to the entry's child rows. `json_str` becomes `pub(crate)` so that the fork's JSON columns are filled by the same function as upstream's. |
| `crates/claude-code-transcripts-ingest/src/run.rs` | changed, 17 lines added, 1 changed | Runs `FORK_SCHEMA_DDL` right after upstream's schema and `FORK_INDEXES_DDL` right after upstream's indexes. A parsed file is sent to the writer when it has entries or unparsed lines; the writer appends its `unparsed_lines` rows and adds up its counters, and writes the `transcripts` row only when the file has entries, as upstream does. After the commit, `extras::write_summary` fills `ingest_drift` and `ingest_meta`. The ingest report on stderr gains the catch-all lines after the unknown-variants block. |
| `crates/claude-code-transcripts-ingest/tests/residual_verify_control.rs` | new | Runs the built `cct` over the fixture with `CCT_VERIFY_RESIDUAL=1`: exit 0 as it is, exit 3 when `CCT_VERIFY_DROP_PATH` removes one stored place before the check. That second variable is read by the check only and exists for this test. |
| `crates/claude-code-transcripts-ingest/tests/generated_inputs.rs` | new | End-to-end test over input generated in a temporary directory: more distinct catch-all places than `ingest_drift` keeps under their own name, values of about 2 MB, a list of 20,000 items, an escaped and a raw NUL character, a CRLF line end, and promoted keys on a line of another entry type. |

Every later change adds its row here.

## Build

Requirements: the Rust toolchain pinned in `rust-toolchain.toml` (1.95.0, with rustfmt and clippy), `node` and `npm` on `PATH`, and network access. The ingest crate's build script runs `npm ci` and a web build for the bundled viewer when `npm` is found, and there is no prebuilt `web/dist` in the tree to fall back on.

```bash
cargo build --release --locked -p claude-code-transcripts-ingest
```

The binary is `target/release/cct`. Keep `--locked`: `Cargo.toml` and `Cargo.lock` are upstream's and are not edited here.

Checks run before every commit (the first two are the repository's own `.githooks/pre-commit`):

```bash
cargo fmt --check
cargo clippy --all-targets -- --deny warnings
cargo test --workspace --locked
```

## Merging a later upstream tag

```bash
git fetch upstream --tags
git checkout corpusdb
git merge <new-tag>
```

1. Conflicts can only appear in the upstream files listed in the table above as changed. Resolve each by taking upstream's version and re-applying the fork's lines from the table.
2. Confirm that `types.rs` and `schema.rs` are identical to the new tag: `git diff --stat <new-tag>..HEAD -- crates/claude-code-transcripts/src/types.rs crates/claude-code-transcripts-ingest/src/schema.rs` prints nothing.
3. Run the three checks above. A failing upstream test means a fork change altered upstream behaviour: fix the fork change, never the upstream test.
4. Update the base tag and commit at the top of this file, and replace `v0.1.12` in the commands above with the new tag.
