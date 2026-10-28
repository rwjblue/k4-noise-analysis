"""Observed intervals, explicit uncertainty, and crash-safe coverage checkpoints."""

from copy import deepcopy

from .api import Reading
from .store import Journal


class Recorder:
    def __init__(self, journal: Journal, max_gap_seconds: float, chunk_seconds: float = 300):
        self.journal = journal
        self.max_gap_ns = int(max_gap_seconds * 1_000_000_000)
        self.chunk_ns = int(chunk_seconds * 1_000_000_000)
        self.tracks = {}
        self.last_utc_ns = None
        self.last_elapsed_ns = None
        self.recovered = False

    def tick(self, readings: list[Reading], now_ns: int, elapsed_ns: int):
        tracks = deepcopy(self.tracks)
        intervals = []
        if not self.recovered:
            previous = self.journal.previous
            for track in previous.get("tracks", {}).values():
                intervals.append((track["start"], track["last"], track["label"]))
            prior = previous.get("last_utc_ns")
            if prior is not None and prior <= now_ns:
                intervals.append((prior, now_ns, "HVAC telemetry unobserved (logger not running)"))
        if self.last_utc_ns is not None:
            wall_delta = now_ns - self.last_utc_ns
            elapsed_delta = elapsed_ns - self.last_elapsed_ns
            if wall_delta > self.max_gap_ns or abs(wall_delta - elapsed_delta) > 5_000_000_000:
                for track in tracks.values():
                    intervals.append((track["start"], track["last"], track["label"]))
                if self.last_utc_ns <= now_ns:
                    intervals.append(
                        (
                            self.last_utc_ns,
                            now_ns,
                            "HVAC telemetry unobserved (sampling pause or host clock jump)",
                        )
                    )
                else:
                    intervals.append((now_ns, now_ns, "HVAC host clock moved backwards"))
                tracks.clear()
        samples = []
        for reading in readings:
            label = reading.label
            old = tracks.get(reading.unit)
            start = now_ns
            if old:
                if old["label"] == label:
                    start = old["start"]
                elif old["quality"] != "observed":
                    intervals.append((old["start"], now_ns, old["label"]))
                else:
                    intervals.append((old["start"], old["last"], old["label"]))
                    if reading.quality == "observed":
                        intervals.append(
                            (
                                old["last"],
                                now_ns,
                                f"HVAC {reading.unit}: state transition between polls",
                            )
                        )
                    else:
                        start = old["last"]
            if now_ns - start >= self.chunk_ns:
                intervals.append((start, now_ns, label))
                start = now_ns
            tracks[reading.unit] = {
                "start": start,
                "last": now_ns,
                "label": label,
                "quality": reading.quality,
            }
            samples.append(
                {
                    "observed_utc_ns": now_ns,
                    "elapsed_ns": elapsed_ns,
                    "unit": reading.unit,
                    "reported_utc_ns": reading.reported_utc_ns,
                    "quality": reading.quality,
                    "values": reading.values,
                }
            )
        checkpoint = {"last_utc_ns": now_ns, "tracks": tracks}
        self.journal.commit(samples, intervals, checkpoint)
        self.tracks = tracks
        self.last_utc_ns = now_ns
        self.last_elapsed_ns = elapsed_ns
        self.recovered = True

    def unavailable(self, reason: str, now_ns: int, elapsed_ns: int):
        units = list(self.tracks) or ["all-units"]
        self.tick([Reading(unit, None, {}, reason) for unit in units], now_ns, elapsed_ns)

    def finish(self, now_ns: int):
        intervals = []
        for track in self.tracks.values():
            end = now_ns if track["quality"] != "observed" else track["last"]
            intervals.append((track["start"], max(end, track["start"]), track["label"]))
        if self.last_utc_ns is not None and self.last_utc_ns < now_ns:
            intervals.append(
                (
                    self.last_utc_ns,
                    now_ns,
                    "HVAC telemetry unobserved (after last observation)",
                )
            )
        self.journal.commit([], intervals, {"last_utc_ns": now_ns, "tracks": {}}, critical=True)
        self.tracks.clear()
