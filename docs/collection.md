# Acquisition and survey contract

Status: proposed; details need live K4 validation before claiming calibrated
measurements or fully unattended active survey.

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

## Operator-initiated survey and minimal radio controls

The original design contemplated continuously watching for other CAT clients,
pausing on external VFO changes and conditionally restoring the original
receiver settings. **That is superseded** by [manual web Start/Stop and
cadence](operations.md). This tool is for an operator-controlled receive
survey, not an exclusive K4 CAT lease manager.

- **Passive** always remains non-invasive: connect :9201, optionally read
  CAT metadata, and never send setters.
- **Survey** starts only after intentional consent: CLI `--allow-retune`
  or authenticated web **Start**, plus `collection.allow_retune = true`.
  The collector is stopped on server startup and requires an operator
  Start even after a restart.
- **Stop** cancels planned retunes immediately, flushes a partial visit
  and ends the session. Leave the radio at its last frequency and do
  **not** restore startup VFO/mode/pan settings. The operator may then
  use QK4 normally.
- The collector retains a strict typed whitelist for receive-frequency
  and pan control; it can never key or transmit. Before each retune,
  perform a fresh TX-state check; transmitting, stale or unknown state
  vetoes frequency changes. This is a best-effort safeguard, not a
  hardware interlock or atomic reservation.
- Do **not** poll for operator/QK4 activity every 500 ms or demand
  complex external-state arbitration. The operator is responsible for
  stopping before use. Concurrent QK4 tuning may race if the operator
  forgets. Unexpected actual frame geometry/readbacks still invalidate
  measurements, create a gap and/or require safe replanning.
- Every retune still requires readback and observed PAN header coverage.
  Failures must never produce mislabelled tiles or missing-as-quiet data.
  Only the simplest relevant state is queried; changing antenna routing
  and transmit settings remains out of scope.
- TCP disconnects, unsupported packets, low disk, invalid state and
  crashes must leave the collector bounded and non-transmitting. The
  web control page stays available while the collector is Stopped or
  in Error, unless the hosting process itself has failed.

The current strict Rust TOML schema still contains legacy
`pause_on_external_change` and `manual_resume_on_conflict` fields.
The implementation agent should update that schema, its tests and the
example when implementing this superseding design. See operations.md
for the web API, LAN authentication and service lifecycle contracts.

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

Start with **5 seconds measured dwell**, **700 ms settle**, and a
configurable **five-minute minimum start-to-start cycle period**.
The current 96 kHz, 2,048-bin, 80%-usable planner has 50 windows across
the default eight bands, so 50 * 5.7 = 285 seconds before overhead.
A practical sweep is approximately 5–6 minutes. Each frame must be
captured after tuning has settled and actual PAN coverage is verified.
Group tiles by band to avoid gratuitous filter switches and visit all
bands on every complete cycle; never skip tiles to satisfy a target
period. If a cycle finishes early, wait until the configured minimum
interval; if late, run the next cycle without overlap. Report actual
cycle time and observed revisit/coverage gaps, not a guaranteed
five-minute cadence. Some rates/spans demand far more tiles.
See [operations](operations.md) for the calculation and future focused
monitoring alternative.

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
k4na serve --config config.toml  # default: stopped; web Start required
k4na inspect --data ./data
k4na replay --input fixture.bin
~~~

The commands above are a target interface, not implemented functionality.
Passive capture must remain useful even if safe automated retuning turns out
to be impossible on a particular K4 firmware or operating configuration.

## Quality gates for a believable baseline

- Synthetic stream tests: fragmented TCP, concatenated frames, bad sync,
  CRC rejection/resync, version mismatch, sequence wrap, interleaved A/B/Y/Z,
  huge garbage, reconnection and bounded buffering.
- Replay the author's R39 captures from HaLo, with appropriate license/
  provenance, to match the independently documented centers, rates and bins.
- Simulated CAT session: TX active before retune, an operator concurrently
  retunes, CAT readback differs, fixed pan refuses to follow, network
  drops, receiver disappears, or writer becomes read-only/full. In each
  case no TX command is sent, no bad-coverage row is accepted, and the
  run stays bounded and explains missing observations. No elaborate
  operator-presence detector is required.
- Live 5-minute passive and limited 80/40/20 survey acceptance against
  a physical K4 before enabling unattended 24-hour mode.
- Observe manual vs AutoRef levels, display span vs packet sample rate and
  requested retune vs packet center. Document actual behavior before
  claiming levels are comparable across bands/days.
