import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


class ConfigurationTests(unittest.TestCase):
    def test_toolchains_and_dotenv_are_mise_configured_and_locked(self):
        with (ROOT / "mise.toml").open("rb") as f:
            config = tomllib.load(f)
        self.assertEqual(config["env"]["_"]["file"], ".env")
        with (ROOT / "mise.lock").open("rb") as f:
            lock = tomllib.load(f)
        for name in ("python", "uv"):
            version = config["tools"][name]
            self.assertTrue(any(tool["version"] == version for tool in lock["tools"][name]))
        with (ROOT / "integrations/carrier/uv.lock").open("rb") as f:
            dependencies = tomllib.load(f)
        packages = {p["name"] for p in dependencies["package"]}
        self.assertIn("carrier-api", packages)
        self.assertIn("ruff", packages)
        ignored = (ROOT / ".gitignore").read_text().splitlines()
        self.assertIn(".env", ignored)
        self.assertIn("!.env.example", ignored)
        self.assertIn("/output/", ignored)

    def test_task_wrappers_use_uv_locked_without_reading_credentials(self):
        for name in ("record", "carrier/log", "carrier/import", "carrier/report"):
            text = (ROOT / "mise/tasks" / name).read_text()
            self.assertIn("uv run --locked", text)
            self.assertNotIn("source ", text)
            self.assertNotIn("pip install", text)
