# Survey cadence and manual web control

Status: **design change (follow-on PR)**. The existing Rust config/parser and
collector scaffolds still reflect parts of the previous interlock design;
the coding agent must implement the changes below. This document specifies
**desired behavior**, not an available UI/CLI.

## Operating concept

The operator is the primary arbiter of radio use. Run one long-lived local
`k4na serve` process on a Mac/Linux host. It serves a tiny mobile-friendly
web page and owns a single collector lifecycle; the browser is only a remote
control and status display. Closing the browser does not stop acquisition.
The collector begins **stopped** after service launch or process restart.

When the operator wants to use the K4/QK4, tap **Stop** on a phone, then
operate. When finished, tap **Start**. No fragile QK4-presence detection,
exclusive CAT lease, or 500 ms external-VFO policing is required. Pressing
Stop is an operator instruction, not a radio TX-inhibit or RF-safety device.
If QK4 and the collector are intentionally used at the same time, the
software does not guarantee that their frequency changes will not race.

Do **not** restore the startup frequency, mode or pan settings on a normal
Stop. Leave the receiver where the survey last left it, stop issuing CAT
writes, finalize the interrupted visit and close the acquisition session.
The operator will retune normally with QK4. This intentionally replaces
the former conditional-restore-on-conflict plan.

### Retained hard limits

Simplify conflict detection, not measurement integrity:
- No transmit, PTT, keying, TUNE carrier, test tone, or arbitrary CAT-script
  path; only a narrow typed and tested RX control whitelist.
- Before *each* active retune, perform a fresh inexpensive CAT TX-state
  check (or use a fresh verified notification). If transmitting, unknown,
  or stale, **do not retune**. Stop/suspend surveying with an explicit reason
  until the operator starts it again. A read/retune race cannot be eliminated
  without a hardware lock; never describe this as a physical safety interlock.
- A stopped collector sends **no further new** CAT setters. Commands
  already written to the socket may still be in flight at the instant
  Stop is requested; serialize commands to honor cancellation at the next
  safe boundary.
- Every accepted visit must match actual observed PAN center/sample rate,
  proper antenna/receiver, acceptable state metadata and usable bin coverage.
  Unexpected changes can mark the visit partial, be skipped, or cause
  re-planning; do not mislabel frequencies or claim missing data were quiet.
- Continue enforcing bounded RAM, hard disk quota, data-shard integrity,
  checksum validation and redacted logs.

## How often do we sample?

**Recommendation: repeat a complete 80-through-10-meter sweep about every
five minutes**. This is a target full-cycle *start-to-start* period, not a
single instantaneous reading every five minutes.

Suggested initial settings:
- 5,000 ms *measured dwell* per frequency tile after a 700 ms settle.
- Minimum survey cycle period of 300,000 ms (five minutes).
- All configured eight bands, in contiguous band-at-a-time tiles.
- One raw CRC-valid frame sampled per second at most; every accepted frame
  contributes to online visit statistics.
- Repeat forever until a user Stop, process termination, resource ceiling
  or unrecoverable error. Every Start begins a new session.

The current `SurveyPlan` creates **50 tiles** for the example 80/40/30/
20/17/15/12/10m bands at an *observed* 96 kHz sample rate, 2,048 bins,
and 80% usable width: 7,4,1,5,2,6,2,23 tiles respectively. Each 5-second
dwell at approximately 15 PAN frames/sec supplies about 75 frames.
Fifty times (5 + 0.7 seconds) gives **285 seconds / 4m45s** before CAT
readbacks, transitions and storage overhead. Thus a practical cycle may
be around **5–6 minutes**, not a guarantee of exactly five minutes.
At five to six minutes, each frequency window is visited about 240–288
times in 24 hours.

