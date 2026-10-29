"""Bounded, non-identifying equipment categories and anonymous zone context."""

MAX_ZONES = 32
CATEGORY_FIELDS = ("idutype", "idusource", "odutype")
ZONE_FIELDS = ("enabled", "zoneconditioning")
ZONE_ALIASES = frozenset(f"zone-{i}" for i in range(1, MAX_ZONES + 1))
CONDITIONING_QUALITIES = frozenset(
    (
        "observed",
        "incomplete zones",
        "missing zones",
        "invalid zone identities",
        "too many zone identities",
        "invalid conditioning quality",
    )
)


def scalar(value) -> bool:
    # No objects, unbounded strings, or non-finite values in retained context.
    return (
        value is None
        or isinstance(value, bool | int)
        or (isinstance(value, str) and len(value) <= 128)
    )


def categories(profile) -> dict:
    if not isinstance(profile, dict):
        return {}
    return {key: profile[key] for key in CATEGORY_FIELDS if key in profile and scalar(profile[key])}


def zone_values(raw, alias) -> dict:
    return {"zone": alias} | {
        key: raw[key] for key in ZONE_FIELDS if key in raw and scalar(raw[key])
    }


def zone_quality(zones) -> str:
    return (
        "observed"
        if all(all(zone.get(key) is not None for key in ZONE_FIELDS) for zone in zones)
        else "incomplete zones"
    )


def zone_order(zone):
    return int(zone["zone"].split("-")[1])


class ZoneAliases:
    """IDs remain in memory only; aliases are stable within one unit/session."""

    def __init__(self):
        self.aliases: dict[str, str] = {}

    def observe(self, raw) -> tuple[list, str]:
        if raw is None or raw == []:
            return [], "missing zones"
        if not isinstance(raw, list):
            return [], "invalid zone identities"
        if len(raw) > MAX_ZONES:
            return [], "too many zone identities"
        identities = []
        for zone in raw:
            identity = zone.get("id") if isinstance(zone, dict) else None
            if isinstance(identity, bool) or not isinstance(identity, str | int):
                return [], "invalid zone identities"
            identity = str(identity)
            if not identity.strip() or len(identity) > 128 or identity in identities:
                return [], "invalid zone identities"
            identities.append(identity)
        new = set(identities) - self.aliases.keys()
        if len(self.aliases) + len(new) > MAX_ZONES:
            return [], "too many zone identities"
        for identity in sorted(new):
            self.aliases[identity] = f"zone-{len(self.aliases) + 1}"
        zones = [
            zone_values(zone, self.aliases[identity])
            for zone, identity in zip(raw, identities, strict=True)
        ]
        # An omitted known zone is unobserved, not removed or silently idle.
        zones.extend(
            {"zone": alias}
            for identity, alias in self.aliases.items()
            if identity not in identities
        )
        zones.sort(key=zone_order)
        return zones, zone_quality(zones)


def capture_context(profile, raw_zones, aliases: ZoneAliases) -> dict:
    zones, quality = aliases.observe(raw_zones)
    return {"equipment": categories(profile), "zones": zones, "conditioning_quality": quality}


def retained_context(values: dict) -> dict:
    """Re-allowlist saved context, never accepting raw zone IDs or profile dumps."""
    out = {}
    if isinstance(values.get("equipment"), dict):
        out["equipment"] = categories(values["equipment"])
    if "zones" not in values and "conditioning_quality" not in values:
        return out  # Old version-1 samples have no context; do not fabricate it.
    raw = values.get("zones")
    quality = values.get("conditioning_quality")
    if not isinstance(raw, list):
        out.update(zones=[], conditioning_quality="missing zones")
        return out
    if len(raw) > MAX_ZONES:
        out.update(zones=[], conditioning_quality="too many zone identities")
        return out
    zones = []
    seen = set()
    for zone in raw:
        alias = zone.get("zone") if isinstance(zone, dict) else None
        if not isinstance(alias, str) or alias not in ZONE_ALIASES or alias in seen:
            out.update(zones=[], conditioning_quality="invalid zone identities")
            return out
        seen.add(alias)
        zones.append(zone_values(zone, alias))
    out["zones"] = sorted(zones, key=zone_order)
    if quality is None:
        quality = zone_quality(zones) if zones else "missing zones"
    elif not isinstance(quality, str) or quality not in CONDITIONING_QUALITIES:
        quality = "invalid conditioning quality"
    if quality == "observed" and zone_quality(zones) != "observed":
        quality = "incomplete zones"
    if not zones and quality in ("observed", "incomplete zones"):
        quality = "missing zones"
    out["conditioning_quality"] = quality
    return out
