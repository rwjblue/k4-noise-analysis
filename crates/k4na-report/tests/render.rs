#![allow(clippy::unwrap_used)]

use k4na_analysis::{Filter, Params};
use k4na_store::reader::DataSet;

fn embedded(html: &str) -> serde_json::Value {
    let start = html.find(r#"<script id="k4na-data" type="application/json">"#).unwrap();
    let rest = &html[start..];
    let body = &rest[rest.find('>').unwrap() + 1..rest.find("</script>").unwrap()];
    serde_json::from_str(body).unwrap()
}

#[test]
fn an_empty_root_renders_a_self_contained_document() {
    let dir = tempfile::tempdir().unwrap();
    let ds = DataSet::open(dir.path()).unwrap();
    let a = k4na_report::render(&ds, &Filter::default(), &Params::default(), 1_793_059_200_000_000_000).unwrap();
    let b = k4na_report::render(&ds, &Filter::default(), &Params::default(), 1_793_059_200_000_000_000).unwrap();
    assert_eq!(a, b, "deterministic for identical inputs");
    assert!(a.starts_with("<!doctype html>"));
    assert!(
        !a.contains("http://") && !a.contains("https://"),
        "no network resources"
    );
    assert!(a.contains("not dBm"));
    let d = embedded(&a);
    assert_eq!(d["report"], k4na_report::REPORT_VERSION);
    assert_eq!(d["analysis"]["provenance"]["visits_analyzed"], 0);
    assert!(d["analysis"]["harmonic_series"].as_array().unwrap().is_empty());
    assert!(a.contains("id=\"series\""));
    // The paired comparison section exists and is hidden without pairs.
    assert!(a.contains("id=\"pairs\"") && a.contains("id=\"pairsec\" style=\"display:none\""));
    assert!(d["analysis"]["pair_summaries"].as_array().unwrap().is_empty());
    assert!(d["grids"].as_array().unwrap().is_empty());
}

#[test]
fn script_terminators_in_data_are_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let ds = DataSet::open(dir.path()).unwrap();
    let f = Filter {
        band: Some("</script><b>".into()),
        ..Default::default()
    };
    let html = k4na_report::render(&ds, &f, &Params::default(), 0).unwrap();
    assert_eq!(
        html.matches("</script>").count(),
        2,
        "only the template's own closing tags"
    );
    assert_eq!(
        embedded(&html)["analysis"]["provenance"]["filter"]["band"],
        "</script><b>"
    );
}