Actual cycle duration is `max(min_cycle_period, time_taken_to_complete)`.
If the cycle is shorter than 5m, wait the remaining time; if longer, start
the next cycle immediately. **Never** launch overlapping cycles or cut
a band short simply to meet the period. Record target/actual elapsed
cycle time and per-tile revisit interval. The target is configurable,
and can be zero for continuous back-to-back passes.

The number of tiles must be recalculated from actual PAN packet rate and
usable bounds, not the configured `#SPN` width or the VFO. At 24 kHz
sample rate many more tiles are needed and a five-minute cycle may be
unachievable; show that accurately on the control page and CLI. Do not
silently decrease dwell or claim 5m coverage in that case.

This is deliberately a **low-duty-cycle survey**: at 5 seconds every
300 seconds, any one frequency is observed only about 1.7% of the
wall-clock time. Excellent for stationary birdies and seasonal/daily
patterns; insufficient to conclude an intermittent source never emits.
Later, support focused surveys via a subset of configured bands/ranges
and longer dwell/revisit behavior (for example selected 40m frequencies
continuously). Do not block v1 on a second scheduling algorithm.

## Service and control plane

`k4na serve --config config/local.toml` is the target mode. Keep CLI
`k4na survey --allow-retune` and passive capture for standalone/headless
work. Both modes share one collector lifecycle component. The HTTP process
must never spawn uncontrolled child collectors or open a second SQLite
writer. A data-root writer lock prevents concurrent CLI and service runs.

Default: bind `127.0.0.1:8787`; to use an iPhone on the same trusted Wi-Fi,
allow an explicit opt-in bind to the host LAN interface (such as
`0.0.0.0:8787`). For example, navigate to
`http://<host-LAN-address>:8787` from a phone. Use `<host>.local`
only where local hostname resolution actually works.

Use a tiny static/embedded HTML page with minimal JS/CSS and a small Rust
HTTP server (Axum is fine if already using Tokio). No React, SPA build,
WebSocket, database service or remote dashboard is required.

### States

- **Stopped** (also the startup state): web server responsive; radio survey
  sockets may be closed; absolutely no CAT writes.
- **Starting**: preflight config, receiver, PAN and disk; one new session.
- **Running**: acquisition, current tile and counters; phone may disconnect.
- **Stopping**: cancellation set immediately; no new retune, flush partial
  visit, finalize session, return to Stopped.
- **Error / Needs attention**: last failure visible; no automatic retuning;
  authenticated Start performs a new preflight and retries.

There is no automatic restart of a retuning survey after service/host
reboot; process may be supervised by launchd/systemd, but **collection**
resumes only after the operator requests Start.

`POST /api/start`:
- Requires authenticated operator request and `collection.allow_retune =
  true` (or passive configured mode). Clicking Start is the explicit
  runtime authorization instead of requiring a startup CLI flag from a
  server process.
- Idempotent if already running/starting. Refuse with actionable status
  if disk/quota, radio or configuration preflight fails.
- Starts a new recorded session. Starting never changes antenna routing.

`POST /api/stop`:
- Idempotent and high priority, including during reconnect/settle/dwell.
  Stops active scan quickly without waiting for the remaining tile/cycle.
- Does not restore VFO/mode, and does not power off/transmit through K4.
- Flushes and marks partial records/gaps, finalizes shards/session and
  yields radio control. Display Stopping until truly finished.

`GET /api/status`:
- Read-only JSON from the collector lifecycle state (not a second
  connection that independently polls the radio).
- State, run start/session ID, current band/tile and RF range, progress
  through cycle, actual last-cycle duration and per-band revisit/coverage,
  accepted/rejected frames/visits, gaps, network health, disk bytes/quota,
  last error and last update time. Exclude radio host, secrets and other
  sensitive configuration data.
- Browser polls about every 2 seconds. This UI refresh interval is NOT
  the RF survey interval or a CAT polling interval.
- No arbitrary CAT commands, user-supplied band endpoints, or config
  editing exposed over HTTP in v1.

