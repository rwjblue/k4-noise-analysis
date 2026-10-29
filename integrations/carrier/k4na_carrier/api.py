"""Only authentication and get_systems queries; never invoke an HVAC setter.

Carrier's response includes private profile/configuration information. Discard
it in memory and retain only an explicit allowlist of equipment telemetry.
"""

import asyncio
import math
from dataclasses import dataclass
from datetime import datetime
from typing import Any

from .context import ZoneAliases, capture_context


class Unavailable(Exception):
    """A redacted reason why there is no usable observation."""


@dataclass(frozen=True)
class Reading:
    unit: str
    reported_utc_ns: int | None
    values: dict[str, Any]
    quality: str

    @property
    def outdoor_percent(self) -> float | None:
        # Profile categories and runtime unit-family codes are different domains.
        equipment = self.values.get("equipment", {})
        category = equipment.get("odutype") if isinstance(equipment, dict) else None
        if (
            self.quality != "observed"
            or not isinstance(category, str)
            or category.strip().lower() not in ("varcaphp", "varcapac")
        ):
            return None
        value = numeric(self.values.get("odu", {}).get("opstat"))
        return value if value is not None and 0 <= value <= 100 else None

    @property
    def label(self) -> str:
        if self.quality != "observed":
            return f"HVAC {self.unit}: unknown ({self.quality})"
        parts = []
        for key in ("odu", "idu"):
            value = self.values.get(key, {}).get("opstat")
            if value is None:
                value = "unknown"
            elif numeric(value) is not None:
                # Preserve changing numeric output in the source log, not a
                # new annotation per percent. Do not treat missing as zero.
                value = "numeric output (see HVAC log)"
            else:
                value = str(value)
                if len(value) > 48 or not all(c.isalnum() or c in " _-.%" for c in value):
                    value = "unrecognized status"
            parts.append(f"{key.upper()} {value}")
        return f"HVAC {self.unit}: " + "; ".join(parts)


def numeric(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    try:
        n = float(value)
        return n if math.isfinite(n) else None
    except ValueError, TypeError, OverflowError:
        return None


def timestamp_ns(value: Any) -> int | None:
    if not isinstance(value, str):
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return int(dt.timestamp() * 1_000_000_000) if dt.tzinfo else None
    except ValueError, OverflowError, OSError:
        return None


def scalar(value: Any) -> bool:
    return (
        value is None
        or isinstance(value, bool | int)
        or (isinstance(value, float) and math.isfinite(value))
        or (isinstance(value, str) and len(value) <= 128)
    )


def equipment_values(status: dict) -> dict:
    """Native Carrier values only: no names, serials, location, zones or occupancy."""
    out = {}
    for key in ("utcTime", "isDisconnected", "mode", "cfgem", "oat"):
        if key in status and scalar(status[key]):
            out[key] = status[key]
    for unit, keys in (
        ("odu", ("type", "opstat", "iducfm")),
        ("idu", ("type", "opstat", "cfm", "statpress", "blwrpm")),
    ):
        raw = status.get(unit)
        if isinstance(raw, dict):
            out[unit] = {k: raw[k] for k in keys if k in raw and scalar(raw[k])}
    return out


class CarrierClient:
    """Polling adapter. Unit aliases are scoped to this connection/session."""

    def __init__(self, username: str, password: str, max_age_seconds: float = 600):
        from carrier_api.api_connection_graphql import ApiConnectionGraphql

        self._api = ApiConnectionGraphql(username=username, password=password)
        self._aliases: dict[str, str] = {}
        self._zone_aliases: dict[str, ZoneAliases] = {}
        self._max_age_ns = int(max_age_seconds * 1_000_000_000)

    async def read(self, now_ns: int) -> list[Reading]:
        # get_systems authenticates/refreshes tokens itself. Unlike load_data,
        # it does not fetch energy history or construct unnecessary models.
        response = await self._api.get_systems()
        systems = response.get("infinitySystems")
        if not isinstance(systems, list) or not systems or len(systems) > 32:
            raise Unavailable("no supported Infinity systems (maximum 32)")
        by_id = {}
        for system in systems:
            serial = system.get("profile", {}).get("serial")
            if not isinstance(serial, str) or not serial or serial in by_id:
                raise Unavailable("invalid system identities")
            by_id[serial] = system
        for serial in sorted(by_id):
            if serial not in self._aliases:
                self._aliases[serial] = f"unit-{len(self._aliases) + 1}"
        if len(self._aliases) > 32:
            raise Unavailable("too many system identities in one session")
        out = []
        for serial, alias in self._aliases.items():
            status = by_id.get(serial, {}).get("status")
            values = equipment_values(status) if isinstance(status, dict) else {}
            if isinstance(status, dict):
                aliases = self._zone_aliases.setdefault(serial, ZoneAliases())
                values.update(
                    capture_context(
                        by_id.get(serial, {}).get("profile"), status.get("zones"), aliases
                    )
                )
            reported = timestamp_ns(values.get("utcTime"))
            if not isinstance(status, dict) or not status:
                quality = "missing system/status"
            elif values.get("isDisconnected") is not False:
                quality = "disconnected or connection unknown"
            elif reported is None:
                quality = "missing source timestamp"
            elif now_ns - reported > self._max_age_ns:
                quality = "stale source timestamp"
            elif reported - now_ns > 60_000_000_000:
                quality = "source clock ahead"
            elif not any(values.get(k, {}).get("opstat") is not None for k in ("odu", "idu")):
                quality = "missing operating status"
            else:
                quality = "observed"
            out.append(Reading(alias, reported, values, quality))
        return out

    async def close(self):
        try:
            async with asyncio.timeout(5):
                await self._api.cleanup()
        except Exception:
            pass  # Cleanup cannot leak a third-party exception or credentials.
