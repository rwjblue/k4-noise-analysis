//! SQLite schema version 1. See docs/storage.md for semantics.

pub const FORMAT: &str = "k4na-shard";
pub const SCHEMA_VERSION: u32 = 1;

/// Tables common to both shard kinds.
const COMMON: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS sessions (
    session_id       TEXT PRIMARY KEY,
    mode             TEXT NOT NULL,
    started_utc_ns   INTEGER NOT NULL,
    ended_utc_ns     INTEGER,
    end_reason       TEXT,
    program_version  TEXT NOT NULL,
    decoder_version  TEXT NOT NULL,
    mapping_version  TEXT NOT NULL,
    config_digest    TEXT NOT NULL,
    config_toml      TEXT NOT NULL,
    antenna_id       TEXT NOT NULL,
    antenna_revision TEXT NOT NULL,
    antenna_description TEXT NOT NULL,
    antenna_notes    TEXT NOT NULL,
    receiver         TEXT NOT NULL,
    firmware_json    TEXT NOT NULL,
    snapshot_json    TEXT NOT NULL,
    units            TEXT NOT NULL,
    calibration      TEXT NOT NULL
) STRICT;
"#;

const SUMMARY: &str = r#"
CREATE TABLE IF NOT EXISTS segments (
    segment_id     TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL,
    pan            TEXT NOT NULL,
    antenna_id     TEXT NOT NULL,
    segment_key    TEXT NOT NULL,
    state_json     TEXT NOT NULL,
    first_utc_ns   INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS visits (
    visit_id            TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL,
    segment_id          TEXT NOT NULL,
    pan                 TEXT NOT NULL,
    band_id             TEXT,
    tile_index          INTEGER,
    tile_id             TEXT,
    target_low_hz       INTEGER,
    target_high_hz      INTEGER,
    requested_vfo_hz    INTEGER,
    requested_span_hz   INTEGER,
    center_hz           INTEGER NOT NULL,
    sample_rate_hz      INTEGER NOT NULL,
    bin_count           INTEGER NOT NULL,
    usable_fraction     REAL NOT NULL,
    full_low_hz         INTEGER NOT NULL,
    full_high_hz        INTEGER NOT NULL,
    actual_low_hz       INTEGER NOT NULL,
    actual_high_hz      INTEGER NOT NULL,
    started_utc_ns      INTEGER NOT NULL,
    started_mono_ns     INTEGER NOT NULL,
    first_utc_ns        INTEGER,
    first_mono_ns       INTEGER,
    last_utc_ns         INTEGER,
    last_mono_ns        INTEGER,
    ended_utc_ns        INTEGER NOT NULL,
    ended_mono_ns       INTEGER NOT NULL,
    settle_ms           INTEGER NOT NULL,
    settle_discarded    INTEGER NOT NULL,
    accepted_frames     INTEGER NOT NULL,
    rejected_frames     INTEGER NOT NULL,
    status              TEXT NOT NULL CHECK (status IN ('complete', 'partial', 'aborted')),
    flags               TEXT NOT NULL,
    end_reason          TEXT NOT NULL,
    noise_field_min     INTEGER,
    noise_field_median  INTEGER,
    noise_field_max     INTEGER,
    sequence_repeats    INTEGER,
    radio_state_json    TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS visits_session_time ON visits (session_id, started_utc_ns);
CREATE INDEX IF NOT EXISTS visits_band_time ON visits (band_id, started_utc_ns);
CREATE INDEX IF NOT EXISTS visits_freq ON visits (actual_low_hz, actual_high_hz);
CREATE INDEX IF NOT EXISTS visits_tile ON visits (tile_id);

CREATE TABLE IF NOT EXISTS summaries (
    visit_id          TEXT NOT NULL,
    pan               TEXT NOT NULL,
    codec             TEXT NOT NULL,
    compression       TEXT NOT NULL,
    quantile_definition TEXT NOT NULL,
    stat_names        TEXT NOT NULL,
    bin_count         INTEGER NOT NULL,
    frame_count       INTEGER NOT NULL,
    units             TEXT NOT NULL,
    uncompressed_len  INTEGER NOT NULL,
    sha256            TEXT NOT NULL,
    data              BLOB NOT NULL,
    PRIMARY KEY (visit_id, pan)
) STRICT;

CREATE TABLE IF NOT EXISTS events (
    event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    utc_ns          INTEGER NOT NULL,
    mono_ns         INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    start_utc_ns    INTEGER,
    end_utc_ns      INTEGER,
    band_id         TEXT,
    tile_id         TEXT,
    count           INTEGER,
    message         TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS events_time ON events (session_id, utc_ns);
CREATE INDEX IF NOT EXISTS events_kind ON events (kind, utc_ns);
"#;

const RAW: &str = r#"
CREATE TABLE IF NOT EXISTS raw_frames (
    frame_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    visit_id        TEXT,
    pan             TEXT NOT NULL,
    received_utc_ns INTEGER NOT NULL,
    received_mono_ns INTEGER NOT NULL,
    sequence        INTEGER NOT NULL,
    center_hz       INTEGER NOT NULL,
    sample_rate_hz  INTEGER NOT NULL,
    sampling_policy TEXT NOT NULL,
    compression     TEXT NOT NULL,
    byte_len        INTEGER NOT NULL,
    data            BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS raw_time ON raw_frames (session_id, received_utc_ns);
CREATE INDEX IF NOT EXISTS raw_visit ON raw_frames (visit_id);
"#;

pub fn ddl(kind: crate::shard::ShardKind) -> String {
    match kind {
        crate::shard::ShardKind::Summary => format!("{COMMON}{SUMMARY}"),
        crate::shard::ShardKind::Raw => format!("{COMMON}{RAW}"),
    }
}
