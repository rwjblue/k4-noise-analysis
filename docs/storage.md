# On-disk format and bounded retention

Status: implemented (schema version 4) in crates/k4na-store; no migrations
yet. Readers reject unsupported schema versions. Version 4 changed only
how raw frames are stored (the `shuffle2+zstd` codec and 16 KiB raw-shard
pages, below); readers still decode version 2 and 3 `zstd` frames, and
older binaries refuse version 4 shards at open rather than failing on an
unknown codec per frame. Version 3 added `visits.measurement_json`; readers still open version 2 shards (the
column reads as NULL). Version 2 moved antenna identity from the session
to each visit, because one survey session can measure several antennas.
Version 1 was pre-release and is not read.

Survey recovery journals ([recovery.md](recovery.md)) are not
measurement data. They live in `storage.recovery_dir` (default
`<root>/recovery`), are never pruned and are not counted against the
byte quota. Each is bounded by compaction (well under 300 KiB); resolved
or acknowledged journals may be deleted by hand.

## Why storage must be designed before continuous capture

The verified K4 LAN frame is 4,162 bytes. At an observed 15 frames/sec,
recording just one PAN verbatim is about 5.39 GB per 24 hours, excluding
filesystem overhead; two pans roughly double that. Compression helps but
cannot be treated as a space guarantee.

The collector processes all accepted frames in memory during each dwell.
Store **per-visit statistics** for longer-term reanalysis and a **sparse,
bounded raw sample** of original packets. Keep the two retention classes
in physically separate shard files to permit deletion without SQLite file
growth or VACUUM during collection.

## Shard layout and ownership

~~~text
data/
  2026/10/27/
    summary-20261027T18Z.sqlite
    raw-20261027T18Z.sqlite
    summary-20261027T19Z.sqlite
    raw-20261027T19Z.sqlite
    ...
  annotations.jsonl
~~~

`annotations.jsonl` holds operator marks from `k4na mark`, `k4na watch
--mark` and `k4na import-marks`, one JSON object per line
(`start_utc_ns`, optional `end_utc_ns`, `text` up to 500 characters,
`created_utc_ns`, optional `source`: `watch` or `import:<file name>`),
each appended with one write and fsynced. It is operator input, not
measurement: the collector never writes it, retention never prunes it (it is bytes per note), and its size
counts in the root's usage. `k4na mark` writes only into an existing
k4na data root. Readers skip lines they cannot parse (a torn last line
after a crash) and report the count. Notes are free text, so keep
people's names and addresses out of them like any other data-root
content.

- One writer owns shards; readers open completed shards read-only.
- Hourly UTC rotation, by shard timestamp, not estimated size. At rotation
  checkpoint WAL, close, atomically mark finalized, and continue in new
  files. Allow read-only inspection of completed shards while capture runs.
- No external database, server or SQLite extensions required. Use
  rusqlite (bundled SQLite) and zstd-compressed byte arrays in BLOBs;
  SQLite rows contain metadata and indexes.
- Each shard is independently intelligible: schema version, program and
  decoder version, session ID, configuration digest, radio firmware,
  antenna descriptions/IDs and dataset units appear in shard metadata.
- Never record credentials. The local IP address is optional operational
  metadata and not needed for measurement replay.
- Do not put received audio or IQ in this storage; neither is in the v1
  LAN pan stream.

## Logical schema (names are suggestions; semantics are mandatory)

Meta: format/schema version, creation/finalization time, shard type,
host-clock metadata, collection software/protocol version and provenance.

Session: session UUID; mode (passive/survey); started/ended UTC; source
host fingerprint (optional); survey profile id; the antennas the session
could attribute visits to (`antennas_json`: id, port, revision,
description); equipment firmware; config hash (station plus profile);
start/end reasons. A session can cross shard boundaries.

State segment: UUID; receiver/pan, physical antenna ID and revision;
CAT preamp/attenuation, AGC and noise reduction, RF gain when readable;
pan AutoRef/#REF/#SCL/#SPN/#AVG/#FXT/#NB; diversity/split/TX state;
any unknowns, calibration label and coordinate mapping version.
A changed or missing critical input creates a new segment or rejects
the frame for like-for-like comparison.

Visit: UUID, session/segment, band and configured tile ID, target RF
interval, requested CAT frequency/span, first/last sample UTC and monotonic
elapsed, observed center/sample rate/bin count, accepted and rejected
counts, settle period, actual frequency coverage, completeness flags
and abort/gap reason. Actual bands are half-open [low_hz, high_hz).
The `band_id` is the survey profile's range id (for example `40m` or
`40m-cw`). Survey visits store the resolved measurement settings and CAT
meta-mode as `measurement_json` (a JSON object of strings, `as-found`
where the profile left a setting alone), plus the effective values
verified by readback as `verified.<prefix>` raw readback text; the same
values are part of the segment's state.

