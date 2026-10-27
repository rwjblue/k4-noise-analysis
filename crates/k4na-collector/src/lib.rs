//! K4 acquisition: receive-only spectrum sessions, guarded CAT, passive
//! capture, opt-in survey and the storage writer thread.

pub mod cat;
pub mod net;
pub mod passive;
pub mod runtime;
pub mod spectrum;
pub mod store_task;
pub mod survey;
pub mod visit;

use k4na_core::Config;
use k4na_core::config::Mode;
use k4na_core::model::{RadioState, SessionInfo};
use k4na_core::time::Timestamp;

/// Builds the session record stored in every shard. The stored config is
/// redacted (no radio host); only the user-chosen antenna label and notes
/// describe the setup.
pub fn session_info(
    cfg: &Config,
    mode: Mode,
    session_id: &str,
    started: Timestamp,
    firmware: Vec<String>,
    snapshot: RadioState,
) -> SessionInfo {
    SessionInfo {
        session_id: session_id.to_owned(),
        mode,
        started,
        program_version: k4na_core::PROGRAM_VERSION.to_owned(),
        decoder_version: k4na_protocol::DECODER_VERSION.to_owned(),
        mapping_version: k4na_core::units::MAPPING_VERSION.to_owned(),
        config_digest: cfg.digest(),
        config_toml: toml::to_string(&cfg.redacted()).unwrap_or_default(),
        antenna_id: cfg.antenna.id.clone(),
        antenna_revision: cfg.antenna.revision.clone(),
        antenna_description: cfg.antenna.description.clone(),
        antenna_notes: cfg.antenna.notes.clone(),
        receiver: cfg.radio.receiver.as_str().to_owned(),
        firmware,
        snapshot,
    }
}
