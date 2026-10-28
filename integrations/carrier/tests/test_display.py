import io
import unittest

from k4na_carrier.api import Reading
from k4na_carrier.display import Changes, Console, equipment

NOW = 1_800_000_000_000_000_000


def reading(output="35", quality="observed", **extra):
    values = {
        "odu": {"type": "varcaphp", "opstat": output},
        "idu": {"opstat": "heat1", "cfm": "800", "blwrpm": "900"},
    }
    values.update(extra)
    return Reading("unit-1", NOW, values, quality)


class DisplayTests(unittest.TestCase):
    def test_equipment_fields_and_capacity_are_visible_without_account_payloads(self):
        text = equipment(reading(profile={"password": "PRIVATE", "name": "PRIVATE"}))
        self.assertIn("ODU=35% output", text)
        self.assertIn("IDU=heat1", text)
        self.assertIn("airflow=800 cfm", text)
        self.assertIn("blower=900 rpm", text)
        self.assertNotIn("PRIVATE", text)

    def test_only_recognized_variable_types_get_percent_units(self):
        text = equipment(Reading("unit-1", NOW, {"odu": {"opstat": "35"}}, "observed"))
        self.assertIn("ODU=35", text)
        self.assertNotIn("%", text)
        self.assertIn("IDU=unknown", text)

    def test_initial_change_cache_refresh_unknown_and_resume(self):
        changes = Changes()
        self.assertEqual(changes.observe(reading()), "initial")
        self.assertEqual(changes.observe(reading(utcTime="new source timestamp")), "unchanged")
        self.assertEqual(changes.observe(reading("60")), "change")
        self.assertEqual(changes.observe(reading("60", "stale source timestamp")), "unknown")
        self.assertEqual(changes.observe(reading("60")), "resumed")

    def test_unknown_never_claims_current_equipment_state(self):
        text = equipment(reading("60", "stale source timestamp"))
        self.assertIn("unknown", text)
        self.assertNotIn("ODU=", text)

    def test_console_includes_timing_counts_intervals_and_is_single_line_safe(self):
        stream = io.StringIO()
        console = Console(stream)
        console.observations([reading()], NOW + 5_000_000_000)
        console.observations([reading("60")], NOW + 6_000_000_000)
        console.unavailable("cloud request failed", NOW + 7_000_000_000)
        console.intervals([(NOW, NOW + 6_000_000_000, "HVAC synthetic interval")])
        console.summary()
        text = stream.getvalue()
        self.assertIn("[initial]", text)
        self.assertIn("[change]", text)
        self.assertIn("[unknown]", text)
        self.assertIn("source age=5.0s", text)
        self.assertIn("3 saved sample(s), 2 observed, 1 unknown, 1 equipment", text)
        unsafe = equipment(reading("\x1b[31mheat\nINJECTED"))
        self.assertNotIn("\x1b", unsafe)
        self.assertNotIn("\n", unsafe)
