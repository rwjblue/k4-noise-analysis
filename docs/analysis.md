# Analysis roadmap and interpretation limits

Analysis is deliberately downstream of collection; this document defines
what the initial data must enable, NOT algorithms required in the v1 writer.

## Questions to answer

1. For each band/frequency and time of day, what is the typical measured
   panadapter background? What were the quietest, noisiest and most variable
   observation periods?
2. Which spectral peaks recur at nearly fixed frequencies, recur at regular
   spacing (comb/clock pattern), or turn on/off in temporal clusters?
3. Can we correlate interference changes with logged heat-pump, inverter,
   EV charging, lighting or other household state? This requires
   operator annotations or later independent telemetry; no inference from
   signal shape alone proves the source.
4. After installing the remote LoG, do near/far comparisons improve
   **signal-to-noise margin** and/or reduce unwanted carriers, accounting
   for gain differences, feedline loss and the actual receiver settings?

## Suggested analytical pipeline

- Open versioned shards **read-only**. Validate units and provenance,
  segment by receiver/antenna/hardware settings and confirm coverage.
- Assemble per-frequency time tiles from the actual bin coordinates,
  not the requested CAT frequency or a nominal display span.
  Avoid interpolation across unobserved gaps and track quantization/
  frequency-resolution uncertainty (sample_rate_hz/bin_count).
- Estimate local background robustly. Candidate first pass: rolling
  10th-25th spatial percentile per 5-10 kHz neighborhood over
  per-visit median data, masking large peaks; report sensitivity to
  window and percentile choices. Retain the native, uncalibrated label.
- Detect local spectral peaks as excess over local background (for
  example an exploratory +8 dB threshold). Track frequency width,
  drift, revisit persistence, intensity and temporal variability.
  Near a band edge or on crowded CW/FT8 frequencies, a baseline
  estimator may be biased; mark confidence/coverage, don't force a label.
- Look for uniformly spaced families and synchronized state changes
  to suggest switching/clock noise. Stationary narrow carriers are
  only **candidate birdies**: WWV, broadcast carriers and other real
  transmissions can also be stationary. Include unknown as a first-class
  classification.
- Present heatmap: RF frequency on X, local time/UTC on Y, color as
  robust excess over local baseline; overlay observed/missing coverage.
  Also provide carrier table, run health and per-band hourly summaries.
- For two receivers, align on same *actual* RF region and near-simultaneous
  collector timestamps, match gain/reference segments and ideally perform
  a co-located antenna calibration or receiver-input swap. Compare wanted
  signal level above background, not raw background reduction alone.
  Never subtract spectra as if both bins were independent calibrated dBm.

## Implemented: `k4na-analysis/1`

The `k4na-analysis` crate and `k4na analyze` read a data root through
the read-only store reader; they need no radio and can run while a
collector writes. Every output carries a `provenance` block (in JSON)
naming the algorithm version, parameters, filter, summary shards,
sessions, profiles, source units and calibration label, and counts of
visits analyzed and skipped. Nothing is written back to the data root.

Per **complete** visit (partial and aborted visits are skipped, never
treated as quiet):

1. Bins are mapped to RF from the stored header geometry
   (`center_hz`, `sample_rate_hz`, `bin_count`), keeping only bins whose
   centre lies in the observed coverage clipped to the planned tile, so
   overlapping tiles are not double counted.
2. Local background: the 20th percentile of the per-bin median (p50)
   over a 5 kHz neighbourhood, evaluated every 1 kHz and linearly
   interpolated. It ignores peaks occupying under ~80% of the window;
   crowded segments (busy CW/FT8 windows, wideband noise) bias it high.
3. Visit metrics: `floor` (median background), `occupancy` (fraction of
   bins with p50 at least 8 dB above background) and `variability`
   (median p90 - p10, the within-dwell spread).
4. Peaks: runs of bins with p50 at least 8 dB above background, merged
   across single-bin dips. A p50 detection means the peak was present
   for at least half the dwell; p10 above threshold marks it steady.

Across visits:

- `analyze bands` groups visits by band, antenna, comparability segment
  and UTC hour: visit count, observed Hz, median/p10/p90/min/max of the
  per-visit floors, median occupancy and variability, and the receiver
  settings. Text output also names the quietest, noisiest and most
  variable hour per band/antenna/segment. Floors are only comparable
  within a segment (same verified receiver settings).
