#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

const EXAMPLE: &str = include_str!("../../../config/example.toml");
const PROFILES: [(&str, &str); 3] = [
    ("hf-full.toml", include_str!("../../../config/surveys/hf-full.toml")),
    ("hf-cw-us.toml", include_str!("../../../config/surveys/hf-cw-us.toml")),
    ("rx-vs-dxc.toml", include_str!("../../../config/surveys/rx-vs-dxc.toml")),
];

fn k4na() -> Command {
    Command::cargo_bin("k4na").unwrap()
}

fn write_config(dir: &Path, mode: &str, cat: u16, pan: u16) -> PathBuf {
    let text = EXAMPLE
        .replace("mode = \"survey\"", &format!("mode = \"{mode}\""))
        .replace("host = \"k4.local\"", "host = \"127.0.0.1\"")
        .replace("cat_port = 9200", &format!("cat_port = {cat}"))
        .replace("spectrum_port = 9201", &format!("spectrum_port = {pan}"))
        .replace(
            "root = \"./data\"",
            &format!("root = {:?}", dir.join("data").display().to_string()),
        )
        .replace("min_free_bytes = 5368709120", "min_free_bytes = 0");
    let path = dir.join(format!("{mode}.toml"));
    std::fs::write(&path, text).unwrap();
    // Profiles resolve relative to the station file, as in config/.
    std::fs::create_dir_all(dir.join("surveys")).unwrap();
    for (name, body) in PROFILES {
        let body = body.replace("min_cycle_ms = 300000", "min_cycle_ms = 0");
        std::fs::write(dir.join("surveys").join(name), body).unwrap();
    }
    path
}

#[test]
fn check_shows_the_profile_and_profile_overrides_apply() {
    let dir = tempfile::tempdir().unwrap();
    let station = write_config(dir.path(), "survey", 9200, 9201);
    k4na()
        .args(["check", "--config"])
        .arg(&station)
        .assert()
        .success()
        .stdout(predicate::str::contains("profile  hf-full"))
        .stdout(predicate::str::contains("antenna  dxc on ant2"));
    k4na()
        .args(["check", "--config"])
        .arg(&station)
        .arg("--profile")
        .arg(dir.path().join("surveys/hf-cw-us.toml"))
        .assert()
        .success()
        .stdout(predicate::str::contains("40m-cw 7.000-7.125 MHz"));
    // Two antennas need the station's switching opt-in.
    k4na()
        .args(["check", "--config"])
        .arg(&station)
        .arg("--profile")
        .arg(dir.path().join("surveys/rx-vs-dxc.toml"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("allow_rx_antenna_switch"));
}

#[test]
fn passive_capture_refuses_an_ambiguous_antenna() {
    let dir = tempfile::tempdir().unwrap();
    let station = write_config(dir.path(), "passive", 9, 9);
    let text = std::fs::read_to_string(&station)
        .unwrap()
        .replace("allow_rx_antenna_switch = false", "allow_rx_antenna_switch = true");
    std::fs::write(&station, text).unwrap();
    k4na()
        .args(["capture", "--duration", "3s", "--config"])
        .arg(&station)
        .arg("--profile")
        .arg(dir.path().join("surveys/rx-vs-dxc.toml"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("exactly one antenna"));
    assert!(!dir.path().join("data").exists());
}

#[test]
fn check_accepts_the_example_and_rejects_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let ok = write_config(dir.path(), "survey", 9200, 9201);
    k4na()
        .args(["check", "--config"])
        .arg(&ok)
        .assert()
        .success()
        .stdout(predicate::str::contains("config OK"));

    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, EXAMPLE.replace("[radio]", "[radio]\ntransmit = true")).unwrap();
    k4na()
        .args(["check", "--config"])
        .arg(&bad)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("transmit"));
}

