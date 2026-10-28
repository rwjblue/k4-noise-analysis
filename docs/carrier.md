# Carrier telemetry alongside RF recording

`integrations/carrier` is a separate Python helper, not part of the radio
collector. It uses the unofficial [carrier-api](https://github.com/dahlb/carrier_api)
client's authentication and `get_systems` query, never equipment setters.
It needs a compatible Carrier Infinity/SmartHome account. Entry-level systems
are not supported by this first adapter; the app name alone does not establish
compatibility. Authentication, repeated read-only polling, console output,
private SQLite recording and offline reporting passed a short Carrier-only
live smoke test. No thermostat settings changed, no radio was connected, and
smoke-test marks were not imported into RF data. Heating/cooling transitions,
stage/output changes and combined RF operation remain unverified live. All
automated tests use synthetic responses and a fake RF process.

## Setup and the normal command

`mise.toml` pins Python and uv alongside Rust, and `mise.lock` locks their
platform downloads. `integrations/carrier/uv.lock` locks the helper's Python
dependencies. Tasks use `uv run --locked`; no global pip install or Home
Assistant installation is needed.

Recording, logging, import and report tasks declare Usage flags for native help, defaults,
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

The console prints and flushes a safe summary after each saved observation:
UTC receive time, unit alias, initial/change/unchanged/unknown/resumed status,
reported indoor/outdoor operation, numeric output, airflow and blower RPM when
present, and source timestamp age. Percent units are shown only for recognized
variable-capacity equipment types; other values remain native. Closed intervals,
successful mark imports, and final observed/unknown/change counts are also
printed. A change means the equipment snapshots differ, not that exact hardware
switching time is known. No profiles, credentials, or raw account responses are
printed; terminal controls in returned strings are removed.

## Offline event report

```sh
mise run carrier:report
mise run carrier:report --telemetry-dir output/carrier-test --limit 10
mise run carrier:report --help
```

This small task reads existing SQLite shards in read-only mode, without a cloud
client, radio connection, credential requirement, Rust build, mark import or
checkpoint update. It prints sample/quality totals, per-session unit summaries,
reported variable-capacity ranges, a recent event timeline, and completed
intervals with their import status. `--limit` bounds rows in each section
(default 25, maximum 1000); totals still cover the whole selected directory.
The reader streams rows with explicit shard, unit/session and payload limits.

Events are derived from consecutive equipment snapshots: initial, change,
unknown and resumed. Cache timestamp refreshes alone are not changes; blower
RPM or pressure changes are. Repeated states across shard rotation are not new
cycles. Unit aliases are grouped by logger session, not assumed identical
across reconnects/restarts. A short run may show no heating/cooling transition;
that is different from showing no observations. Reported `opstat=off` does not
imply that blower airflow or electrical consumption is zero. No electrical
power measurements are recorded. The report never modifies source files, even
when a shard needs recovery; use the logger or offline importer for recovery.

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

### Recorded data model (version 1)

| Table | Fields and meaning |
| --- | --- |
| `samples` | `id` is shard-local; `observed_utc_ns` is host receive time; `elapsed_ns` is monotonic time since logger start; `unit` is a session-local alias; nullable `reported_utc_ns` is parsed Carrier source time; `quality` distinguishes observed from stale/disconnected/missing data; `values_json` retains native allowlisted fields. |
| `intervals` | Shard-local `id`; `start_utc_ns` and `end_utc_ns`; descriptive `text`; `imported` is 0 until successful deduplicated mark import and 1 afterward. These are derived observed/unknown/transition spans, not a complete device event archive. |
| `meta` | Key/value format and client versions, random logger `session` ID, shard creation UTC, and mutable crash-recovery `checkpoint` with the latest observations and unfinished interval state. |

`values_json` allows `utcTime`, `isDisconnected`, `mode`, `cfgem`, `oat`,
`odu.{type,opstat,iducfm}`, and `idu.{type,opstat,cfm,statpress,blwrpm}` when
supplied. Numeric-looking source strings remain strings; the original source
UTC string is retained as well as its parsed timestamp. `mode` is the selected
thermostat mode, not proof that equipment is running. Status and measured
blower/airflow values are kept independently, without inventing consistency or
missing zeroes. There is no new `events` table: console/report events are views
over canonical samples, while intervals feed the existing RF annotations.

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
