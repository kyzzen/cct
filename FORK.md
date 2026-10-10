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
| `crates/claude-code-transcripts-ingest/tests/catch_all.rs` | new | End-to-end test of the fork's behaviour on one synthetic fixture directory. |
| `crates/claude-code-transcripts-ingest/tests/fixtures/catch_all/catch_all.jsonl` | new | Synthetic fixture: one line per way a line can carry something the typed parser does not model. Lines 34 and 35 are entry types the parser does not know. |
| `crates/claude-code-transcripts-ingest/tests/fixtures/catch_all/unknown_only.jsonl` | new | Synthetic fixture: a file that holds unknown entry types only. |

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
