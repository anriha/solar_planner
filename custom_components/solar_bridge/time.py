"""Time platform: HHMM control registers (TOU slot times) as native time entities."""

from __future__ import annotations

from datetime import time
from typing import Any

from homeassistant.components.time import TimeEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import SolarBridgeData
from .entity import ControlEntity


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[SolarBridgeData],
    async_add_entities: AddEntitiesCallback,
) -> None:
    data = entry.runtime_data
    # Writable control registers with a time constraint: the wire value is
    # "HH:MM"; the bridge validates it and encodes the HHMM register.
    async_add_entities(
        ControlTime(data.coordinator, data, control)
        for control in data.meta.get("controls", [])
        if control.get("writable") and control["constraint"]["type"] == "time"
    )


class ControlTime(ControlEntity, TimeEntity):
    """A wall-clock control register (TOU slot boundary)."""

    async def async_set_value(self, value: time) -> None:
        await self._hub.async_write(self._control["name"], value.strftime("%H:%M"))

    @property
    def native_value(self) -> time | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.values.get(self._control["name"])
        if not isinstance(raw, str):
            return None
        try:
            hours, minutes = raw.split(":")
            return time(int(hours), int(minutes))
        except (TypeError, ValueError):
            return None

    # HA renamed TimeEntity.value to native_value; keep the old name so the
    # entity also works on cores that still read `value`.
    @property
    def value(self) -> time | None:
        return self.native_value
