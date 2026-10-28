# Architecture: K4 noise surveying

Status: collector implemented as designed below and tested offline
(fixtures and simulator); not yet validated on a physical K4 (2026-10-27).

## Goal and scope

Build a local-first, receive-only spectrum survey system for an Elecraft K4D.
The immediate objective is a repeatable 24+ hour baseline of RF background,
persistent carriers, intermittent interference, and time-of-day variation
using one loop-on-ground (LoG) receiving antenna. Later, compare the near-house
and remote LoGs with simultaneous main/sub-receiver acquisition.

This is a **spectrum measurement system**, not an I/Q recorder, demodulator,
or automatic proof that a peak is local RFI. Analysis must distinguish an
observed feature from a claimed source.

## Architectural boundaries

~~~text
K4 local LAN                 collector (Rust, 2024 edition)
  CAT :9200  --------->  K4 controller and guarded survey scheduler
  PAN :9201  --------->  bounded framing/CRC decoder
                             |       |
                             |       +--> limited original-frame samples
                             v
                         per-visit temporal aggregation
                             |
                             v
                    versioned on-disk measurement shards
                             |
                  +----------+----------+
                  v                     v
          offline analysis       reports / UI (later)
~~~

- **Collector** owns capture, CAT/state validation, scheduling, clock stamps,
  gap accounting, in-memory statistics, storage and quota enforcement. It does
  not run a classifier or require a GUI.
- **Analyzer (later)** is a pure offline reader of recorded data: estimated
  frequency-dependent baseline, persistence, comb detection, change points,
  coverage maps and receiver-pair comparisons. Its results are derived and
  versioned; it never rewrites original measurement rows.
- **Renderer (later)** consumes analysis output and/or immutable measurement
  shards. Start with static HTML plus exportable CSV/Parquet; do not couple
  acquisition to a particular graphing library, frontend, or web server.
- **Protocol** has an incremental, bounded decoder for the verified local LAN
  format and a trait/adapter to accept other transport formats in the future.
  A separate secure 9204 implementation is explicitly out of scope for v1.

## Existing reliable inputs

The author's HaLo work contains LAN protocol decoding, a fixture taken from
a physical K4 R39, a capture tool, and documented limits on interpretation:
https://github.com/ham2k/halo/pull/3
https://github.com/ham2k/halo/pull/4

The LAN stream has observed 4,162-byte version-2 packets: 64-byte header,
2,048 signed big-endian i16 bins (tenths of a dB), 2-byte CRC. Pan IDs A/B
are main/sub; Y/Z are mini-pans. Each valid packet carries center frequency
and sample rate. Normal observations were about 15 frames/second/pan.
The radio's nominal display span, tuned VFO, and actual header center/rate
can differ, especially in fixed display mode. Preserve raw header and
uninterpreted fields for future protocol corrections.

**Do not call the decoded values calibrated dBm.** The relation between
panadapter data, the radio's reference/auto-reference controls, AGC/gain,
and calibrated RF power remains to be verified. Record the raw i16 units,
radio settings, and provenance, and label output as K4 pan dB units until
calibration has been established by an actual controlled measurement.
Treat the decoded noise field as header metadata, NOT ground truth noise.

The Elecraft K4 Programmer's Reference (D12) documents CAT/display state:
https://ftp.elecraft.com/K4/Manuals%20Downloads/K4ProgrammersReferencerev.D12.html

Elecraft support also recommended port 9201 for LAN pan-only acquisition.
Port 9204 is useful for remote mixed streams but introduces TLS-PSK and audio
multiplexing that do not help the first collector.

## Operating modes

**Passive:** connect only to :9201, do not send CAT setters or retune. Optionally
connect to :9200 for GET-only metadata, but failure to read state must not
block raw capture. Record exactly the changing spectral windows actually
observed; never infer that passive recording covered an entire band.

**Survey:** explicitly enabled CAT-driven frequency and pan state setup on
:9200 plus acquisition on :9201. Iterate configured band intervals until
the requested duration or stop condition. No transmit/keying commands.
This reserves the selected receiver: do not quietly change a radio being
used for QSOs, split, diversity, or a separate remote session. Survey is
fail-closed on loss of transmit/operator interlock, unexpected VFO changes,
or unverified frequency coverage. See collection.md.

Survey mode can also compare antennas sequentially: a profile lists
several registry antennas, and with an explicit station opt-in the
survey selects each main-RX input with `AR` (verified by readback) and
measures each band on each antenna in turn. The TX antenna is never set.
Every visit records its antenna id, revision and observed port.

A later **paired** mode records simultaneously observed A/B sweeps from
separately configured antennas, keyed by exact frequency coverage and
timestamps, without assuming their nominal scales are calibrated or
their feedlines and transformers identical. The physical K4D can already
emit A and B packets on the local LAN; no 9204 dependency is required.