#[test]
fn survey_requires_cli_consent_before_touching_the_network() {
    let dir = tempfile::tempdir().unwrap();
    // Port 9 (discard) is never contacted: consent is checked first.
    let cfg = write_config(dir.path(), "survey", 9, 9);
    k4na()
        .args(["survey", "--duration", "1m", "--config"])
        .arg(&cfg)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--allow-retune"));
    assert!(!dir.path().join("data").exists(), "no data root before consent");
}

#[test]
fn replay_decodes_the_r39_fixture() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../k4na-protocol/tests/fixtures/k4-spectrum/k4-lan-9201-ayb.bin"
    );
    k4na()
        .args(["--json", "replay", "--input", fixture])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"frames_decoded\":3"));
}

#[test]
fn estimate_reports_admission() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path(), "survey", 9200, 9201);
    k4na()
        .args(["estimate", "--config"])
        .arg(&cfg)
        .assert()
        .success()
        .stdout(predicate::str::contains("admission: OK"));
}

/// Real TCP against the simulator: passive capture, then an authorized
/// survey, then offline inspection of the same data root.
#[test]
fn end_to_end_against_tcp_simulator() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let radio = k4na_sim::SimRadio::new(20);
    radio.faults(|f| f.pan_center_offset_hz = -11_240);
    let sim = rt
        .block_on(k4na_sim::TcpSim::start(radio.clone(), "127.0.0.1", 0, 0))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();

    let passive = write_config(dir.path(), "passive", sim.cat_addr.port(), sim.pan_addr.port());
    k4na()
        .args(["capture", "--duration", "3s", "--config"])
        .arg(&passive)
        .assert()
        .success()
        .stdout(predicate::str::contains("ended: duration reached"));
    assert!(
        radio.setters_received().is_empty(),
        "passive capture sent {:?}",
        radio.setters_received()
    );

    let survey = write_config(dir.path(), "survey", sim.cat_addr.port(), sim.pan_addr.port());
    k4na()
        .args(["survey", "--dry-run", "--duration", "1m", "--config"])
        .arg(&survey)
        .assert()
        .success()
        .stdout(predicate::str::contains("observed pan A"));
    assert!(
        radio.setters_received().is_empty(),
        "dry run sent {:?}",
        radio.setters_received()
    );

    let out = k4na()
        .args(["--json", "probe", "--config"])
        .arg(&survey)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let probe: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(probe["values"]["K4"], "0", "{probe}");
    assert_eq!(probe["values"]["#NBL"], "8", "{probe}");
    assert!(
        probe["commands_sent"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c.as_str().unwrap().len() <= 5),
        "GET forms only: {probe}"
    );
    assert!(
        radio.setters_received().is_empty(),
        "probe sent {:?}",
        radio.setters_received()
    );

    let out = k4na()
        .args(["--json", "survey", "--allow-retune", "--duration", "12s", "--config"])
        .arg(&survey)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(report["halt"], "stop", "{report}");
    assert!(report["restore"].as_str().unwrap().starts_with("restored"), "{report}");
    assert!(report["summary"]["visits"].as_u64().unwrap() >= 2, "{report}");
    assert!(radio.forbidden_commands().is_empty());
    assert_eq!(radio.model().fa, 14_074_000, "radio restored");

    let out = k4na()
        .args(["--json", "inspect", "--data"])
        .arg(dir.path().join("data"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let insp: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(insp["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(insp["dirty_shards"], 0);

    // Offline analysis of the same root, labelled as derived and uncalibrated.
    let out = k4na()
        .args(["--json", "analyze", "bands", "--data"])
        .arg(dir.path().join("data"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let bands: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let prov = &bands["provenance"];
    assert!(prov["visits_analyzed"].as_u64().unwrap() >= 1, "{prov}");
    assert_eq!(prov["algorithm"], "k4na-analysis/2");
    assert!(prov["level_units"].as_str().unwrap().contains("not dBm"));
    assert!(!bands["band_hours"].as_array().unwrap().is_empty(), "{bands}");
    k4na()
        .args(["analyze", "carriers", "--format", "csv", "--data"])
        .arg(dir.path().join("data"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("band_id,antenna_id,hz,"));
    k4na()
        .args(["analyze", "families", "--format", "csv", "--data"])
        .arg(dir.path().join("data"))
        .assert()
        .success()
        .stdout(predicate::str::starts_with("band_id,antenna_id,spacing_hz,"));
    let html = dir.path().join("report.html");
    k4na()
        .args(["report", "--out"])
        .arg(&html)
        .arg("--data")
        .arg(dir.path().join("data"))
        .assert()
        .success();
    let html = std::fs::read_to_string(html).unwrap();
    assert!(
        html.starts_with("<!doctype html>") && html.contains("\"grids\":[{"),
        "report has a heatmap"
    );
    k4na()
        .args(["analyze", "bands", "--since", "yesterday", "--data"])
        .arg(dir.path().join("data"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("RFC 3339"));

    let out = k4na()
        .args(["--json", "recovery", "list", "--config"])
        .arg(&survey)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(list[0]["status"], "resolved", "{list}");
}

/// A survey process killed mid-run (SIGKILL: no cleanup) blocks new
/// surveys until `recovery restore` puts the radio back.
#[test]
fn a_killed_survey_is_recovered_with_the_cli() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let radio = k4na_sim::SimRadio::new(20);
    let sim = rt
        .block_on(k4na_sim::TcpSim::start(radio.clone(), "127.0.0.1", 0, 0))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path(), "survey", sim.cat_addr.port(), sim.pan_addr.port());
    let before = radio.model();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("k4na"))
        .args(["survey", "--allow-retune", "--duration", "10m", "--config"])
        .arg(&cfg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while radio.model().fa == before.fa {
        assert!(std::time::Instant::now() < deadline, "the survey never retuned");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_ne!(radio.model().fa, before.fa, "left retuned by the kill");

    // New surveys are refused, before any network use.
    k4na()
        .args(["survey", "--allow-retune", "--duration", "1m", "--config"])
        .arg(&cfg)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unresolved survey recovery journal"));
    k4na()
        .args(["survey", "--dry-run", "--duration", "1m", "--config"])
        .arg(&cfg)
        .assert()
        .failure()
        .stdout(predicate::str::contains("recovery: unresolved"));

    let out = k4na()
        .args(["--json", "recovery", "list", "--config"])
        .arg(&cfg)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(list[0]["status"], "pending", "{list}");
    let sid = list[0]["session_id"].as_str().unwrap().to_owned();
    k4na()
        .args(["recovery", "show", &sid[..13], "--config"])
        .arg(&cfg)
        .assert()
        .success()
        .stdout(predicate::str::contains("pending: ").and(predicate::str::contains("manual: ")));

    let setters = radio.setters_received().len();
    k4na()
        .args(["recovery", "restore", &sid, "--dry-run", "--config"])
        .arg(&cfg)
        .assert()
        .success()
        .stdout(predicate::str::contains("would_restore"));
    assert_eq!(radio.setters_received().len(), setters, "the dry run is GET-only");
    k4na()
        .args(["recovery", "restore", &sid, "--config"])
        .arg(&cfg)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--allow-retune"));
    assert_eq!(radio.setters_received().len(), setters);

    k4na()
        .args(["recovery", "restore", &sid, "--allow-retune", "--config"])
        .arg(&cfg)
        .assert()
        .success()
        .stdout(predicate::str::contains("restore complete"));
    let m = radio.model();
    assert_eq!(
        (m.bn, m.fa, m.span, m.fixed),
        (before.bn, before.fa, before.span, before.fixed)
    );
    assert!(radio.forbidden_commands().is_empty());
    k4na()
        .args(["recovery", "list", "--config"])
        .arg(&cfg)
        .assert()
        .success()
        .stdout(predicate::str::contains("resolved").and(predicate::str::contains("BLOCKS").not()));
}
