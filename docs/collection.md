# Acquisition and survey contract

Status: implemented and tested against the bundled R39 fixtures and a
simulated K4 (see [Implemented behavior](#implemented-behavior-v01)).
Not yet validated on a physical K4: details need live K4 validation
before claiming calibrated measurements or fully unattended active survey.

## Inputs and state

A K4 LAN spectrum frame includes main/sub pan ID (A/B), sequence, frame
header, center Hz, sample rate Hz, 2,048 native signed tenths-of-dB bins,
and the radio's header noise field. The decoder validates sync, version,
header sum, CRC, numeric fields and resource limits. Do not use mini-pans
(Y/Z) for full-band coverage in v1. Preserve wire packet for sampled frames
and integer levels for aggregations.

Frequency of an i-th zero-indexed bin, provisionally:
center_hz + (i - bin_count/2) * sample_rate_hz/bin_count.
An R39 fixture supports ascending bins, but exact sub-bin edge convention
is not established. Store the coordinate convention version and raw fields.
Usable interior excludes configurable edges (default 10% on each side).
Do NOT assume that requested #SPN, radio dial/VFO or screenshot width equals
actual sample_rate_hz.

Each sample/visit records:
- UTC capture timestamps, monotonic durations and frame sequence/ordering;
- band/plan tile, actual center/rate/edges and observed subrange;
- pan receiver A or B, configured antenna ID, RX routing readback when
  available, antenna revision/position, LoG transformer/feedline notes;
- firmware identifier, connection/protocol decoder version, CAT settings
  (frequencies, modes, transmit status, split/diversity, preamp, attenuator,
  AGC, applicable pan reference/auto-ref, averaging, noise blanker);
- declared units, calibration status and readback unknowns;
- accepted/rejected frame counts, discontinuities, stale/readback mismatches,
  settling losses and buffer/disk backpressure.

If some state cannot be read, explicitly mark it unknown and limit
comparability; do not synthesize plausible values.

## Initial safety model

The **passive** subcommand opens :9201 and never mutates radio state.
The **survey** subcommand requires BOTH an opt-in in TOML and
an explicit CLI --allow-retune. A local operator must dedicate the selected
receiver for the run. Do not enable a background survey automatically.

Use a typed CAT whitelist with explicit reviewed setters (for example,
BN/FA, track/fixed pan mode, span and optionally manual AutoRef). GET-only
requests and a small K4 meta/reporting initialization subset are permitted.
Never provide an arbitrary CAT-script field in TOML. No commands for
PTT/TX, test transmit, tune carrier, CW or audio.

Preflight:
1. Probe CAT and :9201 independently; record firmware, settings and mode.
   Fail on unsupported hardware/protocol, missing spectrum, no safe snapshot,
   or unknown transmit state.
2. Refuse active survey when transmitting, keying, split/diversity/receive
   bindings are incompatible, or receiver ownership is uncertain. Detect
   concurrent CAT changes; survey is not a cooperative tuner for QK4.
3. Snapshot all settings the program intends to modify. Prefer changing
   only the minimal state needed for coverage. Do not modify the TX antenna
   or RX antenna routing in the first milestone; require the operator to
   select the correct protected RX input manually.
4. Check free space and estimated upper bound. Require capacity for at
   least one intended retention window under the configured quota.
5. Print the planned bands/tiles, expected maximum cycle time and disk
   budget. Dry-run performs no CAT setters.

Runtime:
- Observe TX state and external VFO/state changes via CAT and relevant
  unsolicited updates; poll/fresh-read before each retune as needed.
  Unknown/stale TX state -> stop issuing CAT writes, pause and log a gap.
- Each retune must be acknowledged/read back; verify subsequent PAN
  headers cover the requested tile before accepting samples. Changes to
  observed center/rate/gain mid-visit split or invalidate the visit.
- Pause when the operator transmits or a second client changes frequency,
  mode, pan or antenna selection. Do not fight a human/QK4 for control.
  Resume only under documented explicit safe criteria; manual resume is
  the conservative initial behavior.
- On shutdown/signal/crash, flush durable data, stop writes and restore
  the last collector-owned state **only if** current state still matches
  the value the collector last set. Otherwise preserve the newer state
  and report the skipped restoration. Recovery after abrupt termination
  must not auto-restore stale values on restart.
- No large unbounded channels; apply bounded backpressure/drop policy to
  optional raw samples only. Still record frame losses/gaps and refuse to
  claim complete coverage when writer/decoder cannot keep up.

## Band planner

TOML specifies inclusive lower and exclusive upper RF frequency, band
identifier and optional weight/priority for each interval. Default band
ranges are 80 (3.5-4.0 MHz), 40 (7.0-7.3), 30 (10.1-10.15),
20 (14.0-14.35), 17 (18.068-18.168), 15 (21.0-21.45),
12 (24.89-24.99), 10 (28.0-29.7); these are proposed measurement
bounds and are not transmit permissions. Any band can be omitted or
changed. Do not require occupancy of only CW portions.

Planner must discover/validate usable *observed* spectrum width, not
assume the display span: usable = sample_rate_hz * usable_fraction, default
0.80. Generate just enough overlapping tile centers to cover every
configured interval, including both ends, subject to radio-supported
centers/modes; reject unsupported or unreachable tiles and record them.
On change of observed rate, rebuild the remaining plan and log why.
Each accepted visit records its requested and actual interval.

Start with sample survey settings of 3 seconds dwell plus 700 ms for
tuning/settling. Actual acceptance begins only after a verified fresh PAN
frame following the CAT retune. The values are tunable, not hardware claims.
Use band-fair round-robin scheduling so the long 10m interval does not
indefinitely starve small bands. Record the timestamp of every visit and
actual revisit interval. Advertise the actual measured coverage fraction
per band per hour and the largest revisit gaps: a full-band scan cannot
detect every intermittent emission between visits.

Optional per-band weighting and higher-resolution followup windows
are useful later, but not required for a functioning first full-band survey.
Do not increase total sample rate until the disk impact is measured.

## Stream processing per visit

For each validated PAN packet:
1. Decode/check it, timestamp using UTC and monotonic clocks, and confirm
   correct pan ID, packet geometry, complete expected coverage and stable
   readback segment.
2. Update bounded per-bin in-memory visit statistics using all accepted
   ~15 Hz frames; do not routinely write every frame.
3. Retain a configurable sparse subset of **original, checksum-valid wire
   frames** (initial target <= one/second per receiver) for offline decoder
   replay and unforeseen detectors. Tag sampling policy and dropped counts;
   randomness, if used, must be seeded/reproducible.
4. Finalize a visit: exact accepted count, timing, coverage quality and
   per-bin min/p10/median/p90/max in native tenth-dB units. For a short
   dwell, exact quantiles via bounded vectors are acceptable; avoid averages
   of dB values masquerading as averaged RF power. Future summary formats
   can include power-domain aggregates with explicit semantics.
5. Atomically persist summary before scheduling next visit. Raw sampling
   is expendable under pressure, summary metadata is not. A partial visit
   is marked partial rather than misreported as a clean measurement.

Do not run birdie classification in the collector. A static classifier
cannot reliably distinguish an authorized continuous carrier from an
oscillator spur; preserving the complete temporal distribution allows
later experiments and controlled on/off event correlation.

## Clock and alignment

Every frame's timestamp is a collector arrival time, not a K4 RF acquisition
clock. Record UTC nanoseconds plus monotonic elapsed time since process/
session start; detect clock jumps and clock-source changes. Do not pair
A/B frames solely by arrival order or matching sequence byte. For later
near/far comparisons align on overlapping actual RF-bin windows and closest
timestamps within a declared tolerance, with uncertainty/missing-pair flags.

## Initial CLI behavior

~~~text
k4na check --config config.toml
k4na estimate --config config.toml --duration 24h
k4na capture --config config.toml --duration 5m
k4na survey --config config.toml --duration 24h --dry-run
k4na survey --config config.toml --duration 24h --allow-retune
k4na inspect --data ./data
k4na replay --input fixture.bin
~~~

These commands are implemented (`capture` is the passive subcommand), plus
`k4na simulate` for dry runs against a clearly labeled simulated K4. Pass
`--json` before the subcommand for machine-readable output. Exit codes:
0 success; 1 runtime failure (storage, CAT loss, no usable spectrum);
2 refused before collection (config, consent, preflight, storage
admission); 3 stopped for operator activity (transmit or external change).

Passive capture must remain useful even if safe automated retuning turns out
to be impossible on a particular K4 firmware or operating configuration.

## Implemented behavior (v0.1)

Passive (`capture`):
- Reads :9201 only. With `radio.passive_cat_readback`, a separate task
  polls GET-only state on a read-only CAT link that cannot encode setters.
  CAT loss is recorded and capture continues.
- Visits are fixed `passive_visit_ms` windows of stable observed geometry
  on the configured pan. A geometry change (operator retune/span), stream
  loss, host clock jump or stop closes the visit early as `partial`.
  Band attribution comes only from the observed center; passive capture
  never claims band coverage.

Survey (`survey --allow-retune`, with `collection.allow_retune = true`):
- Setters are limited to `BN`, `FA` (rounded to 100 Hz), `#SPN` and
  `#FXT`. Mode, antennas, gain and display reference are never set.
- Preflight (GET-only) refuses unless `TQ` reads 0; `LK`, `FT`, `DV`,
  `SB`, `RT`, `XT` read 0; `#FRZ` is not 1; FA/BN are readable; the pan
  is not fixed (`#FXT1`) unless `collection.set_pan_track_mode = true`
  lets the survey switch it to track; and, when
  `require_firmware_readback`, `RV` answered.
- Before each change the interlock (TQ plus FA, FB, BN, MD, FT, DV, SB,
  LK, RT, XT, #SPN, #FXT, #FRZ) must equal the baseline. After setters,
  the full state is read back. The setters must be confirmed, nothing they
  should not affect may move, and the readback becomes the new baseline.
  It is polled every `collection.cat_poll_ms` (default 500 ms) during a
  dwell.
- Any transmit, unreadable TQ, or difference from the baseline is operator
  activity. The survey **stops** with no further setters and leaves the
  radio alone. Resuming means starting a new run (manual resume).
- Lost CAT means unknown radio state: no retune and no restore.
- The plan is built from the packet rate observed after a probe tune and
  rebuilt if the rate changes. The pan-center offset from FA is learned
  from headers, with up to `collection.max_tile_corrections` (default 2)
  correction retunes per tile. A tile whose header
  coverage still misses its target becomes a `coverage_unverified` event,
  not a visit. Settle frames are discarded and counted.
- Clean ends (duration, signal, storage suspension, no usable spectrum)
  restore the original BN/FA/#SPN/#FXT only if the radio still matches the
  collector's last state. A mode change from band-stack recall is
  reported, not corrected. Nothing is restored on a later restart.

Both modes: bin-identical frames across a visit (frozen pan, stuck
stream) produce an `aborted` visit. Frames from other pans are counted,
not stored. Disconnects and stalls become `gap` events with intervals.

### Unverified on hardware

The simulator encodes assumptions that must be checked on a physical K4
before unattended use. Do not treat them as K4 behavior:
- `FA` readback equals the commanded 100 Hz-aligned value exactly.
- `BN` recalls the band stack, and `FB`/`MD` changes after `BN` are the
  only side effects.
- `#FXT0` makes the pan follow FA with a stable center offset.
- The `#SPN` to packet-rate mapping (simulator stand-in: smallest of
  24/48/96/192/384 kHz at least twice the span; only `#SPN40000` to 96 kHz
  is supported by the R39 fixtures).
- `TQ`, `#FRZ` and `RV` are answered on the operator's firmware.
- The 700 ms settle discards every pre-retune frame.
Run a live 5-minute passive capture and a short 80/40/20 survey first.

## Quality gates for a believable baseline

- Synthetic stream tests: fragmented TCP, concatenated frames, bad sync,
  CRC rejection/resync, version mismatch, sequence wrap, interleaved A/B/Y/Z,
  huge garbage, reconnection and bounded buffering.
- Replay the author's R39 captures from HaLo, with appropriate license/
  provenance, to match the independently documented centers, rates and bins.
- Simulated CAT session: TX begins while surveying; other user retunes;
  CAT readback differs; fixed pan refuses to follow; network drops;
  receiver disappears; writer becomes read-only/full. In each case no
  transmit is sent, no bad-coverage row is accepted, and the run stays
  bounded and tells the operator why.
- Live 5-minute passive and limited 80/40/20 survey acceptance against
  a physical K4 before enabling unattended 24-hour mode.
- Observe manual vs AutoRef levels, display span vs packet sample rate and
  requested retune vs packet center. Document actual behavior before
  claiming levels are comparable across bands/days.
