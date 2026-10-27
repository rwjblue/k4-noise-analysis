# Implementation instructions for coding agents

Read README.md and docs/architecture.md, docs/operations.md,
docs/collection.md, docs/storage.md, docs/analysis.md and
docs/implementation-plan.md before implementing or changing the collector.

The operator owns an Elecraft K4D. Collection must be receive-only.
The project must never include transmit/PTT/keying/test-tone/audio
commands or arbitrary CAT command passthrough. Passive mode is
non-invasive. Active survey requires explicit operator Start/consent,
validated frequency coverage and a minimal fresh pre-retune TX-state
veto. **Manual web Start/Stop replaces the previous elaborate
QK4/external-VFO ownership interlock.** Stop must not restore startup
VFO/settings. Treat missing or stale TX state as unsafe to retune.

Use Rust edition 2024 with current stable tooling; prefer a small,
well-tested workspace and deterministic replay before live-radio
integration. Never assume the K4 LAN spectrum level is calibrated dBm,
the visible scope span equals packet sample rate, or the pan center
equals the VFO. Frame headers and validated readbacks are authoritative
for coverage. Retain original integer units and source packet samples.

Independent analysis/renderers must read versioned data without
needing the radio or mutating source measurements. Disk/memory caps,
recorded gaps and recoverable shards are release requirements, not
polish. The first useful release includes a **small authenticated local
control page** for remote Start/Stop/status, not a graphing dashboard.
Use one collector actor shared by CLI and web. Do not commit
measurements, radio IPs, credentials, private Elecraft attachments,
or customer/user-identifying metadata.

Use the author's HaLo K4 protocol work as prior art and fixtures;
respect its MPL-2.0 obligations, record provenance, and verify any
copying is permitted. Keep changes incremental with short focused
commits; update docs and tests whenever behavior or contracts change.
See docs/implementation-plan.md for the ordered acceptance gates.
