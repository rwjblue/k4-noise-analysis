"""Versioned, capped auxiliary shards and restartable, deduplicated mark import.

No automatic pruning: reaching the auxiliary cap stops HVAC logging, not RF.
Only derived import bookkeeping changes in completed shards; samples do not.
"""

import asyncio
import csv
import fcntl
import hashlib
import json
import os
import shutil
import sqlite3
import time
import uuid
from contextlib import closing
from datetime import UTC, datetime
from pathlib import Path

APPLICATION_ID = 0x4B344856
SCHEMA = 1
RESERVE = 256 * 1024


class StorageFull(Exception):
    pass


class JournalError(Exception):
    pass


def child_env() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if not k.startswith("CARRIER_")}


def iso(ns: int) -> str:
    seconds, fraction = divmod(ns, 1_000_000_000)
    return datetime.fromtimestamp(seconds, UTC).strftime("%Y-%m-%dT%H:%M:%S") + (
        f".{fraction:09d}Z"
    )


def checked_connection(path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(f"{path.resolve().as_uri()}?mode=ro", uri=True)
    if (
        connection.execute("PRAGMA application_id").fetchone()[0] != APPLICATION_ID
        or connection.execute("PRAGMA user_version").fetchone()[0] != SCHEMA
    ):
        connection.close()
        raise ValueError("unsupported Carrier telemetry shard")
    return connection


class Journal:
    def __init__(
        self,
        directory: Path,
        max_bytes: int = 64 * 1024 * 1024,
        shard_bytes: int = 4 * 1024 * 1024,
        min_free_bytes: int = 1024 * 1024 * 1024,
        data_root: Path | None = None,
        *,
        start_shard: bool = True,
    ):
        if max_bytes < 2 * RESERVE or shard_bytes < RESERVE or shard_bytes > max_bytes:
            raise JournalError("invalid telemetry byte budgets")
        if not start_shard and not directory.is_dir():
            raise JournalError("no existing Carrier telemetry directory")
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.directory = directory
        marker = directory / "K4NA-CARRIER-1"
        identity = (
            hashlib.sha256(str(data_root.resolve()).encode()).hexdigest() if data_root else ""
        )
        expected = "k4na-carrier/1; private auxiliary telemetry, not RF shards\n" + identity
        if not marker.exists() and any(directory.iterdir()):
            raise JournalError("telemetry directory is not empty and has no format marker")
        if marker.exists() and marker.read_text() != expected:
            raise JournalError(
                "telemetry directory belongs to another data root or format; choose --telemetry-dir"
            )
        if not marker.exists():
            marker.write_text(expected)
        self.lock = (directory / ".lock").open("a")
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            self.lock.close()
            raise JournalError("another Carrier logger owns this telemetry directory") from None
        self.max_bytes = max_bytes
        self.shard_bytes = shard_bytes
        self.min_free_bytes = min_free_bytes
        self.connection: sqlite3.Connection | None = None
        self.path: Path | None = None
        self.hour: int | None = None
        self.checkpoint = {}
        self.session_id = uuid.uuid4().hex
        try:
            paths = self.paths()
            # Under the exclusive logger lock, let SQLite roll back any hot
            # journal from a hard-killed writer before read-only inspection.
            # This recovers incomplete transactions, never rewrites samples.
            for path in paths:
                with closing(sqlite3.connect(path)) as recovery:
                    if (
                        recovery.execute("PRAGMA application_id").fetchone()[0] != APPLICATION_ID
                        or recovery.execute("PRAGMA user_version").fetchone()[0] != SCHEMA
                    ):
                        raise ValueError("unsupported Carrier telemetry shard")
                    recovery.execute("SELECT value FROM meta LIMIT 1").fetchone()
            if paths:
                with closing(checked_connection(paths[-1])) as prior:
                    self.checkpoint = json.loads(
                        prior.execute("SELECT value FROM meta WHERE key='checkpoint'").fetchone()[0]
                    )
            self.previous = self.checkpoint.copy()
            if start_shard:
                self._rotate(time.time_ns())
        except BaseException:
            self.close()
            raise

    def paths(self):
        return sorted(self.directory.glob("carrier-*.sqlite"))

    def usage(self) -> int:
        return sum(p.stat().st_size for p in self.directory.iterdir() if p.is_file())

    def _admit(self, needed: int, critical: bool = False):
        limit = self.max_bytes if critical else self.max_bytes - RESERVE
        if self.usage() + needed > limit:
            raise StorageFull("HVAC telemetry byte cap reached; archive logs or raise --max-bytes")
        if shutil.disk_usage(self.directory).free < self.min_free_bytes + needed:
            raise StorageFull("HVAC telemetry free-space reserve reached")

    def _rotate(self, now_ns: int):
        self._admit(64 * 1024)
        if self.connection is not None:
            self.connection.close()
            self.connection = None
        paths = self.paths()
        sequence = int(paths[-1].name.split("-")[1]) + 1 if paths else 0
        path = self.directory / f"carrier-{sequence:020d}-{uuid.uuid4().hex}.sqlite"
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.close(fd)
        connection = sqlite3.connect(path)
        self.connection = connection
        self.path = path
        self.hour = now_ns // 3_600_000_000_000
        connection.executescript(f"""
            PRAGMA application_id={APPLICATION_ID};
            PRAGMA user_version={SCHEMA};
            PRAGMA journal_mode=DELETE;
            PRAGMA synchronous=FULL;
            CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE samples(
                id INTEGER PRIMARY KEY, observed_utc_ns INTEGER NOT NULL,
                elapsed_ns INTEGER NOT NULL, unit TEXT NOT NULL,
                reported_utc_ns INTEGER, quality TEXT NOT NULL, values_json TEXT NOT NULL
            );
            CREATE TABLE intervals(
                id INTEGER PRIMARY KEY, start_utc_ns INTEGER NOT NULL,
                end_utc_ns INTEGER NOT NULL, text TEXT NOT NULL,
                imported INTEGER NOT NULL DEFAULT 0
            );
        """)
        with connection:
            connection.executemany(
                "INSERT INTO meta VALUES (?, ?)",
                [
                    ("format", "k4na-carrier/1"),
                    ("client", "carrier-api/3.6.2"),
                    ("session", self.session_id),
                    ("created_utc_ns", str(now_ns)),
                    ("checkpoint", json.dumps(self.checkpoint)),
                ],
            )

    def commit(self, samples: list, intervals: list, checkpoint: dict, *, critical=False):
        serialized = json.dumps([samples, intervals, checkpoint], allow_nan=False)
        if len(serialized.encode()) > 128 * 1024:
            raise ValueError("telemetry batch exceeds 128 KiB")
        self._admit(len(serialized.encode()) * 2 + 64 * 1024, critical)
        now = time.time_ns()
        if not critical and (
            self.path.stat().st_size >= self.shard_bytes or self.hour != now // 3_600_000_000_000
        ):
            self._rotate(now)
        with self.connection:
            for sample in samples:
                self.connection.execute(
                    "INSERT INTO samples VALUES (NULL, ?, ?, ?, ?, ?, ?)",
                    (
                        sample["observed_utc_ns"],
                        sample["elapsed_ns"],
                        sample["unit"],
                        sample["reported_utc_ns"],
                        sample["quality"],
                        json.dumps(sample["values"], allow_nan=False),
                    ),
                )
            for start, end, text in intervals:
                if end < start or not 0 < len(text) <= 500:
                    raise ValueError("invalid derived interval")
                self.connection.execute(
                    "INSERT INTO intervals VALUES (NULL, ?, ?, ?, 0)", (start, end, text)
                )
            self.connection.execute(
                "UPDATE meta SET value=? WHERE key='checkpoint'", (json.dumps(checkpoint),)
            )
        self.checkpoint = checkpoint

    async def import_marks(self, executable: str, data: Path) -> int:
        if not (data / "K4NA-DATA-ROOT").is_file():
            return 0  # The RF process, never this helper, creates the data root.
        notes = data / "annotations.jsonl"
        if notes.exists() and notes.stat().st_size > 16 * 1024 * 1024:
            raise StorageFull("annotations exceed 16 MiB; refusing unattended additions")
        imported = 0
        csv_path = self.directory / "carrier-marks.csv"
        for path in self.paths():
            while True:
                with closing(checked_connection(path)) as source:
                    rows = source.execute(
                        "SELECT id,start_utc_ns,end_utc_ns,text FROM intervals "
                        "WHERE imported=0 ORDER BY id LIMIT 128"
                    ).fetchall()
                if not rows:
                    break
                self._admit(128 * 1024, critical=True)
                fd = os.open(csv_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
                with os.fdopen(fd, "w", newline="") as f:
                    writer = csv.writer(f)
                    writer.writerow(["start", "end", "label"])
                    writer.writerows((iso(start), iso(end), text) for _, start, end, text in rows)
                    f.flush()
                    os.fsync(f.fileno())
                process = await asyncio.create_subprocess_exec(
                    executable,
                    "import-marks",
                    "--data",
                    str(data),
                    str(csv_path),
                    "--time",
                    "start",
                    "--end",
                    "end",
                    "--text",
                    "label",
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                    env=child_env(),
                    start_new_session=True,
                )
                try:
                    async with asyncio.timeout(30):
                        result = await process.wait()
                except BaseException:
                    if process.returncode is None:
                        process.terminate()
                    await process.wait()
                    raise
                if result != 0:
                    raise RuntimeError("mark import failed; intervals remain queued for retry")
                with closing(sqlite3.connect(path)) as bookkeeping, bookkeeping:
                    bookkeeping.executemany(
                        "UPDATE intervals SET imported=1 WHERE id=?", ((row[0],) for row in rows)
                    )
                imported += len(rows)
        return imported

    def close(self):
        if self.connection is not None:
            self.connection.close()
            self.connection = None
        if not self.lock.closed:
            fcntl.flock(self.lock, fcntl.LOCK_UN)
            self.lock.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