Each visit also records its antenna: `antenna_id`, `antenna_revision`,
`antenna_port` (the registry port) and `antenna_observed_port` (the port
the `AR`/`AN` readback resolved to, or NULL when no readback was
available). A NULL observed port carries the `antenna_unverified` flag;
passive visits attributed from readback to a non-declared antenna carry
`antenna_from_readback`. A segment never mixes antennas, and inspection
reports coverage per (band, antenna).

Summary: one row per valid visit and pan, carrying zstd-compressed typed
arrays of native signed tenths-of-dB per bin: min, p10, p50, p90 and max;
per-bin count may be common to the whole visit if each bin received an
equal number of valid frames. Include codec version, array lengths, exact
quantile definition, native endian declaration, uncompressed byte length,
checksum and source-frame count. Aggregates come from *all* accepted frames,
not only sparse raw samples. Do not store "dBm" without a validated
calibration. A computed noise floor is a future derived table, not an
original measurement.

Raw: a sampled original complete :9201 packet BLOB with CRC, associated
visit UUID, receiver, received UTC timestamp, monotonic delta, sampling
policy and capture quality. Bytes MUST replay through the independent
protocol decoder. There is no promise that the raw subset includes rare
transients. Each row's `sampling_policy` names the rule that kept it:
interval sampling (`first accepted frame then >= N ms apart per pan`) or
a full-rate capture (`every accepted frame (full-rate capture ...)`).

Raw codecs (`raw_frames.compression`):
- `shuffle2+zstd` (written since schema 4): the packet's even bytes, then
  its odd bytes, zstd-compressed. Bins are big-endian `i16` at an even
  offset, so this separates each bin's slowly varying high byte from its
  noisy low byte. Lossless for any length.
- `zstd` (schemas 2-3): the packet zstd-compressed as is.
- `none`: stored as is.

Raw shards use 16 KiB SQLite pages. Spectra are noise-dominated and barely
compress (about 2.3 KB of a 4,164-byte packet), so with the default 4 KiB
pages each frame row occupied a page of its own. Measured on one real
K4D survey hour (3,081 frames):

| Raw shard layout | Size |
| --- | --- |
| schema 3: `zstd`, 4 KiB pages | 13.0 MB |
| `shuffle2+zstd`, 4 KiB pages | 13.0 MB |
| `shuffle2+zstd`, 16 KiB pages (schema 4) | 8.9 MB |
| `shuffle2+zstd`, 32 or 64 KiB pages | 8.7-8.8 MB |

Larger pages save almost nothing more and add a page per table and index
to every shard's fixed size. Compressing several frames together saved
only about 10% and was not adopted. Summary shards keep 4 KiB pages:
their rows are larger and pack well (64 KiB pages made one 8.5 MB hour
9.4 MB).

The writer commits raw frames in batches (up to 64 frames or 5 s, and at
close) because each SQLite commit appends whole pages, a table leaf and
two index leaves, to the WAL; per-frame commits would write several
times the data at full rate. A crash can lose the unflushed batch; raw
frames are expendable. WAL auto-checkpoints at about 1 MiB whatever the
page size.

Event/gap: time, session/segment/tile ID, typed reason (TX, external
retune, unknown center, malformed packet, state change, reconnect,
backpressure, disk ceiling, clock jump, shutdown, etc.), observed
counts/duration and a redacted message. Never convert unavailable
time/frequency coverage into low-noise samples.

Implemented indexes include antenna+band+time. Suggested indexes:
session+time, band+time, antenna+pan+time,
actual_low/actual_high frequency window and tile ID.

Schema revisions require an explicit migration/export path. Version the
binary array codec independently from the SQLite schema. Validate byte
lengths, checksum and codec before allocating and decoding.

## Defaults and budgeting

Initial defaults in ../config/example.toml:
- 4 GiB hard cap over all data (including temporary files and WAL);
- 5 GiB minimum remaining filesystem space, with active admission control;
- 48 hours raw retention, up to 30 days summary retention;
- at most one raw PAN frame/second/receiver while a tile is active,
  except during an explicit full-rate capture (below);
- preserve at least 24 hours of **summary** coverage as the target
  protected window, subject to physical disk feasibility and safety.
- summarize all valid packets per dwell at native 2,048-bin resolution.

Illustrative upper-bound scale: five i16 summary arrays occupy
5 * 2048 * 2 = 20,480 bytes per visit before compression and row
overhead. An example 45-tile plan, 3 second dwell plus 700 ms settle
per tile, makes about 23,350 visits/day and ~478 MB of summary arrays
per day before compression. Capturing one original packet/second
continuously would be ~360 MB/day. These are *planning estimates*;
actual frame rates, tile counts, sample sizes and SQLite overhead are
measured at runtime. A 24-hour retention target must not silently
override the hard disk ceiling.