## Rust module boundaries (workspace)

- crates/k4na-protocol: bounded K4 LAN framing/CRC/parser and packet types;
  zero radio control, no storage dependencies.
- crates/k4na-core: domain types (antenna identity, observed spectrum,
  visit, quality flags, timestamps), survey planning and online summaries.
- crates/k4na-store: versioned SQLite shards, encoding, retention, read API.
- crates/k4na-collector: Tokio network sessions, CAT GET/control whitelist,
  receiver measurement codecs, interlocks, scheduler, the survey recovery
  journal and restoration engine, lifecycle and metrics.
- crates/k4na-cli: operator commands (check, estimate, capture, probe,
  survey, recovery, inspect, analyze, report, replay, simulate); one
  executable named k4na.
- crates/k4na-sim: a simulated K4 (CAT and spectrum servers over one
  fault-injecting radio model) for tests and dry runs; it logs every
  received command so tests can assert that nothing forbidden was sent.
- crates/k4na-analysis: read-only derived analysis (local background,
  band/hour summaries, recurring carriers) over the store's read API;
  depends on k4na-core and k4na-store only, never on the collector.
- crates/k4na-report: a self-contained static HTML report (heatmaps,
  floors, carrier/family/band-hour tables) rendered from k4na-analysis;
  separate from the writer and needing no network to view.

Begin with a single workspace and binary; organize libraries when doing so
improves testing. Avoid a network service, shared database server, or plugin
framework until a real use requires one.

Suggested crates, selected at implementation time: tokio, clap, serde/toml,
tracing, rusqlite with bundled SQLite, zstd and an appropriate time library.
Use edition = "2024", CI against current stable Rust, a checked-in Cargo.lock
for the application, formatting, clippy, unit tests and offline fixture tests.
Do not pin fictional future dependency versions.

## Non-negotiable invariants

1. The collector **never sends PTT, TUNE, CW, audio or arbitrary operator
   commands**. Survey CAT writes come from a small reviewed typed whitelist.
   The default mode does not change the radio.
2. Every stored sample has its actual, observed pan ID, center, sample rate,
   bin count, acquisition time and receiver/antenna identity (or explicit
   unknown). Display span is never substituted for actual spectrum width.
3. The frequency coordinate is inferred from the observed header and bin
   index, with mapping version/uncertainty recorded. Never shift FFT halves
   or assume an A frame follows VFO A without validation.
4. Missing coverage and skipped frames are recorded as gaps, never as
   readings of zero or very low noise.
5. Bounded channels, bounded decoder buffers and explicit byte-budget policy
   prevent disk/queue runaway. Persistent failure to write data pauses/stops
   safely, with an error and a gap, not silent loss.
6. Original raw samples and immutable per-visit summaries remain distinct
   from downstream estimates, detectors and labels.
7. Clock readings come from the collector host (UTC plus monotonic elapsed
   time/sequence). Do not invent K4 hardware capture timestamps.
8. Changing antenna, receiver routing, gain, averaging, frequency mapping,
   or stream format starts a new comparability segment. No cross-segment
   noise ranking without explicit normalization/qualification.
9. Collector exits and crashes must be safe: stop CAT writes first, persist
   termination reason, and conditionally restore only settings still known
   to be collector-owned. Never overwrite a newer human change. Every
   change is journaled durably before it is sent, so a crash leaves a
   record that `k4na recovery` can restore from ([recovery.md](recovery.md)).

## Data lifecycle / user experience

Typical path: validate TOML and radio connectivity -> passive short run ->
read/replay fixture and validate framing -> explicit dry-run survey plan ->
overnight survey -> inspect coverage, gaps, size and health -> analyze later.
All long runs are foreground/daemonized by the operator, not by this design PR.

No precise geolocation, email, network password, or callsign is necessary in
measurement shards. Store a user-controlled antenna label and optional
site notes; keep IP/host only in local config/logs and redact secrets.

## Primary risks and responses

- **Survey is sampling, not continuous simultaneous monitoring of all HF.**
  Short bursts between visits can be missed; record actual dwell and revisit
  coverage and make selected frequency regions higher priority if needed.
- **Operating interference:** survey requires exclusive main-RX consent,
  honors TX state and external changes, and pauses rather than racing QK4.
- **Uncalibrated spectrum:** preserve native data and radio state for later
  bench calibration; initially report relative differences only.
- **Clock/retune transients:** reject settle frames and mark every incomplete
  visit, so transient retuning cannot masquerade as a signal.
- **Raw 15 fps stream:** do not write every packet by default; aggregate
  incoming frames, retain sparse samples within a disk quota, and rotate shards.
- **Firmware changes:** keep provenance and replay tests from actual captures;
  refuse unknown formats instead of silently guessing.

See collection.md, storage.md, analysis.md, implementation-plan.md and
../config/example.toml for the executable contract.