- `analyze carriers` clusters peaks at most 300 Hz wide whose
  frequencies chain within 100 Hz (or 1.5 bins) on one band and
  antenna, keeping clusters seen in at least 3 visits. Exposure counts
  visits whose coverage included the cluster's median frequency, so
  persistence = detections / exposure and "not seen" is distinguished
  from "not covered". Hourly detection and exposure histograms are in
  JSON. The class is temporal only: `stationary` (seen in >= 90% of at
  least 6 covering visits, frequency spread within two tolerances),
  `recurring` (>= 25%), `occasional`, or `insufficient_exposure`.
- `analyze families` looks for carriers on one band and antenna lying on
  a common grid `offset + k * spacing` (2-250 kHz), within the carrier
  tolerance. Candidate spacings come from carrier differences divided by
  1-4; each grid is refined by least squares. A family needs at least
  4 members, distinct grid lines, at least half the lines between its
  first and last member occupied, and a small chance of arising from
  unrelated carriers: two members define a grid, so the binomial tail
  of the remaining members landing on it, times the number of spacings
  tried (a Bonferroni bound reported as `false_alarm`), must be at most
  0.01. Each carrier joins at most one family. A family is a pattern
  (harmonics of one switching or clock source are a common cause), not
  a source identification.

`k4na report --data DIR --out report.html` (crate `k4na-report`,
`k4na-report/1`) writes one self-contained HTML file: no network
resources, data embedded as JSON with the analysis provenance. Per band
and antenna, in frequency order:

- a heatmap with RF on X and passes on Y (a pass is a run of
  consecutive tile visits; it ends after a pause over a minute, on a new
  session, or when a tile repeats). Visits are binned at 250 Hz and
  merged to at most 1600 columns, keeping the *maximum* excess per
  column so a one-bin carrier stays visible, and the mean level. Color
  is excess over local background (0-30 dB) or the uncalibrated level
  (per-band 2nd-99.5th percentile); gray is not observed, and a gray row
  marks an outage longer than twice the typical pass interval;
- the median visit floor per pass, and a warning when the grid mixes
  receiver-settings segments;
- tables of families, carriers (by persistence) and band/hour floors.

Rows are passes, not uniform time, so the Y axis is labelled with the
first and last pass times; hover shows each cell's time and values. UTC
or browser-local time can be selected.

First observation (2026-10-27 data, 10 m, RX ANT IN1 at `PRE1`): 34
carriers on 34 consecutive grid lines spaced 35.087 kHz, 28.526-29.684
MHz, RMS 13 Hz from the grid, seen in every covering visit (only five
visits each). The source is unknown.

Caveats specific to v1: thresholds are the exploratory defaults above
and are not yet exposed as options; a chain of nearby CW or FT8
signals can merge into one wide-spread cluster (check `min_hz`/`max_hz`);
a sub-dwell burst shorter than half the dwell does not produce a p50
peak; and levels are on the uncalibrated K4 scale (native tenths / 10),
not dBm.

## Why the capture design preserves five percentiles

A median distinguishes persistent structure from short emissions;
p90 and max retain evidence of intermittent higher noise; p10/min
help determine quiet conditions and baseline bias. Per-visit raw
samples allow algorithm debugging but cannot guarantee all sub-second
events were retained. Frame counts and duty cycle matter when
interpreting whether a tone was persistent.

Thresholds are hypotheses, not measurement standards. Different
antenna positions can change reception of legitimate RF and local RFI.
A complete 80-10m scan is not simultaneous; "not observed" is NOT
"absent." A radio sampling each tile for 3 seconds every few minutes
will miss some brief intermittent noise. Quantify observed exposure
and, if indicated, schedule a focused continuous monitor survey on
suspect intervals.

## Export/report contracts (later milestones)

Analysis outputs derived results with their algorithm/version,
input shard IDs, calibration/units, filters and quality flags.
Start with JSON/CSV exports of band/hour summaries, candidate
birdies and visited/gap coverage; optional Parquet for interactive
exploration. A static HTML report can render line plots, carrier
scatterplots, time/frequency heatmaps and side-by-side antenna
comparisons. No GUI or cloud sync required for the initial collector.

Keep original measurement data immutable and distinguish:
- observed packet/visit;
- statistically estimated baseline;
- detected spectral feature;
- operator-provided equipment-state annotation;
- hypothesized local interference source;
- confirmed source via controlled on/off or physical tests.

Do not imply that a drop in received noise alone proves improved
receiver performance; compare SNR or usable signal margins.
