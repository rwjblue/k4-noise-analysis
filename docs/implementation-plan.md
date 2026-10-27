# Agent handoff and implementation plan

The first architecture PR has been merged. The repository now contains
initial protocol decoding, strict config, survey planning, aggregation
and versioned SQLite storage code; the CLI/collector remain scaffolds.
Keep progress already landed on main. The follow-on
[operations design](operations.md) supersedes the earlier heavyweight
CAT interlock requirement and makes a minimal local Start/Stop UI part
of the first useful release. Do not conflate inferred radio behavior
with verified hardware measurements.

## Definition of the first useful release

A Rust local-first program that reads a physical K4 LAN spectrum stream,
records passive observations and performs operator-started surveys across
configured HF bands. It must preserve all-frame per-bin visit summaries,
retain only bounded raw examples, rotate/prune disk shards and report
actual frequency/time coverage and gaps for 24+ hours. **Also provide
a tiny authenticated LAN-capable Start/Stop/status page** so an operator
can pause collection from a phone before using the K4/QK4.

A detailed birdie analyzer, near/far comparison dashboard and graphical
measurement plots are **not** first-release gates. The simple operations
control page is distinct from a full analysis/rendering dashboard.

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

### 3. Opt-in active, full-band survey and cadence
- CAT readbacks and whitelisted receive-only setters with explicit
  manual operator consent. No TX, CW or arbitrary CAT scripts.
- Remove expensive external-VFO/operator detection and conditional
  startup-frequency restoration. A requested Stop cancels further
  retunes, flushes partial observations and leaves receiver frequency
  unchanged. Fresh TX-state veto immediately before each retune remains.
- Read actual sampleRate/center after each retune; dynamically tile
  bands with verified packet coverage. Fixed/mismatched pan -> gap or
  replan, never mislabel frequency.
- Default measured dwell **5 seconds** + **700 ms** settle, minimum
  full-cycle start-to-start period **5 minutes**. Existing planner
  yields **50 tiles** at 96 kHz and 80% usable fraction, i.e. 285
  seconds before overhead. Report actual revisit time. Do not skip
  tiles or overlap cycles to meet a timer.
- Extend the strict config schema with `min_cycle_interval_ms`, update
  the sample TOML, unit tests, estimation and operator documentation.
  Remove/deprecate mandatory `pause_on_external_change` and
  `manual_resume_on_conflict` fields.
- Cover the entire configured 80/40/30/20/17/15/12/10m intervals.

Acceptance: mock K4 exercises cross-band BN/FA, transmit-before-retune,
a competing CAT client, frozen PAN, rate change, wrong header, disconnect,
low disk and rapid Stop. The collector emits no TX commands or invalid
measurements. Short real-K4 tests precede trust in unattended overnight
surveys; 24-hour output includes honest band/time coverage and gaps.

### 3b. Minimal persistent web Start/Stop control (first release)
- Implement one collector lifecycle actor shared by headless CLI and
  `k4na serve`. Serve starts the HTTP process with acquisition Stopped;
  an authenticated Start begins a new configured survey session, and
  Stop cancels immediately and gracefully. No child collector processes,
  automatic retuning restart, or second SQLite writer.
- Single embedded static mobile-first HTML page with Running/Stopped/
  Error, Start and Stop, current band/tile, last sweep and disk quota.
  `GET /api/status` (browser polls ~2 seconds),
  `POST /api/start` and `POST /api/stop`. No live spectrum plot
  or arbitrary CAT command API.
- Bind loopback by default; opt-in LAN binding requires strong secret,
  session auth/CSRF protections, and no public Internet exposure.
  `[control]` config is initially a planned schema extension and must
  be implemented/tested before uncommenting the example.
- CLI `--allow-retune` remains the headless opt-in; a genuinely
  authenticated web Start plus config `allow_retune=true` replaces
  the CLI flag in serve mode.
- Add integration tests for repeat Start/Stop, radio disconnect during
  Stop, browser refresh, restart default Stopped, invalid auth, and
  rapid cancellation without stale CAT writes or partial-data loss.

Acceptance: an operator on the same trusted LAN can stop a running
survey from an iPhone browser without reaching the collector host,
then restart it later; web status matches the underlying collector
and closing the phone browser never stops the service.

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

Radio safety: no unsafe CAT opcode can reach the socket; a fresh TX
veto before every retune; explicit web/CLI Start consent; Stop immediately
blocks new retunes without restoring startup VFO. A concurrent CAT
client may race; never mislabel its observations or claim an exclusive
lock. Test browser controls and a simulated transmitting station.

Measurement integrity: no dBm claims without calibration, no false
"silent" observations for skipped visits, immutable source units, and
profile/gain changes creating new comparability segments.

## Non-goals / things not to shortcut

No regex-guessing radio packets without replay; no OCR/screenshot-derived
power readings; no full-rate indefinitely retained raw stream; no
unbounded queues; no **automatic restart of retuning surveys** after
reboot; no in-place overwriting user-maintained antenna configuration;
no claiming a birdie is household RFI without corroborating evidence.

The agent may propose changes to defaults based on real radio tests, but
must document what was measured, what was inferred and any unverified
behavior. Keep source commits small and logical, update this plan as
milestones ship, and leave the operator's data untracked by Git.
