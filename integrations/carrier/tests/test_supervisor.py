import asyncio
import io
import json
import os
import signal
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import patch

from test_store import rows

from k4na_carrier.api import Reading
from k4na_carrier.cli import Refused, Stopped, duration_seconds, parser, rf_command, supervise
from k4na_carrier.store import Journal, StorageFull

ENTRY = Path(__file__).resolve().parents[1] / "run.py"

FAKE_RF = """#!INTERPRETER
import json, os, signal, sys, time, tomllib
from pathlib import Path
args = sys.argv[1:]
if args[0] == "import-marks":
    raise SystemExit(0)
config_path = Path(args[args.index("--config") + 1])
config = tomllib.loads(config_path.read_text())
data = Path(config["storage"]["root"])
events = config_path.parent / "events.jsonl"
with events.open("a") as f:
    has_credentials = any(k.startswith("CARRIER_") for k in os.environ)
    f.write(json.dumps({"args":args, "carrier_env":has_credentials}) + "\\n")
if args[0] == "check":
    raise SystemExit(0)
data.mkdir(exist_ok=True)
(data / "K4NA-DATA-ROOT").write_text("synthetic RF root")
(data / "running").touch()
def stop(*_):
    (data / "stopped").touch()
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
time.sleep(config.get("fake", {}).get("seconds", 0.25))
(data / "completed").touch()
raise SystemExit(config.get("fake", {}).get("exit", 0))
"""


class FakeClient:
    def __init__(self, *_):
        self.calls = 0
        self.closed = False

    async def read(self, now):
        self.calls += 1
        return [Reading("unit-1", now, {"odu": {"opstat": "35"}}, "observed")]

    async def close(self):
        self.closed = True


class SupervisorTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="k4na supervisor ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.data = self.root / "data"
        self.config = self.root / "station.toml"
        self.config.write_text(
            f"[storage]\nroot={json.dumps(str(self.data))}\nmin_free_bytes=0\n"
            "[fake]\nseconds=0.25\nexit=0\n"
        )
        self.executable = self.root / "fake-rf"
        self.executable.write_text(FAKE_RF.replace("INTERPRETER", sys.executable))
        self.executable.chmod(0o700)
        self.telemetry = self.root / "telemetry"
        self.args = parser().parse_args(
            [
                "record",
                "--config",
                str(self.config),
                "--k4na",
                str(self.executable),
                "--telemetry-dir",
                str(self.telemetry),
                "--duration",
                "1s",
            ]
        )
        self.args.poll_seconds = 0.02
        self.args.timeout_seconds = 0.1
        self.output = io.StringIO()

    async def run_session(self, factory=FakeClient, stop=None):
        with (
            patch.dict(
                os.environ, {"CARRIER_USERNAME": "synthetic-user", "CARRIER_PASSWORD": "SENSITIVE"}
            ),
            redirect_stderr(self.output),
        ):
            return await supervise(
                self.args, stop or asyncio.Event(), factory, minimum_poll_seconds=0
            )

    def test_command_preserves_explicit_survey_consent(self):
        command = rf_command(self.args)
        self.assertEqual(command[1], "capture")
        self.assertNotIn("--allow-retune", command)
        self.args.mode = "survey"
        with self.assertRaises(Refused):
            rf_command(self.args)
        self.args.allow_retune = True
        self.assertIn("--allow-retune", rf_command(self.args))
        self.assertEqual(duration_seconds("1h30m"), 5400)

    async def test_good_session_stops_helper_when_rf_ends_and_strips_credentials(self):
        client = FakeClient()
        self.assertEqual(await self.run_session(lambda *_: client), 0)
        self.assertTrue(client.closed)
        self.assertTrue((self.data / "completed").exists())
        events = [
            json.loads(line) for line in (self.root / "events.jsonl").read_text().splitlines()
        ]
        self.assertEqual([event["args"][0] for event in events], ["check", "capture"])
        self.assertTrue(all(not event["carrier_env"] for event in events))
        self.assertNotIn("SENSITIVE", self.output.getvalue())
        self.assertGreater(len(rows(self.telemetry, "samples")), 0)

    async def test_rf_exit_cancels_pending_cloud_request(self):
        class BlockingAfterStartup(FakeClient):
            async def read(self, now):
                if self.calls:
                    await asyncio.sleep(30)
                return await super().read(now)

        client = BlockingAfterStartup()
        self.args.timeout_seconds = 10
        async with asyncio.timeout(2):
            self.assertEqual(await self.run_session(lambda *_: client), 0)
        self.assertTrue(client.closed)
        self.assertTrue((self.data / "completed").exists())

    async def test_startup_failure_does_not_start_rf_and_never_logs_exception_payload(self):
        class Bad(FakeClient):
            async def read(self, now):
                raise RuntimeError("SENSITIVE and private account response")

        client = Bad()
        with self.assertRaises(Refused):
            await self.run_session(lambda *_: client)
        self.assertFalse(self.data.exists())
        self.assertTrue(client.closed)
        self.assertNotIn("SENSITIVE", self.output.getvalue())

    async def test_stale_initial_reading_refuses_startup(self):
        class Stale(FakeClient):
            async def read(self, now):
                return [Reading("unit-1", now, {}, "stale source timestamp")]

        with self.assertRaises(Refused):
            await self.run_session(Stale)
        self.assertFalse(self.data.exists())

    async def test_request_timeout_refuses_startup_and_closes_client(self):
        class Slow(FakeClient):
            async def read(self, now):
                await asyncio.sleep(30)

        client = Slow()
        with self.assertRaises(Refused):
            await self.run_session(lambda *_: client)
        self.assertTrue(client.closed)
        self.assertFalse(self.data.exists())

    async def test_cloud_outage_retries_and_does_not_stop_rf(self):
        class Flaky(FakeClient):
            async def read(self, now):
                self.calls += 1
                if self.calls == 2:
                    raise OSError("SENSITIVE network failure")
                return [Reading("unit-1", now, {"odu": {"opstat": "off"}}, "observed")]

        client = Flaky()
        self.assertEqual(await self.run_session(lambda *_: client), 0)
        self.assertGreater(client.calls, 2)
        self.assertTrue((self.data / "completed").exists())
        self.assertTrue(any("unknown" in row[3] for row in rows(self.telemetry, "intervals")))
        self.assertNotIn("SENSITIVE", self.output.getvalue())

    async def test_storage_failure_does_not_stop_rf_and_closes_known_coverage(self):
        commit = Journal.commit
        calls = 0

        def limited(journal, *args, **kwargs):
            nonlocal calls
            calls += 1
            if calls > 1 and not kwargs.get("critical"):
                raise StorageFull("synthetic cap")
            return commit(journal, *args, **kwargs)

        with patch.object(Journal, "commit", limited):
            self.assertEqual(await self.run_session(), 0)
        self.assertTrue((self.data / "completed").exists())
        self.assertIn("logging stopped", self.output.getvalue())
        self.assertTrue(any("unobserved" in row[3] for row in rows(self.telemetry, "intervals")))

    async def test_stop_terminates_child_gracefully(self):
        self.config.write_text(self.config.read_text().replace("seconds=0.25", "seconds=30"))
        stop = asyncio.Event()

        async def interrupt():
            # The readiness signal comes from a separate executable's file.
            while not (self.data / "running").exists():  # noqa: ASYNC110
                await asyncio.sleep(0.01)
            stop.set()

        interrupt_task = asyncio.create_task(interrupt())
        try:
            async with asyncio.timeout(5):
                self.assertEqual(await self.run_session(stop=stop), 0)
        finally:
            interrupt_task.cancel()
            await asyncio.gather(interrupt_task, return_exceptions=True)
        self.assertTrue((self.data / "stopped").exists())
        self.assertFalse((self.data / "completed").exists())

    async def test_stop_during_authentication_does_not_start_rf(self):
        stop = asyncio.Event()

        class Interrupt(FakeClient):
            async def read(self, now):
                stop.set()
                await asyncio.sleep(30)

        client = Interrupt()
        with self.assertRaises(Stopped):
            await self.run_session(lambda *_: client, stop)
        self.assertFalse(self.data.exists())
        self.assertTrue(client.closed)

    async def test_rf_exit_code_is_preserved(self):
        self.config.write_text(self.config.read_text().replace("exit=0", "exit=3"))
        self.assertEqual(await self.run_session(), 3)

    async def test_explicit_rf_only_mode_never_constructs_cloud_client(self):
        self.args.without_hvac = True

        def forbidden(*_):
            self.fail("RF-only mode must not contact Carrier")

        self.assertEqual(await self.run_session(forbidden), 0)
        self.assertTrue(any("disabled" in row[3] for row in rows(self.telemetry, "intervals")))

    async def test_real_sigterm_reaches_supervisor_and_child(self):
        self.config.write_text(self.config.read_text().replace("seconds=0.25", "seconds=30"))
        process = await asyncio.create_subprocess_exec(
            sys.executable,
            str(ENTRY),
            "record",
            "--without-hvac",
            "--config",
            str(self.config),
            "--k4na",
            str(self.executable),
            "--telemetry-dir",
            str(self.telemetry),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            async with asyncio.timeout(5):
                while not (self.data / "running").exists():
                    if process.returncode is not None:
                        self.fail("supervisor exited before fake RF started")
                    await asyncio.sleep(0.01)
                process.send_signal(signal.SIGTERM)
                self.assertEqual(await process.wait(), 0)
            self.assertTrue((self.data / "stopped").exists())
        finally:
            if process.returncode is None:
                process.kill()
                await process.wait()
