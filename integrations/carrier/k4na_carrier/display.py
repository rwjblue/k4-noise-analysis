"""Safe, shared presentation of native equipment observations and changes."""

import json
import sys

from .api import Reading, numeric
from .store import iso


def clean(value, limit=160) -> str:
    """Never allow telemetry to inject terminal controls or multiline log records."""
    return " ".join("".join(c for c in str(value) if c.isprintable()).split())[:limit]


def equipment(reading: Reading) -> str:
    if reading.quality != "observed":
        return "unknown: " + clean(reading.quality)
    parts = []
    for name in ("odu", "idu"):
        unit = reading.values.get(name, {})
        status = unit.get("opstat")
        rendered = "unknown" if status is None else clean(status, 64)
        if unit.get("type") in ("varcaphp", "varcapac") and numeric(status) is not None:
            rendered += "% output"
        parts.append(f"{name.upper()}={rendered}")
    indoor = reading.values.get("idu", {})
    for key, label, unit in (
        ("cfm", "airflow", "cfm"),
        ("blwrpm", "blower", "rpm"),
        ("statpress", "static pressure", "native"),
    ):
        if indoor.get(key) is not None:
            parts.append(f"{label}={clean(indoor[key], 64)} {unit}")
    return "; ".join(parts)


def fingerprint(reading: Reading) -> tuple:
    # Exclude timestamp, outdoor temperature and desired mode. These are
    # equipment snapshot changes, not cache refreshes or thermostat setpoints.
    return reading.quality, json.dumps(
        {key: reading.values.get(key, {}) for key in ("odu", "idu")}, sort_keys=True
    )


class Changes:
    def __init__(self):
        self.previous: dict[str, Reading] = {}

    def observe(self, reading: Reading) -> str:
        previous = self.previous.get(reading.unit)
        self.previous[reading.unit] = reading
        if previous is None:
            return "initial" if reading.quality == "observed" else "unknown"
        if fingerprint(previous) == fingerprint(reading):
            return "unchanged"
        if reading.quality != "observed":
            return "unknown"
        if previous.quality != "observed":
            return "resumed"
        return "change"


class Console:
    def __init__(self, stream=None):
        self.stream = sys.stderr if stream is None else stream
        self.changes = Changes()
        self.samples = self.observed = self.changed = 0

    def line(self, text):
        print(text, file=self.stream, flush=True)

    def observations(self, readings: list[Reading], now_ns: int):
        for reading in readings:
            event = self.changes.observe(reading)
            self.samples += 1
            self.observed += reading.quality == "observed"
            self.changed += event == "change"
            age = (
                f"source age={(now_ns - reading.reported_utc_ns) / 1e9:.1f}s"
                if reading.reported_utc_ns is not None
                else "source time missing"
            )
            self.line(
                f"{iso(now_ns)} HVAC {clean(reading.unit, 48)} [{event}] "
                f"{equipment(reading)}; {age}"
            )

    def unavailable(self, reason: str, now_ns: int):
        units = list(self.changes.previous) or ["all-units"]
        self.observations([Reading(unit, None, {}, reason) for unit in units], now_ns)

    def intervals(self, intervals):
        for start, end, text in intervals:
            self.line(
                f"HVAC interval {iso(start)} -> {iso(end)} "
                f"({(end - start) / 1e9:.1f}s): {clean(text, 500)}"
            )

    def summary(self):
        self.line(
            f"HVAC stopped: {self.samples} saved sample(s), {self.observed} observed, "
            f"{self.samples - self.observed} unknown, {self.changed} equipment snapshot change(s)."
        )
