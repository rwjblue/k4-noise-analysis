import asyncio
import hashlib
import io
import os
import sqlite3
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from k4na_carrier.api import Reading
from k4na_carrier.cli import parser, run
from k4na_carrier.recorder import Recorder
from k4na_carrier.report import ReportError, write_report
from k4na_carrier.store import Journal

NOW = 1_800_000_000_000_000_000
SECOND = 1_000_000_000


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / "telemetry"

    def journal(self):
        return Journal(self.directory, min_free_bytes=0)

    def snapshots(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            for i, (output, quality) in enumerate(
                (
                    ("35", "observed"),
                    ("35", "observed"),
                    ("60", "observed"),
                    ("60", "stale source timestamp"),
                    ("off", "observed"),
                )
            ):
                values = {
                    "utcTime": f"synthetic timestamp {i}",
                    "odu": {"type": "synthetic-family", "opstat": output},
                    "idu": {"opstat": "heat1", "cfm": "800"},
                    "equipment": {"odutype": "varcaphp", "idutype": "furnace", "name": "PRIVATE"},
                    "zones": [
                        {
                            "zone": "zone-1",
                            "enabled": "on",
                            "zoneconditioning": "active_heat",
                            "id": "PRIVATE",
                            "name": "PRIVATE",
                        }
                    ],
                    "conditioning_quality": "observed",
                    "profile": {"password": "PRIVATE", "serial": "PRIVATE"},
                }
                recorder.tick(
                    [Reading("unit-1", NOW + i * 30 * SECOND, values, quality)],
                    NOW + i * 30 * SECOND,
                    i * 30 * SECOND,
                )
            recorder.finish(NOW + 121 * SECOND)

    def render(self, limit=25):
        stream = io.StringIO()
        write_report(self.directory, limit, stream)
        return stream.getvalue()

    def fingerprint(self):
        return {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in self.directory.iterdir()
            if path.is_file()
        }

    def test_snapshot_events_capacity_unknown_and_intervals(self):
        self.snapshots()
        text = self.render()
        self.assertIn("Samples: 5 | Observed: 4 | Unknown: 1", text)
        self.assertIn("Equipment snapshot changes: 1 | Timeline events: 4", text)
        for kind in ("initial", "change", "unknown", "resumed"):
            self.assertIn(f"[{kind}]", text)
        self.assertIn("ODU=60% output", text)
        self.assertIn("reported output range=35..60%", text)
        self.assertIn("odutype:varcaphp", text)
        self.assertIn("zone-1[on]:active_heat", text)
        self.assertIn("pending import", text)
        self.assertNotIn("PRIVATE", text)
        self.assertIn("not exact hardware switching times", text)

    def test_old_version_one_samples_remain_readable_without_invented_percent_units(self):
        with self.journal() as journal:
            Recorder(journal, 100).tick(
                [Reading("unit-1", NOW, {"odu": {"type": "varcaphp", "opstat": "35"}}, "observed")],
                NOW,
                0,
            )
        before = self.fingerprint()
        text = self.render()
        self.assertIn("ODU=35;", text)
        self.assertNotIn("ODU=35%", text)
        self.assertNotIn("reported output range=", text)
        self.assertEqual(self.fingerprint(), before)

    def test_context_transitions_are_reported_but_not_imported_as_new_source_claims(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            for index, state in enumerate(("prep_heat", "pending_heat", "active_heat", "idle")):
                values = {
                    "odu": {"opstat": "off"},
                    "idu": {"opstat": "off"},
                    "zones": [{"zone": "zone-1", "enabled": "on", "zoneconditioning": state}],
                    "conditioning_quality": "observed",
                }
                recorder.tick(
                    [Reading("unit-1", NOW, values, "observed")],
                    NOW + index * 30 * SECOND,
                    index * 30 * SECOND,
                )
            recorder.finish(NOW + 90 * SECOND)
        text = self.render()
        self.assertIn("Equipment snapshot changes: 3 | Timeline events: 4", text)
        for state in ("prep_heat", "pending_heat", "active_heat", "idle"):
            self.assertIn(f"zone-1[on]:{state}", text)
        self.assertNotIn("state transition between polls", text)
        self.assertIn("does not identify the heat source", text)

    def test_report_changes_no_files_and_needs_no_credentials_or_writer(self):
        self.snapshots()
        before = self.fingerprint()
        args = parser().parse_args(["report", "--telemetry-dir", str(self.directory)])
        with (
            patch.dict(os.environ, {}, clear=True),
            patch("k4na_carrier.cli.Journal", side_effect=AssertionError("no writer")),
            patch("k4na_carrier.cli.CarrierClient", side_effect=AssertionError("no cloud")),
            patch("sys.stdout", new_callable=io.StringIO) as out,
        ):
            self.assertEqual(asyncio.run(run(args)), 0)
        self.assertIn("Carrier HVAC event report", out.getvalue())
        self.assertEqual(self.fingerprint(), before)

    def test_recent_output_is_bounded_but_totals_cover_all_samples(self):
        self.snapshots()
        text = self.render(limit=1)
        self.assertIn("Timeline (latest 1 of 4 events", text)
        self.assertNotIn("[initial]", text)
        self.assertIn("[resumed]", text)
        self.assertIn("Samples: 5", text)

    def test_session_scoped_aliases_are_not_compared_as_one_device(self):
        for i, value in enumerate(("35", "60")):
            with self.journal() as journal:
                recorder = Recorder(journal, 100)
                recorder.tick(
                    [Reading("unit-1", NOW, {"odu": {"opstat": value}}, "observed")],
                    NOW + i * 30 * SECOND,
                    0,
                )
                recorder.finish(NOW + i * 30 * SECOND)
        text = self.render()
        self.assertIn("Sessions: 2 | Unit/session pairs: 2", text)
        self.assertIn("Equipment snapshot changes: 0 | Timeline events: 2", text)

    def test_rotation_does_not_create_a_false_initial_event(self):
        with self.journal() as journal:
            recorder = Recorder(journal, 100)
            value = Reading("unit-1", NOW, {"odu": {"opstat": "off"}}, "observed")
            recorder.tick([value], NOW, 0)
            journal._rotate(time.time_ns())
            recorder.tick([value], NOW + 30 * SECOND, 30 * SECOND)
            recorder.finish(NOW + 30 * SECOND)
        text = self.render()
        self.assertIn("Shards: 2 | Sessions: 1", text)
        self.assertIn("Equipment snapshot changes: 0 | Timeline events: 1", text)

    def test_empty_shard_and_missing_directory_are_not_off(self):
        with self.assertRaises(ReportError):
            self.render()
        self.assertFalse(self.directory.exists())
        with self.journal():
            pass
        text = self.render()
        self.assertIn("no observations", text)
        self.assertNotIn("ODU=off", text)
        for limit in (0, 1001):
            with self.assertRaises(ReportError):
                self.render(limit)

    def test_damaged_json_and_future_schema_are_rejected(self):
        self.snapshots()
        path = next(self.directory.glob("carrier-*.sqlite"))
        with sqlite3.connect(path) as db:
            original = db.execute("SELECT values_json FROM samples WHERE id=1").fetchone()[0]
            db.execute("UPDATE samples SET values_json='{' WHERE id=1")
        with self.assertRaises(ReportError):
            self.render()
        with sqlite3.connect(path) as db:
            db.execute("UPDATE samples SET values_json=? WHERE id=1", (original,))
            db.execute("PRAGMA user_version=999")
        with self.assertRaises(ReportError):
            self.render()

    def test_oversized_sqlite_cells_are_rejected_before_materializing_json(self):
        self.snapshots()
        path = next(self.directory.glob("carrier-*.sqlite"))
        with sqlite3.connect(path) as db:
            db.execute("UPDATE samples SET values_json=? WHERE id=1", ("x" * (512 * 1024),))
        with self.assertRaises(ReportError):
            self.render()

    def test_no_terminal_escape_or_multiline_injection_from_stored_values(self):
        with self.journal() as journal:
            journal.commit(
                [
                    {
                        "observed_utc_ns": NOW,
                        "elapsed_ns": 0,
                        "unit": "unit-1",
                        "reported_utc_ns": NOW,
                        "quality": "observed",
                        "values": {"odu": {"opstat": "\x1b[31mheat\nINJECTED"}},
                    }
                ],
                [],
                {},
            )
        text = self.render()
        self.assertNotIn("\x1b", text)
        self.assertNotIn("heat\nINJECTED", text)
