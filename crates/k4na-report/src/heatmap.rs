//! Frequency x time grids per band and antenna, quantized for embedding.

use std::collections::BTreeMap;

use k4na_analysis::{VisitMetrics, VisitSpectrum};
use serde::Serialize;

/// Fine accumulation grid; bands are later merged to at most
/// [`MAX_COLUMNS`] columns, keeping the maximum excess per column so a
/// one-bin carrier stays visible.
pub const FINE_HZ: i64 = 250;
pub const MAX_COLUMNS: i64 = 1600;
/// Quantized "no observation" marker.
pub const MISSING: u8 = 255;

/// Excess over local background: 0.25 dB steps from 0 dB.
pub fn q_excess(tenths: i32) -> u8 {
    (tenths.max(0) as f64 / 2.5).round().min(254.0) as u8
}

/// Panadapter level: 0.5 dB steps from -170 dB.
pub fn q_level(tenths: f64) -> u8 {
    ((tenths / 10.0 + 170.0) * 2.0).round().clamp(0.0, 254.0) as u8
}

#[derive(Debug)]
struct VisitCols {
    utc_ns: i64,
    tile: Option<String>,
    session: String,
    segment: String,
    floor: i16,
    first_col: i64,
    excess: Vec<u8>,
    level: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Heatmaps {
    groups: BTreeMap<(String, String), Vec<VisitCols>>,
}

/// One band/antenna grid ready to serialize. `excess` and `level` are
/// row-major `rows x cols` bytes, base64-encoded.
#[derive(Debug, Serialize)]
pub struct Grid {
    pub band_id: String,
    pub antenna_id: String,
    pub low_hz: i64,
    pub col_hz: i64,
    pub cols: usize,
    /// Row start times; a row is one pass over the band's tiles. Rows
    /// with `gap` mark unobserved time between passes.
    pub rows: Vec<Row>,
    pub segments: Vec<String>,
    pub excess: String,
    pub level: String,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub utc_ns: i64,
    pub gap: bool,
    /// Index into `segments`, and the median visit floor (native tenths).
    pub segment: Option<usize>,
    pub floor: Option<i16>,
}

impl Heatmaps {
    pub fn add(&mut self, s: &VisitSpectrum, m: &VisitMetrics, bg: &[i16]) {
        let first_col = s.low_hz.div_euclid(FINE_HZ);
        let last_col = (s.high_hz - 1).div_euclid(FINE_HZ);
        let n = usize::try_from(last_col - first_col + 1).unwrap_or(0);
        let mut ex = vec![i32::MIN; n];
        let mut lv = vec![(0f64, 0u32); n];
        for (i, (&p50, &b)) in s.p50.iter().zip(bg).enumerate() {
            let c = (s.bin_center_hz(i) as i64).div_euclid(FINE_HZ) - first_col;
            let Ok(c) = usize::try_from(c) else { continue };
            if c >= n {
                continue;
            }
            ex[c] = ex[c].max(i32::from(p50) - i32::from(b));
            lv[c].0 += f64::from(p50);
            lv[c].1 += 1;
        }
        let v = &s.visit;
        self.groups
            .entry((
                v.band_id.clone().unwrap_or_else(|| "(no band)".into()),
                v.antenna_id.clone(),
            ))
            .or_default()
            .push(VisitCols {
                utc_ns: v.started_utc_ns,
                tile: v.tile_id.clone(),
                session: v.session_id.clone(),
                segment: v.segment_id.clone(),
                floor: m.floor,
                first_col,
                excess: ex
                    .iter()
                    .map(|&e| if e == i32::MIN { MISSING } else { q_excess(e) })
                    .collect(),
                level: lv
                    .iter()
                    .map(|&(sum, k)| if k == 0 { MISSING } else { q_level(sum / f64::from(k)) })
                    .collect(),
            });
    }

