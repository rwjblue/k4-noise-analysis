import asyncio
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import patch

from k4na_carrier.api import Reading
from k4na_carrier.recorder import Recorder
from k4na_carrier.store import Journal, JournalError, StorageFull, checked_connection

SECOND = 1_000_000_000
NOW = 1_800_000_000 * SECOND
ROOT = Path(__file__).resolve().parents[3]


def reading(value="off", quality="observed"):
    return Reading("unit-1", NOW, {"odu": {"opstat": value}, "idu": {"opstat": "fan"}}, quality)


def rows(directory, table):
    out = []
    for path in sorted(directory.glob("carrier-*.sqlite")):
        with closing(checked_connection(path)) as conn:
            out.extend(conn.execute(f"SELECT * FROM {table}").fetchall())
    return out


class StoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / "telemetry"

    def journal(self, **kwargs):
        return Journal(self.directory, min_free_bytes=0, **kwargs)

    def test_transition_unknown_reconnect_and_numeric_values(self):
        with self.journal() as journal:
            recorder = Recorder(journal, max_gap_seconds=100)
            recorder.tick([reading("35")], NOW, 0)
            recorder.tick([reading("60")], NOW + 30 * SECOND, 30 * SECOND)
            self.assertEqual(len(rows(self.directory, "intervals")), 0)
            recorder.unavailable("cloud request failed", NOW + 60 * SECOND, 60 * SECOND)
            recorder.tick([reading("off")], NOW + 90 * SECOND, 90 * SECOND)
            recorder.finish(NOW + 100 * SECOND)
            intervals = rows(self.directory, "intervals")
            self.assertTrue(any("unknown" in row[3] for row in intervals))
            unknown = next(row for row in intervals if "cloud request failed" in row[3])
            self.assertEqual(unknown[1:3], (NOW + 30 * SECOND, NOW + 90 * SECOND))
            values = [json.loads(row[-1]) for row in rows(self.directory, "samples")]
            self.assertEqual(values[0]["odu"]["opstat"], "35")
            self.assertEqual(values[1]["odu"]["opstat"], "60")
            self.assertTrue(any("ODU off" in row[3] for row in intervals))

    def test_reported_change_leaves_transition_uncertainty(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            recorder.tick([reading("off")], NOW, 0)
            recorder.tick([reading("heat1")], NOW + 30 * SECOND, 30 * SECOND)
            intervals = rows(self.directory, "intervals")
            transition = next(row for row in intervals if "transition" in row[3])
            self.assertEqual(transition[1:3], (NOW, NOW + 30 * SECOND))

    def test_sampling_pause_and_backwards_clock_never_bridge_observed_state(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 60)
            recorder.tick([reading()], NOW, 0)
            recorder.tick([reading()], NOW + 120 * SECOND, 120 * SECOND)
            recorder.tick([reading()], NOW - 10 * SECOND, 130 * SECOND)
            intervals = rows(self.directory, "intervals")
            self.assertTrue(any("sampling pause" in row[3] for row in intervals))
            self.assertTrue(any("backwards" in row[3] for row in intervals))
            self.assertTrue(all(row[1] <= row[2] for row in intervals))

    def test_checkpoint_recovers_crash_and_records_nonrunning_gap(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            recorder.tick([reading("heat1")], NOW, 0)
            recorder.tick([reading("heat1")], NOW + 30 * SECOND, 30 * SECOND)
            # No finish: simulate abrupt death after the committed checkpoint.
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            recorder.tick([reading("off")], NOW + 600 * SECOND, 0)
            intervals = rows(self.directory, "intervals")
            known = next(row for row in intervals if "heat1" in row[3])
            missing = next(row for row in intervals if "not running" in row[3])
            self.assertEqual(known[1:3], (NOW, NOW + 30 * SECOND))
            self.assertEqual(missing[1:3], (NOW + 30 * SECOND, NOW + 600 * SECOND))

    def test_sqlite_hot_journal_is_rolled_back_after_abrupt_writer_exit(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            recorder.tick([reading("35")], NOW, 0)
            path = journal.path
        program = """
import os, sqlite3, sys
connection = sqlite3.connect(sys.argv[1])
connection.execute('PRAGMA cache_size=1')
connection.execute('BEGIN IMMEDIATE')
connection.execute('UPDATE samples SET values_json=?', ('x' * 400000,))
os._exit(0)
"""
        subprocess.run([sys.executable, "-c", program, str(path)], check=True, timeout=5)
        self.assertTrue(Path(str(path) + "-journal").exists())
        with self.journal():
            samples = rows(self.directory, "samples")
            self.assertEqual(json.loads(samples[0][-1])["odu"]["opstat"], "35")

    def test_live_intervals_are_chunked_not_reimported_with_growing_ends(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100, chunk_seconds=60)
            for second in (0, 30, 60, 90):
                recorder.tick([reading()], NOW + second * SECOND, second * SECOND)
            recorder.finish(NOW + 90 * SECOND)
            spans = [(row[1], row[2]) for row in rows(self.directory, "intervals")]
            self.assertEqual(
                spans, [(NOW, NOW + 60 * SECOND), (NOW + 60 * SECOND, NOW + 90 * SECOND)]
            )

    def test_duplicate_writer_foreign_directory_and_dataset_rejected(self):
        with self.journal():
            with self.assertRaises(JournalError):
                self.journal()
        with self.assertRaises(JournalError):
            Journal(self.directory, min_free_bytes=0, data_root=Path("another-data-root"))
        other = Path(self.temp.name) / "foreign"
        other.mkdir()
        (other / "notes.txt").write_text("keep me")
        with self.assertRaises(JournalError):
            Journal(other, min_free_bytes=0)
        self.assertEqual((other / "notes.txt").read_text(), "keep me")

    def test_byte_cap_bounds_source_shards_without_deleting_them(self):
        with self.journal(max_bytes=1024 * 1024, shard_bytes=256 * 1024) as journal:
            sample = {
                "observed_utc_ns": NOW,
                "elapsed_ns": 0,
                "unit": "unit-1",
                "reported_utc_ns": NOW,
                "quality": "observed",
                "values": {"test": "x" * 16000},
            }
            written = 0
            with self.assertRaises(StorageFull):
                for _ in range(200):
                    journal.commit([sample], [], {})
                    written += 1
            self.assertGreater(written, 1)
            self.assertLessEqual(journal.usage(), journal.max_bytes)
            self.assertGreater(len(journal.paths()), 1)
            self.assertEqual(len(rows(self.directory, "samples")), written)
            if os.name == "posix":
                self.assertEqual(journal.path.stat().st_mode & 0o777, 0o600)

    def test_free_space_reserve_refuses_before_writing_a_shard(self):
        with patch("k4na_carrier.store.shutil.disk_usage") as disk:
            disk.return_value.free = 0
            with self.assertRaises(StorageFull):
                Journal(self.directory, min_free_bytes=1)
        self.assertEqual(list(self.directory.glob("*.sqlite")), [])

    def test_offline_import_needs_no_credentials_and_creates_no_source_shards(self):
        from k4na_carrier.cli import parser, run

        binary = ROOT / "target/debug/k4na"
        if not binary.is_file():
            self.skipTest("build k4na-cli to exercise the real marks importer")
        data = Path(self.temp.name) / "measurements"
        data.mkdir()
        (data / "K4NA-DATA-ROOT").touch()
        with Journal(self.directory, min_free_bytes=0, data_root=data) as journal:
            journal.commit([], [(NOW, NOW + SECOND, "HVAC offline import")], {})
            paths = journal.paths()
        args = parser().parse_args(
            [
                "import",
                "--data",
                str(data),
                "--telemetry-dir",
                str(self.directory),
                "--k4na",
                str(binary),
                "--min-free-bytes",
                "0",
            ]
        )
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(asyncio.run(run(args)), 0)
        self.assertEqual(sorted(self.directory.glob("carrier-*.sqlite")), paths)
        self.assertIn("HVAC offline import", (data / "annotations.jsonl").read_text())

    def test_future_schema_is_rejected(self):
        with self.journal() as journal:
            path = journal.path
        with sqlite3.connect(path) as conn:
            conn.execute("PRAGMA user_version=999")
        with self.assertRaises(ValueError):
            self.journal()

    def test_real_importer_deduplicates_after_retry_and_preserves_nanoseconds(self):
        binary = ROOT / "target/debug/k4na"
        if not binary.is_file():
            self.skipTest("build k4na-cli to exercise the real offline marks importer")
        data = Path(self.temp.name) / "measurements"
        data.mkdir()
        (data / "K4NA-DATA-ROOT").write_text("synthetic test root")
        with self.journal() as journal:
            journal.commit([], [(NOW + 123, NOW + SECOND + 456, "HVAC synthetic interval")], {})
            self.assertEqual(asyncio.run(journal.import_marks(str(binary), data)), 1)
            self.assertEqual(asyncio.run(journal.import_marks(str(binary), data)), 0)
            with sqlite3.connect(journal.path) as conn:
                conn.execute("UPDATE intervals SET imported=0")
            self.assertEqual(asyncio.run(journal.import_marks(str(binary), data)), 1)
            annotations = [
                json.loads(line) for line in (data / "annotations.jsonl").read_text().splitlines()
            ]
            self.assertEqual(len(annotations), 1)
            self.assertEqual(annotations[0]["start_utc_ns"], NOW + 123)
            self.assertEqual(annotations[0]["end_utc_ns"], NOW + SECOND + 456)
            self.assertEqual(annotations[0]["source"], "import:carrier-marks.csv")
