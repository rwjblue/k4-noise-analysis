//! Versioned SQLite measurement shards: a single synchronous writer with
//! rotation, recovery and byte budgets, plus an independent read-only
//! reader for inspection, replay and later analysis.

pub mod codec;
pub mod estimate;
pub mod reader;
pub mod schema;
pub mod shard;
pub mod writer;

pub use writer::{FsSpaceProbe, RawOutcome, RecoveryReport, SpaceProbe, Store, StoreOptions, StoreStatus};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("codec: {0}")]
    Codec(String),
    #[error("{0} is not a k4na shard")]
    NotAShard(std::path::PathBuf),
    #[error(
        "{path}: unsupported schema version {found:?} (this build reads version {})",
        schema::SCHEMA_VERSION
    )]
    UnsupportedSchema {
        path: std::path::PathBuf,
        found: Option<u32>,
    },
    #[error("another k4na writer holds the lock on {0}")]
    Locked(std::path::PathBuf),
    #[error("data root: {0}")]
    BadRoot(String),
    #[error("collection suspended: {0}")]
    Suspended(String),
}
