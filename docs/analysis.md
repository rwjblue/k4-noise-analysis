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

## Implemented: `k4na-analysis/4`

Version 4 adds paired main/sub comparisons (`analyze pairs`, below); other
outputs are unchanged from version 3. Version 3 stops counting a masked carrier as absent: a covering visit
where a raised local background would have hidden the carrier is
unobserved, so it leaves exposure, persistence and timing edges alone
(see `analyze carriers` and `analyze groups`). Version 2 splits comparability segments by delivered frame rate:
analysis segment ids are `<stored segment>@<n>fps` (shown as
`01234567@30fps`), so shards written before the writer keyed segments by
frame rate never compare 15/s and 30/s visits as one segment. See
[measurement.md](measurement.md) for why the rate is measured rather
than read back. Version 1 output is otherwise unchanged.

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
  visits whose coverage included the cluster's median frequency and
  where it could have been seen, so persistence = detections / exposure
  and "not seen" is distinguished from "not covered" and "masked". A
  carrier's level is the median over its detections of background plus
  excess; a covering visit without a detection is `masked` when that
  level stands less than 8 dB (the threshold) plus a 2 dB margin over
  the visit's local background at the carrier, i.e. when a floor rise
  would have hidden it. Masked visits are reported but excluded from
  exposure. Hourly detection and exposure histograms are in
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
- `analyze series` looks for families that are harmonics of one
  fundamental `f0` (lines at `n * f0`, no free offset), as a switching or
  clock source radiates: on several bands, or several combs on one.
  Candidates are each family's spacing divided by 1-4, refitted (least
  squares through the origin) to that family's lines; a family joins
  when all its members lie within the carrier tolerance of a harmonic
  and its spacing is 1-4 times `f0`; the fundamental is refitted over
  all members, and the largest fundamental explaining a set of families
  is kept. One family is never a series: at harmonic numbers in the
  hundreds a tiny change of `f0` shifts all its lines almost equally,
  like a free offset, so a single comb fits *some* `f0`. Each other
  family's chance of fitting by accident is `2 tol / f0` (offset) times
  `2 tol / (grid positions * f0)` (spacing); `false_alarm` is the
  candidate count times the product over all families but the least
  likely one, and must be at most 0.01. Carriers outside the series'
  families within half the tolerance of a harmonic are listed with the
  number expected by chance; they are not used in the fit. Each family
  joins at most one series.
- `analyze episodes` finds intervals when a band's floor rose over its
  quiet baseline. Each tile within a band, antenna and segment is one
  series of per-visit floors (tiles of one band can rise by different
  amounts). Its baseline is the 20th percentile of its floors, so a
  source on up to ~80% of the time leaves the quiet state visible. Its
  noise is `1.4826 * median|successive difference| / sqrt(2)`, which a
  source switching on and off barely moves (an interquartile spread
  would absorb the steps). A tile turns on at `max(3 dB, 5 * noise)`
  over baseline and off below half that; a pause longer than three
  revisit intervals ends an episode with an unobserved end. Tiles need
  8 visits. Tile episodes within one revisit interval of each other
  merge into a band episode, which reports the first and last elevated
  visit and the quiet visits bounding them (the onset lies between
  `start_after` and `start`), elevated visits, tiles involved, and the
  median and maximum rise. Per band, `floor_activity` gives the median
  revisit, noise and threshold, the elevated fraction of visits, and
  median on time, off time between episodes, and onset-to-onset period.
  Timing resolution is the revisit interval; an episode says the floor
  rose, not why (a switched device, propagation and a nearby wideband
  signal all qualify).
