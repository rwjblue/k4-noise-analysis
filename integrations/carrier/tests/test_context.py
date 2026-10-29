import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from k4na_carrier.api import Reading, equipment_values
from k4na_carrier.context import MAX_ZONES, ZoneAliases, capture_context, retained_context
from k4na_carrier.store import Journal

NOW = 1_800_000_000_000_000_000


def zone(identity, state="active_heat", enabled="on"):
    return {
        "id": identity,
        "enabled": enabled,
        "zoneconditioning": state,
        "name": "PRIVATE NAME",
        "occupancy": "occupied",
        "rt": "PRIVATE TEMPERATURE",
        "htsp": "PRIVATE SETPOINT",
    }


class ContextTests(unittest.TestCase):
    def test_aliases_stay_stable_on_reorder_and_missing_zones_are_unknown(self):
        aliases = ZoneAliases()
        first, quality = aliases.observe([zone("PRIVATE-B"), zone("PRIVATE-A", "idle")])
        self.assertEqual(quality, "observed")
        self.assertEqual([z["zone"] for z in first], ["zone-1", "zone-2"])
        self.assertEqual(first[0]["zoneconditioning"], "idle")
        second, _ = aliases.observe([zone("PRIVATE-A", "idle"), zone("PRIVATE-B")])
        self.assertEqual(first, second)
        third, quality = aliases.observe([zone("PRIVATE-B"), zone("PRIVATE-C", "pending_cool")])
        self.assertEqual(quality, "incomplete zones")
        self.assertEqual(third[0], {"zone": "zone-1"})
        self.assertEqual(third[1]["zoneconditioning"], "active_heat")
        self.assertEqual(third[2]["zoneconditioning"], "pending_cool")
        self.assertNotIn("PRIVATE", json.dumps(third))

    def test_known_states_and_future_strings_are_preserved_not_coerced_to_heat(self):
        aliases = ZoneAliases()
        for state in (
            "active_heat",
            "prep_heat",
            "pending_heat",
            "active_cool",
            "prep_cool",
            "pending_cool",
            "idle",
            "future_state",
        ):
            with self.subTest(state=state):
                values, quality = aliases.observe([zone(1, state)])
                self.assertEqual(quality, "observed")
                self.assertEqual(values[0]["zoneconditioning"], state)

    def test_missing_duplicate_or_malformed_ids_are_not_silently_idle(self):
        cases = (
            None,
            [],
            "bad",
            [None],
            [zone(None)],
            [zone(True)],
            [zone("")],
            [zone("x" * 129)],
            [zone("a"), zone("a")],
        )
        for raw in cases:
            with self.subTest(raw=raw):
                aliases = ZoneAliases()
                values, quality = aliases.observe(raw)
                self.assertEqual(values, [])
                self.assertNotEqual(quality, "observed")
                self.assertFalse(aliases.aliases)
                self.assertNotIn("idle", quality)

    def test_alias_count_is_bounded_across_polls_and_failed_observations_are_atomic(self):
        aliases = ZoneAliases()
        values, quality = aliases.observe([zone(i) for i in range(MAX_ZONES)])
        self.assertEqual(quality, "observed")
        self.assertEqual(len(values), MAX_ZONES)
        before = aliases.aliases.copy()
        for raw in ([zone(i) for i in range(MAX_ZONES + 1)], [zone("new-id")]):
            self.assertEqual(aliases.observe(raw), ([], "too many zone identities"))
            self.assertEqual(aliases.aliases, before)
        self.assertEqual(aliases.observe([zone(i) for i in range(MAX_ZONES)])[1], "observed")

    def test_only_three_equipment_categories_are_retained(self):
        profile = {
            "idutype": "furnace",
            "idusource": "gas",
            "odutype": " VarCapHP ",
            "serial": "PRIVATE",
            "name": "PRIVATE",
            "indoorModel": "PRIVATE",
            "firmware": "PRIVATE",
            "address": "PRIVATE",
        }
        context = capture_context(profile, [zone("PRIVATE-ID")], ZoneAliases())
        self.assertEqual(
            context["equipment"],
            {
                "idutype": "furnace",
                "idusource": "gas",
                "odutype": " VarCapHP ",
            },
        )
        self.assertNotIn("PRIVATE", json.dumps(context))
        self.assertEqual(
            Reading("unit-1", NOW, context | {"odu": {"opstat": "35"}}, "observed").outdoor_percent,
            35,
        )

    def test_incomplete_or_oversized_fields_do_not_discard_other_equipment_state(self):
        raw = zone(1)
        raw["zoneconditioning"] = "x" * 129
        context = capture_context({"odutype": {"name": "PRIVATE"}}, [raw], ZoneAliases())
        self.assertEqual(context["equipment"], {})
        self.assertEqual(context["zones"], [{"zone": "zone-1", "enabled": "on"}])
        self.assertEqual(context["conditioning_quality"], "incomplete zones")
        self.assertNotIn("PRIVATE", str(context))

    def test_saved_context_is_reallowlisted_and_old_samples_need_no_migration(self):
        self.assertEqual(retained_context({"odu": {"opstat": "off"}}), {})
        values = {
            "equipment": {"odutype": "varcaphp", "serial": "PRIVATE"},
            "zones": [zone("PRIVATE") | {"zone": "zone-1"}],
            "conditioning_quality": "observed",
            "profile": {"password": "PRIVATE"},
        }
        context = retained_context(values)
        self.assertEqual(context["equipment"], {"odutype": "varcaphp"})
        self.assertEqual(
            context["zones"],
            [{"zone": "zone-1", "enabled": "on", "zoneconditioning": "active_heat"}],
        )
        self.assertNotIn("PRIVATE", str(context))
        self.assertEqual(retained_context(context), context)

    def test_bad_saved_zone_aliases_cannot_reveal_raw_ids_or_fake_observed_coverage(self):
        for zones in (
            [zone("PRIVATE")],
            [{"zone": "PRIVATE"}],
            [{"zone": "zone-0"}],
            [{"zone": "zone-1"}, {"zone": "zone-1"}],
            [{"zone": "zone-1"}] * (MAX_ZONES + 1),
        ):
            context = retained_context({"zones": zones, "conditioning_quality": "observed"})
            self.assertEqual(context["zones"], [])
            self.assertNotEqual(context["conditioning_quality"], "observed")
            self.assertNotIn("PRIVATE", str(context))
        for zones in ([], [{"zone": "zone-1"}]):
            context = retained_context({"zones": zones, "conditioning_quality": "observed"})
            self.assertNotEqual(context["conditioning_quality"], "observed")

    def test_unknown_saved_quality_is_not_upgraded_to_observed(self):
        for quality in (False, "future quality", {"private": "PRIVATE"}):
            context = retained_context(
                {
                    "zones": [{"zone": "zone-1", "enabled": "on", "zoneconditioning": "idle"}],
                    "conditioning_quality": quality,
                }
            )
            self.assertEqual(context["conditioning_quality"], "invalid conditioning quality")
            self.assertNotIn("PRIVATE", str(context))

    def test_maximum_roster_round_trips_in_existing_batch_and_schema_limits(self):
        samples = []
        for i in range(32):
            status = {"odu": {"type": "synthetic-family", "opstat": "35"}, "idu": {"opstat": "off"}}
            values = equipment_values(status) | capture_context(
                {"odutype": "varcaphp", "idutype": "furnace", "idusource": "gas"},
                [zone(j) for j in range(MAX_ZONES)],
                ZoneAliases(),
            )
            samples.append(
                {
                    "observed_utc_ns": NOW,
                    "elapsed_ns": 0,
                    "unit": f"unit-{i + 1}",
                    "reported_utc_ns": NOW,
                    "quality": "observed",
                    "values": values,
                }
            )
        with tempfile.TemporaryDirectory() as directory:
            with Journal(Path(directory), min_free_bytes=0) as journal:
                journal.commit(samples, [], {})
                path = journal.path
            with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
                self.assertEqual(db.execute("PRAGMA user_version").fetchone()[0], 1)
                stored = [
                    json.loads(row[0])
                    for row in db.execute("SELECT values_json FROM samples ORDER BY id")
                ]
            self.assertEqual(stored, [s["values"] for s in samples])
            self.assertNotIn("PRIVATE", json.dumps(stored))
