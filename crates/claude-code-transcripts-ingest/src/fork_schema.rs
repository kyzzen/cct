//! DDL of the tables this fork adds. Upstream's `schema.rs` is not edited.
//!
//! No upstream relation changes: everything the typed parser drops goes into
//! the twelve tables below. `entry_id` is `entries.entry_id` of the same
//! ingest. JSON columns hold serialised JSON text, like upstream's.
//!
//! - Catch-all: `entry_extras` (what the typed round trip of a line lost,
//!   after promotion), `unparsed_lines` (lines whose `type` the parser does
//!   not know), `attachment_raw_types` (the real type name behind an
//!   `attachment_entries` row written as `unknown`).
//! - Typed: fields promoted out of the catch-all. A value leaves
//!   `entry_extras` only when its row rebuilds it exactly.
//! - Drift and identity: `ingest_drift` (what sits in the catch-all, per
//!   entry type and place), `ingest_meta` (which build wrote the database,
//!   and its line counts).

pub const FORK_SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS entry_extras (
    entry_id                       BIGINT,
    extra                          JSON
);

CREATE TABLE IF NOT EXISTS unparsed_lines (
    file_path                      TEXT,
    line_no                        BIGINT,
    raw_type                       TEXT,
    session_id                     TEXT,
    payload                        JSON
);

CREATE TABLE IF NOT EXISTS attachment_raw_types (
    entry_id                       BIGINT,
    raw_type                       TEXT
);

CREATE TABLE IF NOT EXISTS assistant_turn_fields (
    entry_id                       BIGINT,
    thinking_duration_ms           BIGINT,
    advisor_model                  TEXT,
    input_transformations_count    INTEGER
);

CREATE TABLE IF NOT EXISTS assistant_input_transformations (
    entry_id                       BIGINT,
    position                       INTEGER,
    transformation_type            TEXT,
    path                           TEXT,
    reason                         TEXT
);

CREATE TABLE IF NOT EXISTS user_turn_fields (
    entry_id                       BIGINT,
    turn_origin                    TEXT,
    turn_prompt_index              BIGINT,
    turn_index                     BIGINT
);

CREATE TABLE IF NOT EXISTS user_classifier_contexts (
    entry_id                       BIGINT,
    request_id                     TEXT,
    live_cwd                       TEXT,
    platform                       TEXT,
    git_root                       TEXT,
    git_branch                     TEXT,
    git_status_clean               BOOLEAN,
    context                        JSON
);

CREATE TABLE IF NOT EXISTS attachment_rendered (
    entry_id                       BIGINT,
    rendered_role                  TEXT,
    rendered                       JSON
);

CREATE TABLE IF NOT EXISTS attachment_thinking_drops (
    entry_id                       BIGINT,
    model                          TEXT,
    request_id                     TEXT,
    query_source                   TEXT,
    thinking_blocks_sent           BIGINT,
    thinking_turns_sent            BIGINT,
    newly_dropped_block_count      BIGINT,
    newly_dropped_turn_count       BIGINT,
    newly_dropped_reason           TEXT,
    payload                        JSON
);

CREATE TABLE IF NOT EXISTS attachment_prompt_snapshots (
    entry_id                       BIGINT,
    model                          TEXT,
    system_prompt                  JSON,
    tools                          JSON,
    settings                       JSON
);

CREATE TABLE IF NOT EXISTS ingest_drift (
    entry_type                     TEXT,
    attachment_type                TEXT,
    path                           TEXT,
    lines                          BIGINT,
    bytes                          BIGINT
);

CREATE TABLE IF NOT EXISTS ingest_meta (
    key                            TEXT,
    value                          TEXT
);
"#;

// One unique index per table on its key, built once after the load, as
// upstream builds its own.
pub const FORK_INDEXES_DDL: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS uq_entry_extras_pk                        ON entry_extras(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_unparsed_lines_pk                      ON unparsed_lines(file_path, line_no);
CREATE UNIQUE INDEX IF NOT EXISTS uq_attachment_raw_types_pk                ON attachment_raw_types(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assistant_turn_fields_pk               ON assistant_turn_fields(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assistant_input_transformations_pk     ON assistant_input_transformations(entry_id, position);
CREATE UNIQUE INDEX IF NOT EXISTS uq_user_turn_fields_pk                    ON user_turn_fields(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_user_classifier_contexts_pk            ON user_classifier_contexts(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_attachment_rendered_pk                 ON attachment_rendered(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_attachment_thinking_drops_pk           ON attachment_thinking_drops(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_attachment_prompt_snapshots_pk         ON attachment_prompt_snapshots(entry_id);
CREATE UNIQUE INDEX IF NOT EXISTS uq_ingest_drift_pk                        ON ingest_drift(entry_type, attachment_type, path);
CREATE UNIQUE INDEX IF NOT EXISTS uq_ingest_meta_pk                         ON ingest_meta(key);
"#;