- `analyze groups` finds features that switch on and off together. A
  feature is a band floor with episodes, or a carrier with its presence
  over the visits that covered it (bridging one missed detection, so a
  carrier near threshold does not flicker into many episodes). Each
  timeline's on intervals are its episodes extended halfway to the
  bounding quiet observations; its edges are the observed onsets and
  ends. A carrier's masked visits, and any pause over three revisit
  intervals, are unobserved: they end an episode without an edge and
  are left out of agreement. Two features join when, over the time both
  observed (common span minus either's unobserved intervals), the phi
  coefficient of their on/off signals is at least 0.7 (chance-corrected,
  unlike overlap: two sources each on 80% of the time overlap mostly by
  chance) and at least 3 edges match one-to-one within the mean of their
  revisit intervals (one or two coincident edges occur by chance over a
  night). Features with fewer than 3 observed edges, such as always-on
  carriers, cannot join. Groups are connected components; each lists
  its members (with the comb family a carrier belongs to), the weakest
  and strongest joining phi, and the onsets and median onset interval of
  its anchor (a floor if it has one, else the member with most edges).
  A group is a hypothesis that one switched source or one propagation
  change drives its members, not an identification.

  Before version 3 a masked carrier counted as off, so a steady carrier
  appeared to switch off exactly when wideband noise switched on. That
  added spurious edges, and carriers masked by the same source grouped
  with each other on the floor's ends. Masking is judged from the
  carrier's median level, so a carrier much weaker during the masked
  time than usual is still assumed present-but-hidden, not absent.
  Floor timelines still treat a recording pause shorter than three
  revisits as quiet.

Operator marks are notes about what happened at the station: "furnace
started", "dehumidifier unplugged 02:10-02:40". `k4na mark --data DIR
[--at TIME | --ago 10m] [--until TIME | --for 30m] TEXT` appends one
to `DIR/annotations.jsonl` (see [storage.md](storage.md)); `k4na marks
--data DIR` lists them. Times need an explicit zone (`Z` or an offset
such as `-04:00`): a local wall-clock time read as UTC would place a
mark hours off. Without `--at` or `--ago` the mark is now. Marks are
written only by this command, never by the collector, so marking is safe
while a survey runs and needs no radio. Analysis output carries the
marks overlapping the analyzed span (`annotations`, with
`provenance.annotations_skipped` counting unreadable lines); `analyze
episodes` and `analyze groups` list them after their tables, and the
report draws them on every heatmap and floor plot and lists them in a
table. Nothing is inferred from a mark: it does not change any metric,
and a mark that coincides with an onset is evidence for the operator to
weigh, limited by the revisit cadence and by how well the mark's time
is known. A note written later with `--at` records when it was written
(`created_utc_ns`) as well.

Marks also come from `k4na watch --mark` (see
[collection.md](collection.md)) and from device logs: `k4na
import-marks --data DIR FILE.csv --time COL[,COL] ...` reads a CSV
export from a thermostat, energy monitor or smart plug. The header row is
the first one naming every requested column, so preamble lines are
skipped; several time columns (`Date,Time`) are joined with a space.
Times may carry an offset; otherwise `--tz` is required (an IANA zone
such as `America/New_York`, so daylight-saving changes are applied, or a
fixed offset). Wall-clock times that occur twice at the fall-back take
the earlier instant, and ones that do not exist at the spring-forward
move forward; both are counted in a warning. Times are ISO 8601, common
US forms (`10/28/2026 2:15 AM`) or a `--format` strftime pattern.

- `--text COL`: each row with non-empty text is a mark, an interval
  with `--end COL`.
- `--on COL` (repeatable): each run of rows whose value is above
  `--above` (default 0) is one interval mark, labeled `--label` or the
  column name. A row covers the time until the next row, or the median
  row spacing at the end and across gaps over three times it, so an
  interval is only as precise as the export's rows (a 5-minute runtime
  row with 180 s of runtime marks all 5 minutes). Unreadable values end
  a run. `--merge-gap` joins nearby runs and `--min-duration` drops
  short ones.

Imported marks have source `import:<file name>`; a mark with the same
start, end and text as an existing one is skipped, so re-importing an
export adds only new rows. `--dry-run` shows them without writing.

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
- operator marks drawn across the heatmap (a dashed line for a moment,
  a shaded band for an interval) and the floor plot, with the note in
  the hover tip;
- tables of operator marks, timing groups, floor episodes, harmonic
  series, families, carriers (by persistence) and band/hour floors.

Rows are passes, not uniform time, so the Y axis is labelled with the
first and last pass times; hover shows each cell's time and values. UTC
or browser-local time can be selected.

First observation (2026-10-27 data, 10 m, RX ANT IN1 at `PRE1`): 34
carriers on 34 consecutive grid lines spaced 35.087 kHz, 28.526-29.684
MHz, RMS 13 Hz from the grid, seen in every covering visit (only five
visits each). The source is unknown.

First night (2026-10-28 03:01-09:59 UTC, RX ANT IN1, 113 passes, 3.7 min
revisit): `analyze episodes` finds the same five episodes on 40, 30 and
20 m and four on 17 m (its first rise was 2.5 dB), 05:37-06:59 UTC,
each band's onsets within 40 s of the others: on ~8 min, off ~11 min,
period 18.5 min. Rises: 30 m ~19 dB, 40 m ~10 dB, 20 m ~6-8 dB, 17 m
~5 dB; none on 80, 15, 12 or 10 m. Over the same passes a comb near
15.97 kHz harmonics appears (the 20 m 31.93 kHz and 40 m 15.96 kHz
families; a 40 m line at 7.0098 MHz is harmonic 439, a 20 m line at
14.0196 MHz harmonic 878). A cycling switched load (an inverter
compressor is one candidate) fits, unconfirmed. Separately, a 40 m
9.995 kHz-spaced family was present ~03:30-08:55 UTC with sharp edges.
`analyze groups` joins the four floors, both combs and three 30 m
carriers 15.96 kHz apart into one group (anchored on the 40 m floor:
onsets 05:35, 05:54, 06:12, 06:27, 06:53); version 3 adds two more
31.93 kHz comb lines (33 members). The 40 m 9.995 kHz comb's lines lie
near multiples of 20 kHz (7000.05 ... 7280.08 and 10100.11-10140.09
kHz), so the 9.995 kHz fit is a sub-multiple. `analyze series` finds
the 40 m 15.96 kHz and 20 m 31.93 kHz combs are one harmonic series:
f0 = 15967.69 Hz, 40 m harmonics 439-457 (every one), 20 m 878-898
(every other one), mean offsets -4 and +2 Hz, chance 1.3e-4; the three
30 m carriers of the cycling group are harmonics 633-635 (0.7 expected
by chance). The 9.995 kHz comb fits no shared fundamental. That comb is masked while
the cycling source raises the floor. Version 2 put it with three 30 m
carriers in an 18-member second group whose "onsets" were the cycling
floor's ends; version 3 dissolves that group into a 3-member one (30 m
10100.11 and 10120.08 kHz, 40 m 7180.05 kHz) with irregular onsets,
03:30-08:26 UTC, unrelated to the cycle.

Caveats specific to v1: thresholds are the exploratory defaults above
and are not yet exposed as options; a chain of nearby CW or FT8
signals can merge into one wide-spread cluster (check `min_hz`/`max_hz`);
a sub-dwell burst shorter than half the dwell does not produce a p50
peak; and levels are on the uncalibrated K4 scale (native tenths / 10),
not dBm.

### Paired visits

`analyze pairs` compares two antennas measured over the same dwell on the
main and sub receivers ([paired.md](paired.md)). For each pair whose two
visits are complete, narrow peaks (at most `max_carrier_width_hz`) on the
two pans are matched one to one within `carrier_tolerance_hz` (or 1.5
bins). The pair's `d` is the median over matched carriers of sub minus
main excess over local background; it needs at least
`pair_min_carriers` (3) matches. Floors are compared the same way.

Per band and antenna pair, `d0` and `d1` are the median `d` with the
profile's first antenna on the main receiver and with the antennas
swapped. The antenna difference (second minus first; positive favors the
second) is `(d0 - d1) / 2` and the receiver difference (sub minus main)
is `(d0 + d1) / 2`. Flags:

- `one_orientation`: no swap, so antenna and receiver are confounded and
  no antenna difference is given (the per-orientation `d` still is).
- `receiver_difference`: the receivers differ by more than
  `pair_receiver_tolerance` (3 dB) in signal over background, so one may
  be limited by its own noise or set differently.
- `settings_differ`: some pair had unequal verified receiver settings
  (listed per pair in the CSV).
- `few_carriers`: no pair had enough matched carriers.

Carriers seen on only one pan are counted (`carriers_main_only`,
`carriers_sub_only`) but not compared: an antenna that hears more
carriers is better in a way the matched median does not show. The floor
decomposition is context, not a ranking: a quieter floor can come from
less gain as easily as from less noise. An `--antenna` filter keeps whole
pairs here, while other views still see only that antenna's visits.
Other views include pan B visits under their own antenna and segment;
carriers from both receivers on one antenna share a carrier row.

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
