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
"absent." The [default operating cadence](operations.md) is roughly
5 seconds per frequency window every 5–6 minutes, or about a **1.7%
observation duty cycle** per frequency. This is excellent for measuring
persistent carriers and slow time-of-day changes, but may miss short
intermittent emissions entirely. Quantify observed exposure and
confidence, and if necessary configure a focused monitoring run over
only a few suspect frequency ranges. The temporal percentiles within
a five-second visit must not be interpreted as percentiles of all 24
hours of wall-clock RF activity.

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