A single compact page shows **Running / Stopped / Error**, **Start** and
**Stop**, current band/frequency tile, last full sweep, total measurements,
disk usage and last error. Buttons have clear pending states and display
confirmed outcomes, not optimistic success. Usable in a mobile browser.

### Authentication and network boundaries

- Loopback-only mode may use local-only control without authentication.
- Binding to a non-loopback interface **requires** an independently
  configured random control secret (e.g. environment variable; do not
  commit or log it). Provide a simple login form, server-side authenticated
  session cookie (`HttpOnly`, `SameSite=Strict`), and CSRF/origin
  protections for POST commands; never use mutating GET endpoints.
- No public Internet listener, port forwarding, permissive CORS or URL
  query-string tokens. Plain HTTP on a trusted home LAN is only a
  convenience; credentials/cookies are not encrypted in transit. Use a
  trusted VPN/Tailscale with HTTPS termination if controlling from
  outside the LAN or if LAN eavesdropping matters.
- Bind address, web enablement and secret source live in strict TOML.
  A missing secret must prevent LAN binding rather than exposing
  unauthenticated controls. Browser status is authenticated too.

## Proposed config extensions (not yet accepted by current parser)

The existing `config/example.toml` remains parseable by the partially
implemented strict Rust parser. The agent must extend its schema and tests
before uncommenting new config fields:

~~~toml
[collection]
mode = "survey"
allow_retune = true
dwell_ms = 5000
settle_ms = 700
min_cycle_interval_ms = 300000

[control]
enabled = true
bind = "127.0.0.1:8787"     # or explicit trusted-LAN address
auth_token_env = "K4NA_CONTROL_TOKEN"
start_on_launch = false     # v1 must remain false
~~~

Eliminate the old mandatory `pause_on_external_change` and
`manual_resume_on_conflict` behavior and expensive continuous
external-change polling. Remove their strict `true` validation and
migrate/deprecate the TOML keys (or retain only parse-time compatibility
with explicit warnings). Keep `pause_on_transmit` as the always-on
simple pre-retune veto; the app's readonly/state-validation checks still
protect data integrity.

Do not attempt to implement speculative `FI` pan-only cross-band tuning
as a prerequisite for this operating model. Use the known observed-coverage
planner and explicit CAT retunes first; pan-only exploration can be a
separate physical-radio experiment.

## Acceptance tests

1. In loopback/default web mode, service starts **Stopped**, web requests
   return sensible state, no CAT setter is sent, and the process remains up.
2. From a phone-sized browser on trusted LAN, after login Start causes one
   collector session and valid survey data; Stop cancels immediately and
   stays accessible for a later Start. Reloading/closing the browser does
   not change collection state.
3. Two rapid Start requests do not spawn two collectors or database
   writers. Stop is idempotent before, during and after a survey. No
   automatic resurrection after restart or disconnect.
4. Mock K4: transmit is active just before a retune -> no frequency
   change; every emitted CAT command is in reviewed receive-only
   whitelist. A state-change race is documented, not claimed impossible.
5. External operator/QK4 changes do not trigger a complicated ownership
   protocol. The next collected frame is validated against the actual
   tile, and mismatches create gaps/replan without false measurements.
6. A 96 kHz synthetic stream with example bands produces 50 tiles,
   completes in around 5m or longer as warranted by simulated overhead,
   and records full cycle time, visits and actual revisit latency.
7. Deliberately low 24 kHz sample rate grows the tile count; UI shows
   that the cycle exceeds target rather than skipping tiles or lying
   about temporal coverage.
8. Simulate disk full, no K4 connection, SIGTERM and stop during
   reconnect/settle/write: no new CAT setters after Stop is accepted,
   partial visits and failures are accounted for, shards recover.
9. LAN listener without auth fails to launch, unauthorized POSTs are
   rejected, and cross-origin/browser CSRF requests cannot start a
   collector.
