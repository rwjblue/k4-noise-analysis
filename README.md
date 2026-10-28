# k4-noise-analysis

Collector-first RF noise survey tooling for an Elecraft K4D, written in Rust.

**Status: collector implemented.** The protocol decoder replays real R39
captures. Passive capture and band/antenna survey passes have run on a
K4D (R39), as has a short check of receiver measurement settings and
kill-and-recover through the journal. Conflict handling and storage
limits (including an accelerated 24-hour run under a 64 MiB cap) are
tested against a simulated K4. See
[unverified hardware assumptions](docs/collection.md#unverified-on-hardware)
before trusting unattended runs.

## Quick start

~~~text
mise install                     # pinned stable Rust
mise run ci                      # fmt, clippy, tests (no radio needed)
cargo run -p k4na-cli -- check --config config/example.toml
k4na estimate --config my.toml --duration 24h
k4na capture  --config my.toml --duration 5m        # passive, no setters
k4na survey   --config my.toml --duration 1m --dry-run
k4na survey   --config my.toml --duration 24h --allow-retune
k4na survey   --config my.toml --profile config/surveys/hf-cw-us.toml --duration 24h --allow-retune
k4na probe    --config my.toml                      # GET-only state readout
k4na recovery list --config my.toml                 # interrupted surveys
k4na recovery restore <session> --dry-run --config my.toml
k4na inspect  --data ./data
k4na analyze bands    --data ./data              # background per band/hour (read-only)
k4na analyze carriers --data ./data --format csv # recurring narrow peaks
k4na analyze families --data ./data              # combs: carriers on a common grid
k4na report  --data ./data --out report.html     # heatmaps + tables, one offline file
k4na simulate                    # SIMULATED K4 on 127.0.0.1:9200/9201
~~~

A survey journals every radio change before sending it. If one is
interrupted, new surveys refuse to start until `k4na recovery restore`
or `acknowledge` settles it; see [docs/recovery.md](docs/recovery.md).

`mise run long-run` runs the accelerated 24-hour simulated survey.

The station file (`config/example.toml`) holds the radio, antenna
registry, safety and storage settings. Survey profiles under
`config/surveys/` choose antennas, frequency ranges and cadence; see
[docs/collection.md](docs/collection.md#station-file-and-survey-profiles).

## Motivation

Establish a reproducible 24+ hour baseline of noise and candidate birdies
using a near-house loop-on-ground receiving antenna. Later compare that
antenna with a remote LoG using simultaneous K4D main/sub spectra.
Keep collection, analysis and rendering independent, preserve measurements
in original units, and never fill the host disk.

## First release

A guarded, receive-only CLI collects the K4's **LAN spectrum stream on
9201**, optionally querying and tuning over **CAT 9200** when explicitly
authorized. Configurable full-band survey intervals (80-10m by default)
are visited repeatedly. All accepted spectrum frames during a dwell
contribute to per-bin temporal summaries; a sparse sample of original
checksum-valid packets is retained for replay. Versioned SQLite shards
have independent summary/raw retention and a hard byte quota.

Passive capture never retunes. Active survey must be intentionally enabled,
and it stops on radio use, unknown TX state, state conflicts, unusable
spectrum coverage or resource exhaustion. The radio cannot transmit
through the collector.

The K4 LAN spectrum readings are in native tenths-of-dB units, **not yet
established as calibrated dBm**. A stationary carrier is a candidate
birdie, not automatically a confirmed household RFI source.

## Design and implementation entry points

- [Architecture and invariants](docs/architecture.md)
- [Survey and protocol acceptance](docs/collection.md)
- [Receiver measurement settings](docs/measurement.md)
- [Survey recovery journal](docs/recovery.md)
- [Storage, quotas and retention](docs/storage.md)
- [Later analysis and near/far comparison](docs/analysis.md)
- [Ordered implementation plan and acceptance tests](docs/implementation-plan.md)
- [Example configuration](config/example.toml)
- [Agent instructions](AGENTS.md)

## Prior art

The author has already implemented K4 spectrum handling and captured
physical K4 R39 packet fixtures for Ham2K HaLo:
- [K4 LAN spectrum PR](https://github.com/ham2k/halo/pull/3)
- [K4 secure Main/Sub panadapter PR](https://github.com/ham2k/halo/pull/4)

The local 9201 stream has a simpler implementation path than the TLS-PSK
9204 stream for this tool. For K4 radio CAT syntax, see the
[Elecraft Programmer's Reference D12](https://ftp.elecraft.com/K4/Manuals%20Downloads/K4ProgrammersReferencerev.D12.html).

## Implementation

Use Rust stable (edition 2024) and follow
[docs/implementation-plan.md](docs/implementation-plan.md) in small
testable milestones. The initial deliverable is valid and bounded
**acquisition**, not a speculative automatic signal classifier.