    /// Grids in frequency order, then by antenna.
    pub fn finish(self) -> Vec<Grid> {
        let mut out: Vec<Grid> = self
            .groups
            .into_iter()
            .filter_map(|((band, antenna), vs)| grid(band, antenna, vs))
            .collect();
        out.sort_by(|a, b| (a.low_hz, &a.antenna_id).cmp(&(b.low_hz, &b.antenna_id)));
        out
    }
}

/// Splits time-ordered visits into passes: a new pass starts after a
/// pause of more than a minute, on a new session, or when a tile repeats.
fn passes(vs: &[VisitCols]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut tiles = std::collections::BTreeSet::new();
    for i in 0..vs.len() {
        let v = &vs[i];
        let new = i > start
            && (v.utc_ns - vs[i - 1].utc_ns > 60_000_000_000
                || v.session != vs[i - 1].session
                || v.tile.as_ref().is_none_or(|t| tiles.contains(t)));
        if new {
            out.push(start..i);
            start = i;
            tiles.clear();
        }
        if let Some(t) = &v.tile {
            tiles.insert(t.clone());
        }
    }
    if start < vs.len() {
        out.push(start..vs.len());
    }
    out
}

fn grid(band_id: String, antenna_id: String, mut vs: Vec<VisitCols>) -> Option<Grid> {
    vs.sort_by_key(|v| v.utc_ns);
    let lo = vs.iter().map(|v| v.first_col).min()?;
    let hi = vs.iter().map(|v| v.first_col + v.excess.len() as i64).max()?;
    let factor = ((hi - lo) + MAX_COLUMNS - 1) / MAX_COLUMNS;
    let factor = factor.max(1);
    let cols = usize::try_from((hi - lo + factor - 1) / factor).ok()?;
    let passes = passes(&vs);
    let starts: Vec<i64> = passes.iter().map(|r| vs[r.start].utc_ns).collect();
    let mut steps: Vec<i64> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    steps.sort_unstable();
    let typical = steps.get(steps.len().saturating_sub(1) / 2).copied();
    let mut segments: Vec<String> = Vec::new();
    let (mut rows, mut excess, mut level) = (Vec::new(), Vec::new(), Vec::new());
    for (pi, r) in passes.iter().enumerate() {
        if let (Some(t), true) = (typical, pi > 0)
            && starts[pi] - starts[pi - 1] > 2 * t.max(60_000_000_000)
        {
            rows.push(Row {
                utc_ns: starts[pi - 1] + t,
                gap: true,
                segment: None,
                floor: None,
            });
            excess.extend(std::iter::repeat_n(MISSING, cols));
            level.extend(std::iter::repeat_n(MISSING, cols));
        }
        let mut ex = vec![MISSING; cols];
        let mut lv = vec![(0u32, 0u32); cols];
        let mut floors: Vec<i16> = Vec::new();
        for v in &vs[r.clone()] {
            floors.push(v.floor);
            for (k, (&e, &l)) in v.excess.iter().zip(&v.level).enumerate() {
                let c = ((v.first_col + k as i64 - lo) / factor) as usize;
                if e != MISSING {
                    ex[c] = if ex[c] == MISSING { e } else { ex[c].max(e) };
                }
                if l != MISSING {
                    lv[c].0 += u32::from(l);
                    lv[c].1 += 1;
                }
            }
        }
        floors.sort_unstable();
        let seg = &vs[r.start].segment;
        let si = segments.iter().position(|s| s == seg).unwrap_or_else(|| {
            segments.push(seg.clone());
            segments.len() - 1
        });
        rows.push(Row {
            utc_ns: starts[pi],
            gap: false,
            segment: Some(si),
            floor: floors.get((floors.len().max(1) - 1) / 2).copied(),
        });
        excess.extend(ex);
        level.extend(lv.iter().map(|&(s, k)| s.checked_div(k).map_or(MISSING, |v| v as u8)));
    }
    Some(Grid {
        band_id,
        antenna_id,
        low_hz: lo * FINE_HZ,
        col_hz: factor * FINE_HZ,
        cols,
        rows,
        segments,
        excess: base64(&excess),
        level: base64(&level),
    })
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000_000_000;

    fn vc(t: i64, tile: &str, first_col: i64, excess: Vec<u8>) -> VisitCols {
        VisitCols {
            utc_ns: t,
            tile: Some(tile.into()),
            session: "s".into(),
            segment: "seg".into(),
            floor: -1300,
            first_col,
            level: vec![80; excess.len()],
            excess,
        }
    }

    #[test]
    fn passes_split_on_repeated_tiles_pauses_and_sessions() {
        let mut vs = vec![
            vc(0, "a", 0, vec![]),
            vc(MIN / 10, "b", 0, vec![]),
            vc(2 * MIN / 10, "a", 0, vec![]), // tile repeats: new pass
            vc(5 * MIN, "b", 0, vec![]),      // pause: new pass
        ];
        vs.push(VisitCols {
            session: "t".into(),
            ..vc(5 * MIN + 1, "c", 0, vec![])
        });
        assert_eq!(passes(&vs), vec![0..2, 2..3, 3..4, 4..5]);
    }

    #[test]
    fn grid_merges_tiles_marks_gaps_and_keeps_narrow_maxima() {
        let vs = vec![
            vc(0, "a", 100, vec![4, 40, 4]),
            vc(MIN / 10, "b", 103, vec![8, 8]),
            vc(4 * MIN, "a", 100, vec![4, 4, 4]),
            vc(8 * MIN, "a", 100, vec![4, 4, 4]),
            // A long outage, then one more pass of tile a only.
            vc(60 * MIN, "a", 100, vec![1, 2, 3]),
        ];
        let g = grid("10m".into(), "rx1".into(), vs).unwrap();
        assert_eq!((g.low_hz, g.col_hz, g.cols), (100 * FINE_HZ, FINE_HZ, 5));
        let gaps: Vec<bool> = g.rows.iter().map(|r| r.gap).collect();
        assert_eq!(gaps, [false, false, false, true, false]);
        assert_eq!(g.segments, ["seg"]);
        assert_eq!(g.rows[0].floor, Some(-1300));
        assert_eq!(g.rows[3].floor, None);
        assert_eq!(&g.excess[..8], &base64(&[4, 40, 4, 8, 8, 4])[..8]);
        // The last pass never saw columns 3-4.
        let tail = [1, 2, 3, MISSING, MISSING];
        let mut all = Vec::new();
        for row in [
            [4, 40, 4, 8, 8],
            [4, 4, 4, MISSING, MISSING],
            [4, 4, 4, MISSING, MISSING],
            [MISSING; 5],
            tail,
        ] {
            all.extend(row);
        }
        assert_eq!(g.excess, base64(&all));
    }

    #[test]
    fn wide_bands_merge_columns_with_the_maximum() {
        let n = (MAX_COLUMNS * 2) as usize;
        let mut ex = vec![0u8; n];
        ex[7] = 99;
        let g = grid("10m".into(), "rx1".into(), vec![vc(0, "a", 0, ex)]).unwrap();
        assert_eq!((g.col_hz, g.cols), (2 * FINE_HZ, MAX_COLUMNS as usize));
        assert!(g.cols as i64 <= MAX_COLUMNS);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[255, 0, 128]), "/wCA");
    }

    #[test]
    fn quantization_saturates_and_never_emits_the_missing_marker() {
        assert_eq!(q_excess(-50), 0);
        assert_eq!(q_excess(100), 40);
        assert_eq!(q_excess(10_000), 254);
        assert_eq!(q_level(-1300.0), 80);
        assert_eq!(q_level(-5000.0), 0);
        assert_eq!(q_level(5000.0), 254);
    }
}
