#![allow(dead_code, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use k4na_collector::net::{BoxStream, ConnectFuture, Connector};
use k4na_collector::runtime::{RunContext, Stop, TokioClock};
use k4na_collector::store_task::StoreHandle;
use k4na_core::Config;
use k4na_core::config::Mode;
use k4na_core::time::{Clock, NS_PER_SEC};
use k4na_sim::{SimRadio, serve_cat, serve_pan};
use k4na_store::{FsSpaceProbe, SpaceProbe, Store, StoreOptions};

pub const T0: i64 = 1_793_059_200 * NS_PER_SEC; // 2026-10-27T00:00:00Z

#[derive(Debug, Clone, Copy)]
pub enum Port {
    Cat,
    Pan,
}

/// Connects to the simulator through an in-memory duplex stream.
#[derive(Debug, Clone)]
pub struct SimConnector {
    pub radio: SimRadio,
    pub port: Port,
}

impl Connector for SimConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async move {
            let (client, server) = tokio::io::duplex(256 * 1024);
            let radio = self.radio.clone();
            match self.port {
                Port::Cat => tokio::spawn(serve_cat(radio, server)),
                Port::Pan => tokio::spawn(serve_pan(radio, server)),
            };
            Ok(Box::new(client) as BoxStream)
        })
    }

    fn describe(&self) -> String {
        format!("sim:{:?}", self.port)
    }
}

pub const STATION: &str = include_str!("../../../../config/example.toml");
pub const HF_FULL: &str = include_str!("../../../../config/surveys/hf-full.toml");
pub const RX_VS_DXC: &str = include_str!("../../../../config/surveys/rx-vs-dxc.toml");
#[allow(dead_code)]
pub const RX_VS_DXC_PAIRED: &str = include_str!("../../../../config/surveys/rx-vs-dxc-paired.toml");

pub fn station(root: &Path, mode: &str) -> String {
    STATION
        .replace("mode = \"survey\"", &format!("mode = \"{mode}\""))
        .replace("root = \"./data\"", &format!("root = {:?}", root.display().to_string()))
        .replace("min_free_bytes = 5368709120", "min_free_bytes = 0")
}

/// The example station with the `hf-full` profile, back-to-back cycles and
/// a 3 s dwell (cadence has its own tests).
pub fn config(root: &Path, mode: &str) -> Config {
    let profile = HF_FULL
        .replace("min_cycle_ms = 300000", "min_cycle_ms = 0")
        .replace("dwell_ms = 5000", "dwell_ms = 3000");
    Config::from_parts(&station(root, mode), Some(&profile)).unwrap()
}

pub fn clock() -> Arc<dyn Clock> {
    Arc::new(TokioClock::simulated(T0))
}

pub fn ctx<'a>(
    cfg: &'a Config,
    store: &'a StoreHandle,
    clock: &Arc<dyn Clock>,
    sid: &str,
    stop: Stop,
) -> RunContext<'a> {
    RunContext {
        cfg,
        store,
        clock: clock.clone(),
        session_id: sid.to_owned(),
        stop,
    }
}

pub fn open_store(cfg: &Config, session_id: &str, mode: Mode, clock: &Arc<dyn Clock>) -> StoreHandle {
    open_store_with(cfg, session_id, mode, clock, Box::new(FsSpaceProbe))
}

pub fn open_store_with(
    cfg: &Config,
    session_id: &str,
    mode: Mode,
    clock: &Arc<dyn Clock>,
    probe: Box<dyn SpaceProbe>,
) -> StoreHandle {
    let info = k4na_collector::session_info(cfg, mode, session_id, clock.now(), vec![], Default::default());
    let (store, _) = Store::open(StoreOptions::from_config(&cfg.storage), probe, info, clock.now()).unwrap();
    StoreHandle::inline(store)
}

/// Free space under test control.
#[derive(Debug, Clone)]
pub struct FakeDisk(pub Arc<AtomicU64>);

impl SpaceProbe for FakeDisk {
    fn available_bytes(&self, _: &Path) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
