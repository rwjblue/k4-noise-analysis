import io
import unittest

from k4na_carrier.api import Reading
from k4na_carrier.display import Changes, Console, equipment

NOW = 1_800_000_000_000_000_000


def reading(output="35", quality="observed", **extra):
    values = {
        "odu": {"type": "synthetic-family", "opstat": output},
        "idu": {"opstat": "heat1", "cfm": "800", "blwrpm": "900"},
        "equipment": {"odutype": "varcaphp", "idutype": "furnace", "idusource": "gas"},
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

    def test_runtime_type_is_not_a_profile_category_or_an_indoor_percentage(self):
        value = reading(equipment={}, odu={"type": "varcaphp", "opstat": "35"})
        self.assertNotIn("%", equipment(value))
        value = reading(idu={"type": "varcaphp", "opstat": "35"})
        self.assertIn("ODU=35% output", equipment(value))
        self.assertIn("IDU=35;", equipment(value))
        self.assertNotIn("IDU=35%", equipment(value))

    def test_percent_requires_known_category_observed_status_and_valid_numeric_range(self):
        for category in (None, "unknown", 35):
            self.assertNotIn("%", equipment(reading(equipment={"odutype": category})))
        for output in (True, False, 10**1000, "NaN", "inf", "-1", "101", "off", "dehumidify"):
            self.assertNotIn("%", equipment(reading(output)))
        self.assertIn("ODU=35% output", equipment(reading(equipment={"odutype": " VarCapHP "})))
        self.assertIn("ODU=0% output", equipment(reading("0")))

    def test_conditioning_changes_are_visible_without_becoming_heat_source_marks(self):
        changes = Changes()
        labels = set()
        for index, state in enumerate(("prep_heat", "pending_heat", "active_heat", "idle")):
            value = reading(
                zones=[{"zone": "zone-1", "enabled": "on", "zoneconditioning": state}],
                conditioning_quality="observed",
            )
            labels.add(value.label)
            self.assertIn(f"zone-1[on]:{state}", equipment(value))
            self.assertEqual(changes.observe(value), "initial" if index == 0 else "change")
        self.assertEqual(len(labels), 1)

    def test_missing_conditioning_is_unknown_and_context_is_terminal_safe(self):
        text = equipment(reading(zones=[], conditioning_quality="missing zones"))
        self.assertIn("conditioning unknown (missing zones)", text)
        value = reading(
            zones=[
                {
                    "zone": "zone-1",
                    "enabled": "on",
                    "zoneconditioning": "active_heat\nINJECTED\x1b[31m",
                    "name": "PRIVATE",
                }
            ],
            conditioning_quality="observed",
        )
        text = equipment(value)
        self.assertNotIn("PRIVATE", text)
        self.assertNotIn("\n", text)
        self.assertNotIn("\x1b", text)

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
