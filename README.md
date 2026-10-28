# k4-noise-analysis

The panadapter shows you the noise right now. It doesn't tell you whether
that comb on 40 meters was there all night or started when something in
the house kicked on.

`k4na` records spectrum from an Elecraft K4 so you can find those
patterns without spending the night watching the scope. The starting
point was a loop-on-ground near the house: get a baseline before trying
a remote LoG, and figure out which noise is worth chasing.

Leave the radio where it is for passive recording, or dedicate the main
receiver to a repeating band survey. The supplied profiles cover 80–10
meters, with a narrower CW/data profile for quicker revisits.

The result is **one offline HTML report**: band heatmaps, noise-floor
changes, recurring carriers, and combs that may share a source. It looks
for features that switch together, with your equipment on/off notes
alongside them. No server, cloud account, or radio connection needed to
read it.

## What you're looking for

A made-up example—not station measurements or literal CLI output:

| Feature | Example finding |
| --- | --- |
| 40 m background | Four rises of about 8 dB during the night |
| 20 m background | Rises about 5 dB at the same times |
| Carrier combs | Combs on both bands fit a common fundamental near 24 kHz |
| Operator note | “Shed power off” falls between the last noisy pass and the next quiet one |

That's a reason to repeat the power-off test, not a verdict against
anything in the shed. You get a shorter list of suspects, with
frequencies and times to check them against.

## Try it

You'll need a K4 on your LAN and Rust. From the checkout:

```sh
mise install                     # the project's pinned Rust toolchain
cargo install --locked --path crates/k4na-cli
cp config/example.toml config/local.toml
```

Edit `config/local.toml` for your radio host and antenna wiring. The
example uses `./data`, a 4 GiB measurement cap, and a 5 GiB free-space
reserve. Start with passive capture: it never changes radio settings and
records only the spectrum the radio delivers.

```sh
k4na check --config config/local.toml
k4na capture --config config/local.toml --duration 5m
k4na report --data ./data --out report.html
```

Open `report.html`. While a later run is recording, use another shell to
note a test with `k4na mark --data ./data "shed power off"`, then
regenerate the report.

To hunt a source, tune the K4 to one of its lines and run
`k4na watch --config config/local.toml --hz 10123.547k --mark` (or
`--f0 <comb fundamental>` for every harmonic in view). It reads only
the spectrum stream, never CAT, and prints each line's dB over its local
background and the floor every second, plus confirmed changes. Type a
note such as `breaker 12 off` and Enter while flipping breakers; with
`--mark` the notes and changes are saved as marks.

Device logs make marks without guessing times. Export a thermostat's or
energy monitor's history as CSV and import the intervals when a column
was on, for example:

```sh
k4na import-marks --data ./data thermostat.csv --time Date,Time \
  --tz America/New_York --on "Heat Stage 1 (sec)" --label furnace
```

To scan bands, follow the [survey setup](docs/collection.md). Reserve the
main receiver, preview with `--dry-run`, and start with a short, attended
run. Retuning requires config consent **and** `--allow-retune`. There are
no transmit or keying commands. Operator activity or lost CAT state stops
the survey; interrupted runs need [explicit recovery](docs/recovery.md).

## Keep the results in perspective

Levels are on the K4's native scale, **not calibrated dBm**. Less noise
doesn't necessarily mean better SNR. A survey can miss noise between
visits, and a persistent carrier isn't proof of local RFI.

Capture, surveys, analysis, and reports work today, with short live
checks on a K4D running R39. Read the
[remaining hardware checks](docs/collection.md#unverified-on-hardware)
before trusting unattended runs. Simultaneous main/sub antenna comparison
is still planned.

## Technical details

- [Collection and survey profiles](docs/collection.md), [receiver settings](docs/measurement.md), and [recovery](docs/recovery.md)
- [Analysis methods and interpretation](docs/analysis.md)
- [Storage and retention](docs/storage.md)
- [Architecture](docs/architecture.md) and [implementation plan](docs/implementation-plan.md)

Written in Rust, with local SQLite storage and read-only offline
analysis. `mise run ci` runs the checks and tests without a radio.

The protocol work builds on the author's Ham2K HaLo contributions:
[LAN spectrum](https://github.com/ham2k/halo/pull/3) and
[Main/Sub panadapters](https://github.com/ham2k/halo/pull/4).
