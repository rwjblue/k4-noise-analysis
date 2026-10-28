//! Read-only access to a data root. Never modifies shards, needs no radio
//! and does not require the writer to be stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::StoreError;
use crate::codec::decode_raw;
use crate::codec::decode_stats;
use crate::shard::{ShardKind, ShardMeta, ShardName, discover, open_read_only};

#[derive(Debug, Clone, Serialize)]
pub struct ShardInfo {
    pub path: PathBuf,
    pub kind: &'static str,
    pub period_start_utc_ns: i64,
    pub bytes: u64,
    pub finalized: bool,
    pub recovered: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct DataSet {
    root: PathBuf,
    shards: Vec<(ShardName, ShardInfo)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VisitRow {
    pub visit_id: String,
    pub session_id: String,
    pub segment_id: String,
    pub pan: String,
    pub band_id: Option<String>,
    pub tile_id: Option<String>,
    pub target_low_hz: Option<i64>,
    pub target_high_hz: Option<i64>,
    pub requested_vfo_hz: Option<i64>,
    pub center_hz: i64,
    pub sample_rate_hz: i64,
    pub bin_count: i64,
    pub actual_low_hz: i64,
    pub actual_high_hz: i64,
    pub started_utc_ns: i64,
    pub ended_utc_ns: i64,
    /// Monotonic times of the first and last accepted frame (`None` with
    /// no frames).
    pub first_mono_ns: Option<i64>,
    pub last_mono_ns: Option<i64>,
    pub accepted_frames: i64,
    pub rejected_frames: i64,
    pub settle_discarded: i64,
    pub status: String,
    pub flags: String,
    pub end_reason: String,
    pub antenna_id: String,
    pub antenna_port: String,
    /// `None` when the attribution was not verified by readback.
    pub antenna_observed_port: Option<String>,
    /// Profile measurement settings JSON (NULL for passive visits and v2
    /// shards).
    pub measurement_json: Option<String>,
}

impl VisitRow {
    /// Delivered frames per second, rounded; see
    /// [`k4na_core::model::delivered_frame_rate`].
    pub fn frame_rate(&self) -> Option<u32> {
        let mono = |t: Option<i64>| t.and_then(|t| u64::try_from(t).ok());
        k4na_core::model::delivered_frame_rate(
            u64::try_from(self.accepted_frames).unwrap_or(0),
            mono(self.first_mono_ns),
            mono(self.last_mono_ns),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub mode: String,
    pub started_utc_ns: i64,
    pub ended_utc_ns: Option<i64>,
    pub end_reason: Option<String>,
    pub profile_id: Option<String>,
    /// Antenna ids the session could attribute visits to.
    pub antennas: Vec<String>,
    pub receiver: String,
    pub firmware: String,
    pub config_digest: String,
    pub snapshot: String,
    pub units: String,
    pub calibration: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub session_id: String,
    pub utc_ns: i64,
    pub kind: String,
    pub start_utc_ns: Option<i64>,
    pub end_utc_ns: Option<i64>,
    pub band_id: Option<String>,
    pub tile_id: Option<String>,
    pub count: Option<i64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawRow {
    pub session_id: String,
    pub visit_id: Option<String>,
    pub pan: String,
    pub received_utc_ns: i64,
    pub center_hz: i64,
    pub sample_rate_hz: i64,
    /// The rule that kept this frame (interval sampling or a full-rate
    /// capture); see docs/storage.md.
    pub sampling_policy: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BandCoverage {
    pub band_id: String,
    pub antenna_id: String,
    pub complete_visits: u64,
    pub other_visits: u64,
    pub tiles: u64,
    /// Union of planned tile targets (survey) seen in the data.
    pub target_hz: u64,
    /// Union of usable observed spectrum from complete visits, clipped to targets
    /// when targets exist.
    pub covered_hz: u64,
    pub first_utc_ns: Option<i64>,
    pub last_utc_ns: Option<i64>,
    /// Largest interval between consecutive complete visits of the same tile.
    pub max_revisit_s: Option<f64>,
    pub median_revisit_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InspectReport {
    pub root: PathBuf,
    pub summary_shards: u64,
    pub raw_shards: u64,
    pub dirty_shards: u64,
    pub unreadable_shards: Vec<(PathBuf, String)>,
    pub summary_bytes: u64,
    pub raw_bytes: u64,
    pub sessions: Vec<SessionRow>,
    pub visits_total: u64,
    pub visits_by_status: BTreeMap<String, u64>,
    pub accepted_frames: u64,
    pub rejected_frames: u64,
    pub settle_discarded_frames: u64,
    pub segments: u64,
    pub bands: Vec<BandCoverage>,
    pub events_by_kind: BTreeMap<String, u64>,
    pub gap_events: u64,
    pub gap_seconds: f64,
    pub raw_frames: u64,
    pub first_utc_ns: Option<i64>,
    pub last_utc_ns: Option<i64>,
    pub bytes_per_day: Option<f64>,
    pub recent_problems: Vec<EventRow>,
}

impl DataSet {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        if !root.is_dir() {
            return Err(StoreError::BadRoot(format!("{} is not a directory", root.display())));
        }
        let mut shards = Vec::new();
        for f in discover(root)? {
            let info = match open_read_only(&f.path) {
                Ok((_, meta)) => info_from(&f.path, &f.name, f.bytes, &meta, None),
                Err(e) => info_from(&f.path, &f.name, f.bytes, &ShardMeta::default(), Some(e.to_string())),
            };
            shards.push((f.name, info));
        }
        Ok(Self {
            root: root.to_owned(),
            shards,
        })
    }

    pub fn shards(&self) -> impl Iterator<Item = &ShardInfo> {
        self.shards.iter().map(|(_, i)| i)
    }

    fn readable(&self, kind: ShardKind) -> impl Iterator<Item = &ShardInfo> {
        self.shards
            .iter()
            .filter(move |(n, i)| n.kind == kind && i.error.is_none())
            .map(|(_, i)| i)
    }

    fn each_conn<T>(
        &self,
        kind: ShardKind,
        mut f: impl FnMut(&Connection) -> Result<T, StoreError>,
    ) -> Result<Vec<T>, StoreError> {
        let mut out = Vec::new();
        for info in self.readable(kind) {
            let (conn, _) = open_read_only(&info.path)?;
            out.push(f(&conn)?);
        }
        Ok(out)
    }

    /// Operator annotations (see [`crate::annotations`]) and the count of
    /// unreadable lines skipped.
    pub fn annotations(&self) -> Result<(Vec<crate::annotations::Annotation>, u64), StoreError> {
        crate::annotations::read(&self.root)
    }

    pub fn sessions(&self) -> Result<Vec<SessionRow>, StoreError> {
        let mut by_id: BTreeMap<String, SessionRow> = BTreeMap::new();
        for rows in self.each_conn(ShardKind::Summary, |c| {
            let mut st = c.prepare(
                "SELECT session_id, mode, started_utc_ns, ended_utc_ns, end_reason, antennas_json, receiver, firmware_json,
                        config_digest, snapshot_json, units, calibration, profile_id FROM sessions",
            )?;
            let rows = st
                .query_map([], |r| {
                    let antennas: String = r.get(5)?;
                    Ok(SessionRow {
                        session_id: r.get(0)?,
                        mode: r.get(1)?,
                        started_utc_ns: r.get(2)?,
                        ended_utc_ns: r.get(3)?,
                        end_reason: r.get(4)?,
                        profile_id: r.get(12)?,
                        antennas: antenna_ids(&antennas),
                        receiver: r.get(6)?,
                        firmware: r.get(7)?,
                        config_digest: r.get(8)?,
                        snapshot: r.get(9)?,
                        units: r.get(10)?,
                        calibration: r.get(11)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })? {
            for row in rows {
                let entry = by_id.entry(row.session_id.clone()).or_insert_with(|| row.clone());
                if row.ended_utc_ns.is_some() {
                    entry.ended_utc_ns = row.ended_utc_ns;
                    entry.end_reason = row.end_reason.clone();
                }
            }
        }
        let mut v: Vec<_> = by_id.into_values().collect();
        v.sort_by_key(|s| s.started_utc_ns);
        Ok(v)
    }

    pub fn visits(&self) -> Result<Vec<VisitRow>, StoreError> {
        let mut out: Vec<VisitRow> = self
            .each_conn(ShardKind::Summary, |c| {
                let mut st = c.prepare(&format!(
                    "SELECT {} FROM visits v ORDER BY v.started_utc_ns",
                    visit_columns(c)?
                ))?;
                let rows = st.query_map([], visit_from_row)?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })?
            .into_iter()
            .flatten()
            .collect();
        out.sort_by_key(|v| v.started_utc_ns);
        Ok(out)
    }

    pub fn events(&self) -> Result<Vec<EventRow>, StoreError> {
        let mut out: Vec<EventRow> = self
            .each_conn(ShardKind::Summary, |c| {
                let mut st = c.prepare(
                    "SELECT session_id, utc_ns, kind, start_utc_ns, end_utc_ns, band_id, tile_id, count, message FROM events ORDER BY utc_ns",
                )?;
                let rows = st
                    .query_map([], |r| {
                        Ok(EventRow {
                            session_id: r.get(0)?,
                            utc_ns: r.get(1)?,
                            kind: r.get(2)?,
                            start_utc_ns: r.get(3)?,
                            end_utc_ns: r.get(4)?,
                            band_id: r.get(5)?,
                            tile_id: r.get(6)?,
                            count: r.get(7)?,
                            message: r.get(8)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })?
            .into_iter()
            .flatten()
            .collect();
        out.sort_by_key(|e| e.utc_ns);
        Ok(out)
    }

    /// Decoded `[min, p10, p50, p90, max]` arrays for a visit, validated.
    pub fn summary(&self, visit_id: &str) -> Result<Option<[Vec<i16>; 5]>, StoreError> {
        for info in self.readable(ShardKind::Summary) {
            let (conn, _) = open_read_only(&info.path)?;
            let row = conn.query_row(
                "SELECT codec, compression, bin_count, uncompressed_len, sha256, data FROM summaries WHERE visit_id = ?1",
                [visit_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u32>(2)?,
                        r.get::<_, u32>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                    ))
                },
            );
            match row {
                Ok((codec, comp, bins, len, sha, data)) => {
                    return decode_stats(&codec, &comp, bins, len, &sha, &data).map(Some);
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// Calls `f` with every visit that has a stored summary, together with
    /// its validated `[min, p10, p50, p90, max]` arrays: one query per
    /// shard, oldest first within a shard. A shard's visits are not sorted
    /// across shards; callers needing global order sort by time.
    pub fn for_each_summary(
        &self,
        mut f: impl FnMut(&VisitRow, [Vec<i16>; 5]) -> Result<(), StoreError>,
    ) -> Result<u64, StoreError> {
        let mut n = 0;
        for info in self.readable(ShardKind::Summary) {
            let (conn, _) = open_read_only(&info.path)?;
            let mut st = conn.prepare(&format!(
                "SELECT {}, s.codec, s.compression, s.bin_count, s.uncompressed_len, s.sha256, s.data
                 FROM visits v JOIN summaries s ON s.visit_id = v.visit_id AND s.pan = v.pan
                 ORDER BY v.started_utc_ns",
                visit_columns(&conn)?
            ))?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let visit = visit_from_row(r)?;
                let stats = decode_stats(
                    &r.get::<_, String>(VISIT_COLUMNS)?,
                    &r.get::<_, String>(VISIT_COLUMNS + 1)?,
                    r.get(VISIT_COLUMNS + 2)?,
                    r.get(VISIT_COLUMNS + 3)?,
                    &r.get::<_, String>(VISIT_COLUMNS + 4)?,
                    &r.get::<_, Vec<u8>>(VISIT_COLUMNS + 5)?,
                )?;
                f(&visit, stats)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Calls `f` with every stored original frame, oldest first per shard.
    pub fn for_each_raw(&self, mut f: impl FnMut(RawRow) -> Result<(), StoreError>) -> Result<u64, StoreError> {
        let mut n = 0;
        for info in self.readable(ShardKind::Raw) {
            let (conn, _) = open_read_only(&info.path)?;
            let mut st = conn.prepare(
                "SELECT session_id, visit_id, pan, received_utc_ns, center_hz, sample_rate_hz, compression, byte_len, data,
                        sampling_policy
                 FROM raw_frames ORDER BY frame_id",
            )?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let compression: String = r.get(6)?;
                let byte_len: i64 = r.get(7)?;
                let data: Vec<u8> = r.get(8)?;
                let bytes = decode_raw(&compression, usize::try_from(byte_len).unwrap_or(usize::MAX), &data)?;
                f(RawRow {
                    session_id: r.get(0)?,
                    visit_id: r.get(1)?,
                    pan: r.get(2)?,
                    received_utc_ns: r.get(3)?,
                    center_hz: r.get(4)?,
                    sample_rate_hz: r.get(5)?,
                    sampling_policy: r.get(9)?,
                    bytes,
                })?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn inspect(&self) -> Result<InspectReport, StoreError> {
        let mut r = InspectReport {
            root: self.root.clone(),
            ..Default::default()
        };
        for (name, info) in &self.shards {
            match name.kind {
                ShardKind::Summary => {
                    r.summary_shards += 1;
                    r.summary_bytes += info.bytes;
                }
                ShardKind::Raw => {
                    r.raw_shards += 1;
                    r.raw_bytes += info.bytes;
                }
            }
            if let Some(e) = &info.error {
                r.unreadable_shards.push((info.path.clone(), e.clone()));
            } else if !info.finalized {
                r.dirty_shards += 1;
            }
        }
        r.sessions = self.sessions()?;
        let visits = self.visits()?;
        let mut segments = BTreeSet::new();
        for v in &visits {
            r.visits_total += 1;
            *r.visits_by_status.entry(v.status.clone()).or_default() += 1;
            r.accepted_frames += v.accepted_frames as u64;
            r.rejected_frames += v.rejected_frames as u64;
            r.settle_discarded_frames += v.settle_discarded as u64;
            segments.insert(v.segment_id.clone());
            r.first_utc_ns = Some(r.first_utc_ns.map_or(v.started_utc_ns, |t| t.min(v.started_utc_ns)));
            r.last_utc_ns = Some(r.last_utc_ns.map_or(v.ended_utc_ns, |t| t.max(v.ended_utc_ns)));
        }
        r.segments = segments.len() as u64;
        r.bands = band_coverage(&visits);
        for e in self.events()? {
            *r.events_by_kind.entry(e.kind.clone()).or_default() += 1;
            if e.kind == "gap" {
                r.gap_events += 1;
                if let (Some(s), Some(t)) = (e.start_utc_ns, e.end_utc_ns) {
                    r.gap_seconds += (t - s).max(0) as f64 / 1e9;
                }
            }
            if matches!(
                e.kind.as_str(),
                "transmit_detected"
                    | "external_change"
                    | "readback_mismatch"
                    | "storage_suspended"
                    | "shard_recovered"
                    | "clock_jump"
                    | "warning"
            ) {
                r.recent_problems.push(e);
            }
        }
        let keep = r.recent_problems.len().saturating_sub(20);
        r.recent_problems.drain(..keep);
        r.raw_frames = self
            .each_conn(ShardKind::Raw, |c| {
                Ok(c.query_row("SELECT count(*) FROM raw_frames", [], |row| row.get::<_, i64>(0))?)
            })?
            .into_iter()
            .sum::<i64>() as u64;
        if let (Some(a), Some(b)) = (r.first_utc_ns, r.last_utc_ns) {
            let days = (b - a) as f64 / 86_400e9;
            if days > 1.0 / 24.0 {
                r.bytes_per_day = Some((r.summary_bytes + r.raw_bytes) as f64 / days);
            }
        }
        Ok(r)
    }
}

fn info_from(path: &Path, name: &ShardName, bytes: u64, meta: &ShardMeta, error: Option<String>) -> ShardInfo {
    ShardInfo {
        path: path.to_owned(),
        kind: name.kind.as_str(),
        period_start_utc_ns: name.period_start_utc_ns,
        bytes,
        finalized: meta.finalized_utc_ns.is_some(),
        recovered: meta.recovered,
        error,
    }
}

/// Length of the union of half-open intervals.
pub fn union_len(mut ranges: Vec<(i64, i64)>) -> u64 {
    ranges.retain(|(a, b)| b > a);
    ranges.sort_unstable();
    let mut total = 0i64;
    let mut cur: Option<(i64, i64)> = None;
    for (a, b) in ranges {
        cur = match cur {
            Some((s, e)) if a <= e => Some((s, e.max(b))),
            Some((s, e)) => {
                total += e - s;
                Some((a, b))
            }
            None => Some((a, b)),
        };
    }
    if let Some((s, e)) = cur {
        total += e - s;
    }
    total as u64
}

/// Number of columns [`visit_columns`] selects.
const VISIT_COLUMNS: usize = 28;

/// The `visits v` columns [`visit_from_row`] reads, in order.
fn visit_columns(c: &Connection) -> Result<String, StoreError> {
    // Schema v2 shards predate measurement_json.
    let measurement = if has_column(c, "visits", "measurement_json")? {
        "v.measurement_json"
    } else {
        "NULL"
    };
    Ok(format!(
        "v.visit_id, v.session_id, v.segment_id, v.pan, v.band_id, v.tile_id, v.target_low_hz, v.target_high_hz,
         v.requested_vfo_hz, v.center_hz, v.sample_rate_hz, v.bin_count, v.actual_low_hz, v.actual_high_hz,
         v.started_utc_ns, v.ended_utc_ns, v.accepted_frames, v.rejected_frames, v.settle_discarded, v.status, v.flags,
         v.end_reason, v.antenna_id, v.antenna_port, v.antenna_observed_port, {measurement}, v.first_mono_ns, v.last_mono_ns"
    ))
}

fn visit_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<VisitRow> {
    Ok(VisitRow {
        visit_id: r.get(0)?,
        session_id: r.get(1)?,
        segment_id: r.get(2)?,
        pan: r.get(3)?,
        band_id: r.get(4)?,
        tile_id: r.get(5)?,
        target_low_hz: r.get(6)?,
        target_high_hz: r.get(7)?,
        requested_vfo_hz: r.get(8)?,
        center_hz: r.get(9)?,
        sample_rate_hz: r.get(10)?,
        bin_count: r.get(11)?,
        actual_low_hz: r.get(12)?,
        actual_high_hz: r.get(13)?,
        started_utc_ns: r.get(14)?,
        ended_utc_ns: r.get(15)?,
        accepted_frames: r.get(16)?,
        rejected_frames: r.get(17)?,
        settle_discarded: r.get(18)?,
        status: r.get(19)?,
        flags: r.get(20)?,
        end_reason: r.get(21)?,
        antenna_id: r.get(22)?,
        antenna_port: r.get(23)?,
        antenna_observed_port: r.get(24)?,
        measurement_json: r.get(25)?,
        first_mono_ns: r.get(26)?,
        last_mono_ns: r.get(27)?,
    })
}

fn has_column(c: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let mut st = c.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let names = st.query_map([], |r| r.get::<_, String>(0))?;
    for n in names {
        if n? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn antenna_ids(json: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Id {
        id: String,
    }
    serde_json::from_str::<Vec<Id>>(json)
        .map(|v| v.into_iter().map(|a| a.id).collect())
        .unwrap_or_default()
}

/// Coverage per (band, antenna): data from different antennas is never
/// merged.
fn band_coverage(visits: &[VisitRow]) -> Vec<BandCoverage> {
    let mut by_band: BTreeMap<(String, String), Vec<&VisitRow>> = BTreeMap::new();
    for v in visits {
        by_band
            .entry((
                v.band_id.clone().unwrap_or_else(|| "(no band)".into()),
                v.antenna_id.clone(),
            ))
            .or_default()
            .push(v);
    }
    let mut out = Vec::new();
    for ((band_id, antenna_id), vs) in by_band {
        let mut c = BandCoverage {
            band_id,
            antenna_id,
            ..Default::default()
        };
        let mut targets = Vec::new();
        let mut covered = Vec::new();
        let mut tiles: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for v in &vs {
            if let (Some(lo), Some(hi)) = (v.target_low_hz, v.target_high_hz) {
                targets.push((lo, hi));
            }
            if v.status == "complete" {
                c.complete_visits += 1;
                let (lo, hi) = match (v.target_low_hz, v.target_high_hz) {
                    (Some(tl), Some(th)) => (v.actual_low_hz.max(tl), v.actual_high_hz.min(th)),
                    _ => (v.actual_low_hz, v.actual_high_hz),
                };
                covered.push((lo, hi));
                tiles
                    .entry(v.tile_id.clone().unwrap_or_default())
                    .or_default()
                    .push(v.started_utc_ns);
            } else {
                c.other_visits += 1;
            }
            c.first_utc_ns = Some(c.first_utc_ns.map_or(v.started_utc_ns, |t| t.min(v.started_utc_ns)));
            c.last_utc_ns = Some(c.last_utc_ns.map_or(v.ended_utc_ns, |t| t.max(v.ended_utc_ns)));
        }
        c.target_hz = union_len(targets);
        c.covered_hz = union_len(covered);
        c.tiles = tiles.len() as u64;
        let mut gaps: Vec<f64> = Vec::new();
        for times in tiles.values_mut() {
            times.sort_unstable();
            gaps.extend(times.windows(2).map(|w| (w[1] - w[0]) as f64 / 1e9));
        }
        if !gaps.is_empty() {
            gaps.sort_by(f64::total_cmp);
            c.max_revisit_s = gaps.last().copied();
            c.median_revisit_s = Some(gaps[(gaps.len() - 1) / 2]);
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::union_len;

    #[test]
    fn union_of_intervals() {
        assert_eq!(union_len(vec![(0, 10), (5, 15), (20, 25), (25, 30), (3, 4)]), 25);
        assert_eq!(union_len(vec![]), 0);
        assert_eq!(union_len(vec![(10, 5)]), 0);
    }
}
