# Carrier telemetry alongside RF recording

`integrations/carrier` is a separate Python helper, not part of the radio
collector. It uses the unofficial [carrier-api](https://github.com/dahlb/carrier_api)
client's authentication and `get_systems` query, never equipment setters.
It needs a compatible Carrier Infinity/SmartHome account. Entry-level systems
are not supported by this first adapter; the app name alone does not establish
compatibility. Account access and actual hardware fields have **not** been
validated live. All automated tests use synthetic responses and a fake RF process.

## Setup and the normal command

`mise.toml` pins Python and uv alongside Rust, and `mise.lock` locks their
platform downloads. `integrations/carrier/uv.lock` locks the helper's Python
dependencies. Tasks use `uv run --locked`; no global pip install or Home
Assistant installation is needed.

All three argument-taking tasks declare Usage flags for native help, defaults,
file/directory completion and `capture`/`survey` choices. Flags follow the task
name directly; no extra `--` is needed. Try `mise run record --help`,
`mise run carrier:log --help`, or `mise run carrier:import --help`. Mise's shell
completion uses the same declarations; it does not need the separate `usage`
CLI. The wrappers forward original arguments, not Usage environment defaults,
so retune consent can never be synthesized from an environment variable.

1. Run `mise install`.
2. Create `.env` using `.env.example` as a template (do not overwrite an
   existing `.env`). Set `CARRIER_USERNAME` and `CARRIER_PASSWORD` to your
   SmartHome credentials. Quote dotenv values containing spaces or `#`.
3. Configure `config/local.toml` as usual, then run:

```sh
mise run record --duration 8h
```

Mise loads `.env` through `[env] _.file`; the helper does not parse or source
it. `.env` is ignored. Passwords are never command-line arguments or written
into telemetry, marks, or exception logs. Carrier HTTP/GraphQL logging is
suppressed, and `CARRIER_*` variables are removed from RF/import child processes.
Do not commit local credentials, measurements, or identity metadata.

The task builds the checkout's `target/debug/k4na` (rather than relying on a
possibly stale installed binary), verifies station/profile configuration,
receives a fresh initial reading for every discovered system, and only then
starts **passive** capture. Default duration is 8 hours. Missing credentials,
unsupported systems, disconnected/stale devices, or a failed first request
refuse startup. Ctrl-C/SIGTERM stops the RF process gracefully and flushes HVAC
intervals. RF's exit code is preserved. The supervisor waits for radio restoration;
it never escalates a slow shutdown to SIGKILL. A hard-killed supervisor cannot
clean up its child: inspect running processes and the RF recovery journal before
restarting an interrupted survey.

Active survey is still a separate, explicit choice. First preview using the
existing `k4na survey --dry-run` workflow. To record an authorized survey:

```sh
mise run record --mode survey --allow-retune --duration 8h \
  --profile config/surveys/hf-cw-us.toml
```

This does not grant config consent or bypass any receiver, coverage, storage,
operator-activity or recovery checks. The wrapper forwards only typed options,
not arbitrary CAT commands. `--raw-full-rate` and `--raw-full-rate-max-bytes`
are forwarded when explicitly supplied.

To knowingly record RF without Carrier access:

```sh
mise run record --without-hvac --duration 5m
```

This never contacts Carrier and records HVAC coverage as unknown, **not off**.
To run only the HVAC logger alongside a separately launched RF session:

```sh
mise run carrier:log --data ./data --duration 8h
```

`carrier:log` never connects to the radio. Both loggers are foreground programs;
there is no installed service, login item, or automatic recording on boot.

## What is recorded and displayed

The first version **polls every 30 seconds**, not a push event subscription.
`--poll-seconds` may be increased (minimum 5); it does not change how often
Carrier's cached device status updates. `--timeout-seconds` defaults to 20.
`--max-age-seconds` defaults to 600: a missing, old, implausibly future source
timestamp or a disconnected/unknown device connection is unobserved. These
are conservative software defaults, not verified thermostat cadence guarantees.