Implement both a preflight --estimate mode and live counters: bytes in
each retention class, total including WAL and temp, volume free bytes,
actual bytes/day, predicted time to cap, dropped raw samples, accepted
visits, gaps, and age of oldest/newest data.

## Eviction / backpressure order

1. Bound RAM regardless of disk state: bounded network/writer channels
   with a deterministic policy. If raw queue is full, drop raw samples,
   record counts and continue visit summary if possible.
2. On an approaching soft threshold stop optional original-frame
   sampling first. Prune expired raw hourly shards, then older raw shards
   regardless of nominal raw retention.
3. Prune completed summary shards older than retention, then the oldest
   summary shards beyond the protected 24-hour coverage target if
   necessary. Never delete open shards.
4. If quota/free-space cannot be maintained while preserving the
   protected summary window, suspend collection safely and explicitly
   report why. Never fill the host disk hoping compression succeeds.
5. WAL/journal/temp files count toward budgets. Reserve enough headroom
   for the largest pending SQLite transaction and rotation/checkpoint.
   The soft and raw-resume thresholds are fractions of the budget *minus*
   headroom, so pressure pruning always starts before the hard ceiling
   (usage + headroom > budget) can suspend collection.
   Admission likewise counts one unprunable shard duration of summary
   and raw bytes on top of the protected window, because pruning is per
   shard and open shards are never deleted.
   Pre-allocate/validate at least a shard's expected headroom before a
   long run; re-check frequently, not just at startup.

**Not a promise of 24h at any disk size:** if the physical volume cannot
support a minimum day's data even after optional raw sampling is disabled,
the collector refuses or halts with an actionable error. Report this
before a 24h survey via estimate/preflight.

Only delete shards created and tracked by this application under its
configured data root; never recurse into unrelated paths. Deletion
requires finalized manifests/SQLite headers and a valid format marker.
Avoid races with readers by coordinating finalized shards and reporting
pruned files.

## Integrity, shutdown and recovery

SQLite writes are transactional. For summaries, commit the complete visit
and state/gap metadata together or write an explicitly partial/incomplete
visit. Crash recovery must recognize open WAL and partial shards; finalization
is idempotent, with a repair/inspect path for dirty shards. Avoid silent
truncation of a visit's percentile arrays.

Use UTC for rotation, and monotonic process time to measure dwell and
elapsed intervals. Persist clock-change events; do not infer a continuous
on-air timeline from a wall-clock jump.

Maintain deterministic fixture playback: given identical raw frames and
config, protocol and aggregation produce identical summaries. After schema
changes, readers must reject unsupported major versions with a clear
error. The repository contains only synthetic/test fixtures and source;
operators' survey DBs, radio IP and runtime logs are gitignored.

## Why not just store screenshots, Parquet or every dB/bin frame?

Screenshots lose exact bins and headers. Parquet is useful **export** later,
but versioned SQLite + compressed i16 arrays provides transactional writes,
indexing, robust recovery and simple Rust readers without a full analytics
stack. Full-rate raw is expensive and unnecessary for an initial baseline;
an explicit capture keeps every accepted frame temporarily, with a
duration and strict byte cap. User-visible coverage and gap accounting are
more valuable than an enormous undifferentiated dump.

The default sample keeps about 3% of frames: 5 of the ~151 accepted in a
5 s dwell at 30 fps. Measured on the 2026-10-28 overnight survey, one
hour took 8.1 MB of summaries and 13.0 MB of raw sample under schema 3,
about 8.9 MB under schema 4 (about 2.9 KB per frame with its row and
indexes). Keeping every frame would be about 93,000 frames, roughly 270 MB
per survey hour or 6.5 GB per day: about 30 times the summaries, which already hold min/p10/p50/p90/max of every frame per bin.
What full rate adds is time structure inside a dwell (bursts, impulse
trains, pulse rates up to half the frame rate) and the freedom to
re-summarize differently later. It does not make a survey continuous:
each tile is still seen for one dwell per cycle.

### Full-rate raw capture

`k4na capture` and `k4na survey` accept `--raw-full-rate <DURATION>` and
`--raw-full-rate-max-bytes <BYTES>` (default 1 GiB; units B, KB, MB, GB,
KiB, MiB, GiB). From the first accepted frame the sampler keeps every
accepted frame until the duration passes or the original packet bytes
kept would exceed the cap, then falls back to
`collection.raw_sample_interval_ms`. Packet bytes bound disk use, since
frames are stored compressed. There is no config-file key: a full-rate
capture is never a default.

It is refused unless it lasts more than 0 and at most 24 hours, ends
within `storage.raw_retention_hours`, keeps at most half of
`storage.max_total_bytes`, and fits beside the protected summary window
and an unprunable shard in the usable budget. A full-rate capture is
still expendable raw data: storage pressure pauses it and prunes raw
shards before summaries. The start and end (with the reason and bytes
kept) are recorded as `raw_capture` events.
