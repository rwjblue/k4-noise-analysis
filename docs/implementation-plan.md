# Agent handoff and implementation plan

The first PR was design-only. Milestones 0-3 and most of 4 are now
implemented and tested offline (fixtures, simulator, accelerated runs).
Passive capture and band/antenna surveys have been validated on a K4D;
receiver measurement setters and kill-and-recover have had one short
live check on one RX input. The
remaining live acceptance items stay open, and the assumptions listed in
[collection.md](collection.md#unverified-on-hardware) must be checked
before unattended use. Continue in small reviewable commits without
conflating an unverified observation with a hardware guarantee.

| Milestone | Status |
| --- | --- |
| 0. Workspace, fixtures, CI | Done |
| 1. Parser, passive capture | Done, including a live 5-minute K4D capture; byte-capped diagnostic dump not yet implemented |
| 2. Storage, aggregation | Done, including accelerated 24 h under the minimum cap (`mise run long-run`) |
| 3. Survey | Done; live K4D 80-10 m passes on one and three antennas (see collection.md); real 24 h dataset open |
| 3a. Measurement profiles, recovery | Done in the simulator ([measurement.md](measurement.md), [recovery.md](recovery.md)): per band/antenna receiver settings, durable journal, `k4na recovery`; one short live K4D check of setters and kill-and-recover on one RX input |
| 4. Quality | `k4na inspect` health summary done; troubleshooting guide and golden whole-survey replay open |
| 5. Analysis | Started: `k4na analyze bands` and `carriers` (crate `k4na-analysis`, see [analysis.md](analysis.md#implemented-k4na-analysis1)); comb families, heatmap report and A/B comparison open |

## Definition of the first useful release

A Rust command-line program that can read a physical K4 LAN spectrum
stream, validate/record passive main-pan observations, safely scan
configured full HF band intervals when expressly authorized, accumulate
per-bin visit summaries from all received frames, keep bounded raw
original packets for replay, rotate/prune disk shards and report
time/frequency coverage/gaps for a 24+ hour run.

An offline birdie detector, near/far comparison dashboard and browser
UI are **not** initial release gates.

## Sequence / atomic goals

### 0. Workspace and fixture provenance
- Rust workspace (2024 edition, stable channel, Cargo.lock for binary).
  Implement CLI config parsing, checks and deterministic logs.
- Agree on source licensing before importing any code or packet captures.
  The Halo code is MPL-2.0 and has user-authored packet fixtures; preserve
  attribution and SPDX when reusing any code. Do not publish the private
  Elecraft C# ZIP/PDF attachments or excerpts without permission.
- Obtain/copy appropriate legally reusable R39 K4 LAN test captures from
  the author's Halo PR or collect new local K4 captures with permission.
- CI: fmt, clippy, tests, strict config parsing, offline fixtures.

Acceptance: cargo test --locked, fmt --check and clippy complete without
hardware/network access. Replays include a known-good real packet.

### 1. Receive-only parser and bounded passive capture
- Incremental :9201 TCP reader with safe reconnect/backoff, framing,
  header sum, strict field parsing, CRC16/IBM-SDLC and resynchronization.
- Expose native signed int16 bins with full header, receiver/pan, rate,
  center, sequence and arrival timestamps. Bound buffers and counts.
- Main A packet capture; recognize B/Y/Z but default to storing A.
- k4na check, capture and replay; passive mode MUST NOT open a write
  path on CAT, even on protocol errors. Save a byte-capped diagnostic
  fixture and inspect frame counts/ranges.

Acceptance: reproduce documented R39 values; fuzz-like tests for
fragmented/corrupt inputs, garbage, wraparound and reconnect; a live
5-minute passive capture with declared measured span and no CAT setters.

### 2. Durable data and streaming statistical aggregation
- Implement native per-visit vector aggregation: min, p10/p50/p90/max
  over every accepted sweep, quality/gap accounting, compressed arrays.
- SQLite hourly summary/raw shards, versioned codec and independent
  read-only inspector. Atomic finalize and crash recovery.
- Configurable exact raw frame thinning <=1 Hz default, raw diagnostic
  cap and independent retention; app-only subtree pruning.
- Preflight and runtime quota controls including WAL/temp and minimum
  free space, logging actual/predicted daily size and dropped raw frames.
- Allow a passive fixed-frequency timed 24-hour capture initially; passive
  observation may not claim that all bands were covered.

Acceptance: 24-hour synthetic/high-speed accelerated simulation stays
below configurable hard cap and records intentional missing data. Process
kill/restart recovers, rotation and eviction work, and no user files outside
the data root are removed.

### 3. Opt-in active, full-band survey
- CAT readbacks and whitelisted setters with explicit user consent and
  an operator-state interlock. No TX, CW or arbitrary CAT scripts.
- Snapshot/restore only collector-owned settings, comparing readbacks
  before restoration to avoid overwriting human changes.
- Read actual sampleRate/center after each retune; dynamically tile
  band intervals with guarded overlapping usable interior windows.
  Fixed-display or mismatch -> reject/record, never mislabel coverage.
- Fair scheduler, configurable dwell/settle, bounded runtime and
  explicit stop/pause conditions. Streaming summaries per visit.
- Cover 80/40/30/20/17/15/12/10m example configuration, including
  the upper 10m band if requested.
- Survey profiles separate from the station file: named ranges (whole
  bands or segments), `min_cycle_ms` cadence and antenna choice.
  Implemented; real-radio cadence and `AR` behavior still unverified.
- Antenna registry with per-visit antenna id/revision/observed port;
  opt-in, readback-verified main-RX `AR` selection for multi-antenna
  profiles, per-band input put back. Validated on a K4D (`AR4`-`AR6`).
- Deterministic receiver measurement settings per band/antenna and a
  durable recovery journal: change only what can be put back, journal
  before sending, restore only the collector's own values, block new
  surveys until an interrupted one is settled. Simulator-tested,
  including a SIGKILLed survey restored by `k4na recovery`; one short
  live K4D check (survey plus `kill -9` and recovery) on one RX input.

Acceptance: mock K4 exercises cross-band BN/FA, split/diversity conflict,
human retune, TX activity, frozen PAN, rate change, wrong header, disconnect,
low disk and graceful shutdown; no actual RF transmission possible via
whitelist. Verify against the real K4 on 80/40/20 for short sessions
before trusting 24-hour unattended collection. Produce a 24h dataset
with per-band coverage and no unaccounted gaps (gaps are allowed if
explicitly recorded).

### 4. Collector quality and reproducibility
- Health summary CLI: frame decode errors, received vs accepted visits,
  per-band observed coverage, age of data, actual storage, raw retention,
  receiver settings/changes, incomplete visits and clock shifts.
- Config examples and troubleshooting (fixed pan tracking, gain/AutoRef,
  collector conflict with QK4, scope rate vs requested span).
- Golden offline replay tests for an entire recorded survey with
  deterministic summaries and clean handling of schema version mismatch.

Acceptance: an agent can independently explain and reproduce a
24-hour baseline from only the config, versioned shards and documentation.
No interface requiring a running radio is necessary for offline analysis.

### 5. Downstream analysis (separate PRs)
- Estimate frequency/time-dependent background and possible carrier
  families, with coverage/confidence and operator annotations.
- Export data and render plots, heatmaps and candidate tables.
- Add paired A/B simultaneous receive support with per-antenna IDs and
  verified readbacks, before/after source tagging and calibrated
  comparison limitations.
- Investigate per-band hardware calibration before emitting dBm.
  Do not let these block collector readiness.

## Cross-cutting tests

Protocol: known hardware fixtures; randomized chunk boundaries; malformed
fields/checksums; sequence wrap, A/B interleaving; bounded resync.

Planner: half-open intervals, overlap, band-edge coverage including the
entire 1.7 MHz 10m interval, rate change, stale center, filter changes,
non-starvation under uneven band width, changing midnight/timezones.

Storage: schema upgrades/rejection, identical replay, crash in middle of
transaction, WAL cleanup, raw/summary independent lifetime, disk-full,
clock jump, quota including temp, and never deleting foreign files.

Radio safety: asserting NO unsafe CAT opcodes can reach the socket,
TX/external-retune pauses before next setter, conditional restoration
and explicit operator intent for writes. Test a simulated transmitting
station and concurrent CAT client.

Measurement integrity: no dBm claims without calibration, no false
"silent" observations for skipped visits, immutable source units, and
profile/gain changes creating new comparability segments.

## Non-goals / things not to shortcut

No regex-guessing radio packets without replay; no OCR/screenshot-derived
power readings; no full-rate indefinitely retained raw stream; no
unbounded queues; no automatic background retuning during QSOs; no
in-place overwriting user-maintained antenna configuration; no claiming
a birdie is household RFI without corroborating evidence.

The agent may propose changes to defaults based on real radio tests, but
must document what was measured, what was inferred and any unverified
behavior. Keep source commits small and logical, update this plan as
milestones ship, and leave the operator's data untracked by Git.
