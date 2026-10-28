//! Static, self-contained HTML report over a data root: per-band
//! frequency x time heatmaps, floors, carriers, families and band/hour
//! summaries. Read-only; needs no radio and no network to view.

pub mod heatmap;

use k4na_analysis::{Filter, Params};
use k4na_store::StoreError;
use k4na_store::reader::DataSet;
use serde::Serialize;

pub use heatmap::{Grid, Heatmaps};

/// Bumped when the report's layout or embedded data contract changes.
pub const REPORT_VERSION: &str = "k4na-report/1";

const TEMPLATE: &str = include_str!("report.html");

#[derive(Debug, Serialize)]
struct Data<'a> {
    report: &'static str,
    generated_utc_ns: i64,
    analysis: &'a k4na_analysis::Analysis,
    grids: &'a [Grid],
}

/// The report as one HTML document. `generated_utc_ns` is recorded, not
/// read from the clock, so identical inputs render identically.
pub fn render(ds: &DataSet, filter: &Filter, params: &Params, generated_utc_ns: i64) -> Result<String, StoreError> {
    let mut maps = Heatmaps::default();
    let analysis = k4na_analysis::run(ds, filter, params, |s, m, bg| maps.add(s, m, bg))?;
    let grids = maps.finish();
    let data = Data {
        report: REPORT_VERSION,
        generated_utc_ns,
        analysis: &analysis,
        grids: &grids,
    };
    // `</` must not appear inside the <script> element.
    let json = serde_json::to_string(&data)?.replace("</", "<\\/");
    Ok(TEMPLATE.replace("/*K4NA_DATA*/null", &json))
}
