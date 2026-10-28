"""Exercise the real mise parser with harmless copies of task Usage headers.

Never run a recording task to test its argument handling: even a broken flag
forwarder must not be able to reach the operator's radio or Carrier account.
"""

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from k4na_carrier.cli import Refused, parser, rf_command

ROOT = Path(__file__).resolve().parents[3]
TASKS = {
    "record": "record",
    "carrier/log": "log",
    "carrier/import": "import",
    "carrier/report": "report",
}
PREFIX = "K4NA_USAGE_PROBE="


@unittest.skipUnless(shutil.which("mise"), "mise is required for task-interface tests")
class MiseTaskTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="k4na usage ")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.parsers = parser()._subparsers._group_actions[0].choices
        self.probes = {}
        for task, command in TASKS.items():
            source = ROOT / "mise/tasks" / task
            actions = self.parsers[command]._actions
            names = [a.dest for a in actions if a.dest != "help"]
            # Include files resolve relative to the original task, not the
            # temporary probe's location. All other Usage text stays verbatim.
            header = []
            for line in source.read_text().splitlines():
                if not line.startswith(("#USAGE", "#MISE")):
                    continue
                match = re.fullmatch(r'#USAGE include file="([^"]+)"', line)
                if match:
                    path = (source.parent / match[1]).resolve()
                    line = f"#USAGE include file={json.dumps(str(path))}"
                header.append(line)
            code = (
                "import json, os, sys; "
                f"names={names!r}; "
                "result={'argv':sys.argv[1:], "
                "'parsed':{name:os.environ.get('usage_'+name) for name in names}}; "
                f"print({PREFIX!r}+json.dumps(result))"
            )
            probe = self.directory / task.replace("/", "-")
            probe.write_text(
                "#!/usr/bin/env bash\n"
                + "\n".join(header)
                + "\nset -euo pipefail\n"
                + f'exec {shlex.quote(sys.executable)} -c {shlex.quote(code)} "$@"\n'
            )
            probe.chmod(0o700)
            self.probes[task] = probe

    def invoke(self, task, *args, extra_env=None):
        env = os.environ.copy()
        if extra_env:
            env.update(extra_env)
        result = subprocess.run(
            ["mise", "run", str(self.probes[task]), *args],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            timeout=30,
        )
        payload = None
        for line in result.stdout.splitlines():
            if line.startswith(PREFIX):
                payload = json.loads(line[len(PREFIX) :])
        return result, payload

    def test_all_flags_have_native_help_and_do_not_execute_task_body(self):
        for task, command in TASKS.items():
            with self.subTest(task=task):
                result, payload = self.invoke(task, "--help")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIsNone(payload)
                for action in self.parsers[command]._actions:
                    for option in action.option_strings:
                        self.assertIn(option, result.stdout)
                if task == "record":
                    self.assertIn("capture", result.stdout)
                    self.assertIn("survey", result.stdout)
                self.assertIn("default", result.stdout.lower())

    def test_declared_defaults_match_python_defaults(self):
        for task, command in TASKS.items():
            with self.subTest(task=task):
                result, payload = self.invoke(task)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(payload["argv"], [])
                for action in self.parsers[command]._actions:
                    if action.dest == "help" or action.default is None:
                        continue
                    value = payload["parsed"][action.dest]
                    if isinstance(action.default, bool):
                        # Mise may leave an absent boolean unset; the Usage
                        # convention is ${usage_flag:-false}, not a required env.
                        self.assertEqual(value or "false", str(action.default).lower())
                    elif isinstance(action.default, int | float):
                        self.assertEqual(float(value), action.default)
                    else:
                        self.assertEqual(value, str(action.default))

    def test_direct_flags_preserve_spaces_and_are_forwarded_verbatim(self):
        common = [
            "--duration",
            "1h30m",
            "--k4na",
            "a binary with spaces",
            "--telemetry-dir",
            "a telemetry directory",
            "--poll-seconds",
            "60",
            "--timeout-seconds",
            "15",
            "--max-age-seconds",
            "900",
            "--max-bytes",
            "104857600",
            "--shard-bytes",
            "8388608",
            "--min-free-bytes",
            "1073741824",
        ]
        cases = {
            "record": common
            + [
                "--config",
                "station config.toml",
                "--profile",
                "survey profile.toml",
                "--mode",
                "survey",
                "--allow-retune",
                "--without-hvac",
                "--raw-full-rate",
                "5m",
                "--raw-full-rate-max-bytes",
                "1048576",
            ],
            "carrier/log": common + ["--data", "a measurement directory"],
            "carrier/import": [
                "--data",
                "a measurement directory",
                "--k4na",
                "a binary with spaces",
                "--telemetry-dir",
                "a telemetry directory",
                "--max-bytes",
                "104857600",
                "--min-free-bytes",
                "1073741824",
            ],
        }
        cases["carrier/report"] = ["--telemetry-dir", "a telemetry directory", "--limit", "10"]
        for task, args in cases.items():
            with self.subTest(task=task):
                result, payload = self.invoke(task, *args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(payload["argv"], args)
                # The original wrapper, unlike this harmless probe, must still
                # pass argv to the Python command rather than rebuilding it.
                source = (ROOT / "mise/tasks" / task).read_text()
                self.assertIn(f'run.py {TASKS[task]} "$@"', source)

    def test_unknown_flags_and_invalid_mode_fail_before_script_execution(self):
        for task in TASKS:
            with self.subTest(task=task):
                result, payload = self.invoke(task, "--not-a-real-option")
                self.assertNotEqual(result.returncode, 0)
                self.assertIsNone(payload)
        result, payload = self.invoke("record", "--mode", "invalid")
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(payload)
        self.assertIn("capture", result.stderr)
        self.assertIn("survey", result.stderr)

    def test_usage_environment_cannot_synthesize_retune_consent(self):
        result, payload = self.invoke(
            "record", "--mode", "survey", extra_env={"usage_allow_retune": "true"}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(payload["argv"], ["--mode", "survey"])
        args = parser().parse_args(["record", *payload["argv"]])
        self.assertFalse(args.allow_retune)
        with self.assertRaises(Refused):
            rf_command(args)
