"""Bounded offline event reporting; never instantiate Journal or a cloud client."""

import heapq
import json
import sqlite3
import sys
from collections import deque
from contextlib import closing
from dataclasses import dataclass
from pathlib import Path

from .api import Reading, equipment_values
from .context import retained_context
from .display import Changes, clean, equipment
from .store import JournalError, checked_connection, iso


class ReportError(JournalError):
    pass


@dataclass
class Unit:
    session: str
    alias: str
    samples: int = 0
    observed: int = 0
    changes: int = 0
    last_ns: int = 0
    last: Reading | None = None
    minimum_output: float | None = None
    maximum_output: float | None = None

    def add(self, reading, observed_ns, event):
        self.samples += 1
        self.observed += reading.quality == "observed"
        self.changes += event == "change"
        self.last_ns = observed_ns
        self.last = reading
        value = reading.outdoor_percent
        if value is not None:
            self.minimum_output = (
                value if self.minimum_output is None else min(self.minimum_output, value)
            )
            self.maximum_output = (
                value if self.maximum_output is None else max(self.maximum_output, value)
            )


def utc(ns: int) -> str:
    return iso(ns).split(".", 1)[0] + "Z"


def write_report(directory: Path, limit=25, stream=None):
    if not 1 <= limit <= 1000:
        raise ReportError("--limit must be between 1 and 1000")
    # Do not collect an arbitrarily large directory into memory.
    paths = heapq.nsmallest(4097, directory.glob("carrier-*.sqlite"))
    if len(paths) > 4096:
        raise ReportError("too many shards; select a directory with at most 4096 shards")
    if not paths:
        raise ReportError("no Carrier telemetry shards found; run carrier:log first")
    units = {}
    trackers = {}
    events = deque(maxlen=limit)
    intervals = deque(maxlen=limit)
    samples = observed = changes = event_count = interval_count = pending = 0
    first_ns = last_ns = None
    for path in paths:
        try:
            with closing(checked_connection(path)) as db:
                db.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, 256 * 1024)
                db.execute("BEGIN")  # One read-only snapshot per shard, safe during collection.
                meta = dict(
                    db.execute("SELECT key,value FROM meta WHERE key IN ('format','session')")
                )
                session = meta.get("session", "")
                if meta.get("format") != "k4na-carrier/1" or (
                    len(session) != 32 or any(c not in "0123456789abcdef" for c in session)
                ):
                    raise ValueError("invalid format/session")
                if session not in trackers:
                    if len(trackers) >= 4096:
                        raise ReportError("report session cap reached; select a smaller dataset")
                    trackers[session] = Changes()
                for timestamp, alias, reported, quality, raw in db.execute(
                    "SELECT observed_utc_ns,unit,reported_utc_ns,quality,values_json "
                    "FROM samples ORDER BY id"
                ):
                    if (
                        not isinstance(timestamp, int)
                        or not isinstance(alias, str)
                        or len(alias) > 48
                        or not isinstance(quality, str)
                        or len(quality) > 160
                        or not isinstance(raw, str)
                        or len(raw.encode()) > 128 * 1024
                        or (reported is not None and not isinstance(reported, int))
                    ):
                        raise ValueError("invalid sample")
                    values = json.loads(raw)
                    if not isinstance(values, dict):
                        raise ValueError("invalid sample values")
                    reading = Reading(
                        alias,
                        reported,
                        equipment_values(values) | retained_context(values),
                        quality,
                    )
                    key = session, alias
                    if key not in units:
                        if len(units) >= 4096:
                            raise ReportError(
                                "report unit/session cap reached; select a smaller dataset"
                            )
                        units[key] = Unit(session, alias)
                    event = trackers[session].observe(reading)
                    units[key].add(reading, timestamp, event)
                    samples += 1
                    observed += quality == "observed"
                    changes += event == "change"
                    first_ns = timestamp if first_ns is None else min(first_ns, timestamp)
                    last_ns = timestamp if last_ns is None else max(last_ns, timestamp)
                    if event != "unchanged":
                        event_count += 1
                        age = (
                            "source time missing"
                            if reported is None
                            else (f"source age={(timestamp - reported) / 1e9:.1f}s")
                        )
                        events.append(
                            f"  {utc(timestamp)}  {session[:8]}/{clean(alias, 48)} "
                            f"[{event}] {equipment(reading)}; {age}"
                        )
                for start, end, text, imported in db.execute(
                    "SELECT start_utc_ns,end_utc_ns,text,imported FROM intervals ORDER BY id"
                ):
                    if (
                        not isinstance(start, int)
                        or not isinstance(end, int)
                        or end < start
                        or not isinstance(text, str)
                        or len(text) > 500
                        or imported not in (0, 1)
                    ):
                        raise ValueError("invalid interval")
                    interval_count += 1
                    pending += imported == 0
                    intervals.append(
                        f"  {utc(start)} -> {utc(end)} ({(end - start) / 1e9:.1f}s) "
                        f"[{session[:8]}; {'imported' if imported else 'pending import'}] "
                        f"{clean(text, 500)}"
                    )
        except OSError, sqlite3.Error, ValueError, TypeError, RecursionError:
            raise ReportError(
                f"unsupported, damaged, or unrecovered telemetry shard: {clean(path.name)}; "
                "restart the logger or use carrier:import to recover an interrupted transaction"
            ) from None
    out = sys.stdout if stream is None else stream
    print("Carrier HVAC event report (UTC)", file=out)
    print(
        f"Shards: {len(paths)} | Sessions: {len(trackers)} | Unit/session pairs: {len(units)}",
        file=out,
    )
    print(f"Samples: {samples} | Observed: {observed} | Unknown: {samples - observed}", file=out)
    span = "no observations" if first_ns is None else f"{utc(first_ns)} -> {utc(last_ns)}"
    print(f"Saved receive-time range: {span}", file=out)
    print(f"Equipment snapshot changes: {changes} | Timeline events: {event_count}", file=out)
    print(f"Completed interval rows: {interval_count} | Pending mark import: {pending}", file=out)
    print("\nLatest unit/session summaries:", file=out)
    for unit in heapq.nlargest(limit, units.values(), key=lambda u: u.last_ns):
        output = (
            ""
            if unit.minimum_output is None
            else (f"; reported output range={unit.minimum_output:g}..{unit.maximum_output:g}%")
        )
        print(
            f"  {unit.session[:8]}/{clean(unit.alias, 48)}: {unit.observed}/{unit.samples} "
            f"observed, {unit.changes} change(s); {equipment(unit.last)}{output}",
            file=out,
        )
    print(f"\nTimeline (latest {len(events)} of {event_count} events, recording order):", file=out)
    for line in events:
        print(line, file=out)
    if not events:
        print("  (none)", file=out)
    print(f"\nIntervals (latest {len(intervals)} of {interval_count} rows):", file=out)
    for line in intervals:
        print(line, file=out)
    if not intervals:
        print("  (none; active intervals may not be finalized yet)", file=out)
    print(
        "\nChanges compare saved snapshots, not exact hardware switching times. "
        "Unknown coverage is not off; percent output is not electrical power. "
        "Unit/zone aliases are session-scoped; same-state five-minute chunks are not new cycles. "
        "Conditioning keeps active/prep/pending distinct and does not identify the heat source. "
        "Older samples may lack equipment categories and conditioning context.",
        file=out,
    )
