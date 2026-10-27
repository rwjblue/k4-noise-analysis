//! Domain types, configuration, survey planning and online visit
//! statistics. No networking or storage.

pub mod aggregate;
pub mod band;
pub mod config;
pub mod measure;
pub mod model;
pub mod plan;
pub mod time;
pub mod units;

pub use config::Config;

/// Program version recorded with every session.
pub const PROGRAM_VERSION: &str = env!("CARGO_PKG_VERSION");
