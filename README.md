# k4-noise-analysis

Collector-first RF noise survey tooling for an Elecraft K4D, written in Rust.

**Status: implementation in progress.** Initial Rust protocol, core/planner
and storage modules have landed; the collector CLI, active K4 survey and
phone-accessible control UI are not yet complete.

## Motivation

Establish a reproducible 24+ hour baseline of noise and candidate birdies
using a near-house loop-on-ground receiving antenna. Later compare that
antenna with a remote LoG using simultaneous K4D main/sub spectra.
Keep collection, analysis and rendering independent, preserve measurements
in original units, and never fill the host disk.

## Proposed first release

A guarded, receive-only CLI will collect the K4's **LAN spectrum stream on
9201**, optionally querying and tuning over **CAT 9200** when explicitly
authorized. Configurable full-band survey intervals (80-10m by default)
will be visited repeatedly. All accepted spectrum frames during a dwell
contribute to per-bin temporal summaries; a sparse sample of original
checksum-valid packets is retained for replay. Versioned SQLite shards
have independent summary/raw retention and a hard byte quota.

Passive capture never retunes; active survey must be intentionally enabled
and must pause on radio use, unknown TX state, state conflicts, unusable
spectrum coverage or resource exhaustion. The radio cannot transmit
through the collector.

Recommended initial cadence is **5 seconds per frequency window** plus
700 ms settling, repeating a full configured 80–10m sweep approximately
every **5–6 minutes** at the observed 96 kHz main-pan sample rate. The
collector runs until stopped via a small local web page or CLI.
A future `k4na serve` process will be controllable from an authenticated
phone browser on the trusted home LAN without detecting QK4 usage.

The K4 LAN spectrum readings are in native tenths-of-dB units, **not yet
established as calibrated dBm**. A stationary carrier is a candidate
birdie, not automatically a confirmed household RFI source.

## Design and implementation entry points

- [Architecture and invariants](docs/architecture.md)
- [Survey cadence and manual web control](docs/operations.md)
- [Survey and protocol acceptance](docs/collection.md)
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
testable milestones. The initial deliverable is valid, bounded **acquisition** with manual
Start/Stop and real status from a lightweight mobile browser, not a
speculative automatic signal classifier. See the follow-on operations
contract before implementing complicated CAT conflict detection.