Each source sample preserves host UTC nanoseconds, monotonic elapsed time,
Carrier's source timestamp (including its original string), native equipment
status strings, airflow, blower RPM, static pressure, temperature and units
when supplied. Missing values are not zero. The explicit allowlist excludes
profile names, serials, addresses, schedules, zones and occupancy. Generic
`unit-1`, `unit-2`, etc. aliases are connection-scoped; do not assume permanent
unit identity after the account's system roster changes. Shard metadata records
format/client versions and a random logger-session ID.

The helper derives intervals for reported indoor/outdoor operating states,
including explicitly reported off states. Changing numeric output stays in the
source log, rather than making one annotation per percent. Marks are observed
intervals, not exact switching timestamps: a change between polls has a separate
transition-uncertainty interval. A sampling pause/clock jump, cloud error,
missing system, stale timestamp or logger restart leaves explicit unknown
coverage. When the helper was stopped, do not assume Carrier can backfill cycles.

Completed intervals, split into at most five-minute chunks for live visibility,
are imported automatically with `k4na import-marks`. Their source is
`import:carrier-marks.csv`. Repeated import is deduplicated by start/end/text.
The RF process alone creates its data root; until that root exists marks remain
queued. RF/report code and measurement schemas are unchanged. Regenerate a
report normally to see HVAC intervals on the existing plots. Capacity curves
and automatic HVAC/RF statistical correlation are **not** implemented.

A runtime cloud outage retries without stopping RF. A telemetry write/cap error
stops HVAC logging, prints a persistent warning, and leaves RF running. Final
coverage/gap import is attempted from reserved space. If the filesystem itself
will not accept writes, the terminal warning and last durable checkpoint are
authoritative; no successful gap write can be promised on an unwritable disk.

## Auxiliary storage and recovery

Raw telemetry defaults to `output/carrier`, outside the RF data root because
the RF writer does not budget or prune arbitrary auxiliary files. It has its own
64 MiB cap (`--max-bytes`), 4 MiB/hourly SQLite shard rotation (`--shard-bytes`),
and 128 KiB maximum transaction payload. Admission reserves headroom for SQLite
journals and final coverage/mark import. The combined command inherits
`storage.min_free_bytes`; standalone logging reserves 1 GiB. An explicit
`--min-free-bytes` overrides the helper's reserve, not RF's own storage policy.

Shards use `k4na-carrier/1`, SQLite application ID `0x4b344856`, user version 1,
FULL synchronous transactions and rollback journals. `samples` holds immutable
source observations; `intervals` holds derived marks and mutable import status;
`meta` holds format/client/session information and the recovery checkpoint.
Readers can open them read-only without a radio, account or running logger.
Shard sequence numbers, not wall-clock names, establish recovery order.

There is **no automatic deletion** of telemetry. At the cap, archive the whole
telemetry directory and start a new one, or explicitly raise its budget. Unknown
files are never pruned. An exclusive directory lock prevents duplicate writers.
A format marker binds each telemetry directory to one measurement-root
fingerprint; use `--telemetry-dir` to separate datasets.

A restart closes checkpointed intervals only through their last durable
observation and marks the missing span unknown. Already committed, not-yet-imported
intervals can also be retried **offline**, without authenticating or starting RF:

```sh
mise run carrier:import --data ./data
```

Pass the same `--telemetry-dir` if it was customized. The importer never invents
an end time for a growing live interval; interrupted active intervals are
reconciled on the next successful logger start. If mark import itself fails,
source intervals remain queued. The existing marks format is deliberately not
extended into a second numeric telemetry database.

## Development

```sh
mise run carrier:check  # locked deps; Ruff; offline tests; real offline marks importer
mise run ci            # Rust checks/tests plus the Carrier checks
```

Tests cover redaction, native values, stale/disconnected/missing status, mark
idempotency and nanosecond placement, bounded/rotated shards, crash checkpoints,
clock jumps, startup refusal, cloud/write failures, safe child shutdown, actual
SIGTERM, exit-code forwarding, and explicit survey consent. No test needs
credentials, the radio, or Carrier's service. Dependency installation needs a
package registry unless already cached.
