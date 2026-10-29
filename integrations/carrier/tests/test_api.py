import unittest
from unittest.mock import AsyncMock, patch

from k4na_carrier.api import CarrierClient, Reading, Unavailable, timestamp_ns
from k4na_carrier.store import iso

NOW = 1_800_000_000_000_000_000


def payload(status=None, serial="synthetic-unit"):
    return {
        "infinitySystems": [
            {
                "profile": {
                    "serial": serial,
                    "name": "PRIVATE NAME",
                    "address": "PRIVATE ADDRESS",
                    "idutype": "furnace",
                    "idusource": "gas",
                    "odutype": "varcaphp",
                },
                "config": {"password": "PRIVATE PASSWORD"},
                "status": status
                if status is not None
                else {
                    "utcTime": iso(NOW),
                    "isDisconnected": False,
                    "mode": "hpheat",
                    "cfgem": "F",
                    "odu": {"type": "synthetic-family", "opstat": "35", "iducfm": "800"},
                    "idu": {"opstat": "heat1", "cfm": "800", "blwrpm": "900"},
                    "zones": [
                        {
                            "id": "PRIVATE ZONE ID",
                            "name": "PRIVATE ZONE",
                            "occupancy": "occupied",
                            "enabled": "on",
                            "zoneconditioning": "active_heat",
                            "rt": "PRIVATE TEMPERATURE",
                            "htsp": "PRIVATE SETPOINT",
                        }
                    ],
                },
            }
        ]
    }


class ApiTests(unittest.IsolatedAsyncioTestCase):
    def client(self, result):
        api = AsyncMock()
        api.get_systems.return_value = result
        with patch("carrier_api.api_connection_graphql.ApiConnectionGraphql", return_value=api):
            client = CarrierClient("test-user", "test-password")
        return client, api

    async def test_only_queries_status_and_discards_identity(self):
        client, api = self.client(payload())
        readings = await client.read(NOW)
        self.assertEqual(len(readings), 1)
        self.assertEqual(readings[0].quality, "observed")
        self.assertEqual(readings[0].values["odu"]["opstat"], "35")
        self.assertEqual(readings[0].values["idu"]["cfm"], "800")
        self.assertEqual(
            readings[0].values["equipment"],
            {"idutype": "furnace", "idusource": "gas", "odutype": "varcaphp"},
        )
        self.assertEqual(
            readings[0].values["zones"],
            [{"zone": "zone-1", "enabled": "on", "zoneconditioning": "active_heat"}],
        )
        self.assertEqual(readings[0].values["conditioning_quality"], "observed")
        self.assertEqual(readings[0].outdoor_percent, 35)
        self.assertNotIn("PRIVATE", str(readings))
        self.assertNotIn("synthetic-unit", str(readings))
        self.assertEqual([call[0] for call in api.mock_calls], ["get_systems"])
        await client.close()
        api.cleanup.assert_awaited_once()

    async def test_missing_stale_disconnected_and_future_are_unknown(self):
        cases = (
            ({"isDisconnected": True}, "disconnected"),
            ({"isDisconnected": None}, "connection unknown"),
            ({"utcTime": iso(NOW - 601_000_000_000)}, "stale"),
            ({"utcTime": iso(NOW + 61_000_000_000)}, "ahead"),
            ({"utcTime": "2026-10-28T12:00:00"}, "missing source timestamp"),
            ({"odu": {}, "idu": {}}, "missing operating status"),
        )
        for changes, reason in cases:
            with self.subTest(reason=reason):
                result = payload()
                result["infinitySystems"][0]["status"].update(changes)
                client, _ = self.client(result)
                reading = (await client.read(NOW))[0]
                self.assertIn(reason, reading.quality)
                self.assertIn("unknown", reading.label)

    async def test_missing_unit_is_unknown_not_removed(self):
        first = payload()
        first["infinitySystems"].extend(payload(serial="synthetic-second")["infinitySystems"])
        client, api = self.client(first)
        self.assertEqual(len(await client.read(NOW)), 2)
        api.get_systems.return_value = payload()
        readings = await client.read(NOW)
        self.assertEqual(len(readings), 2)
        self.assertEqual(sum(r.quality == "observed" for r in readings), 1)

    async def test_zone_aliases_are_independent_for_each_unit(self):
        result = payload()
        other = payload(serial="synthetic-second")["infinitySystems"][0]
        other["status"]["zones"][0]["id"] = "OTHER PRIVATE ZONE ID"
        result["infinitySystems"].append(other)
        client, api = self.client(result)
        readings = await client.read(NOW)
        self.assertEqual([r.values["zones"][0]["zone"] for r in readings], ["zone-1", "zone-1"])
        self.assertEqual([call[0] for call in api.mock_calls], ["get_systems"])
        self.assertNotIn("PRIVATE", str(readings))

    async def test_bad_zone_context_does_not_invalidate_equipment_or_invent_idle(self):
        for zones in (None, [], [{"name": "PRIVATE"}], [{"id": "zone-without-status"}]):
            with self.subTest(zones=zones):
                result = payload()
                result["infinitySystems"][0]["status"]["zones"] = zones
                client, _ = self.client(result)
                reading = (await client.read(NOW))[0]
                self.assertEqual(reading.quality, "observed")
                self.assertEqual(reading.outdoor_percent, 35)
                self.assertNotEqual(reading.values["conditioning_quality"], "observed")
                self.assertNotIn("idle", str(reading.values))
                self.assertNotIn("PRIVATE", str(reading.values))

    async def test_no_supported_devices_refuses_instead_of_fake_off(self):
        client, _ = self.client({"infinitySystems": []})
        with self.assertRaises(Unavailable):
            await client.read(NOW)

    def test_numeric_capacity_is_preserved_but_not_a_mark_per_percent(self):
        one = Reading("unit-1", NOW, {"odu": {"opstat": "35"}}, "observed")
        two = Reading("unit-1", NOW, {"odu": {"opstat": "60"}}, "observed")
        self.assertEqual(one.label, two.label)
        self.assertIn("IDU unknown", one.label)
        self.assertNotIn("off", one.label)
        self.assertIsNone(timestamp_ns("not a timestamp"))
