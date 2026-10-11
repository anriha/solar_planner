"""Switch platform: bool directives from meta.directives."""

from __future__ import annotations

from homeassistant.components.switch import SwitchEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import SolarBridgeData
from .entity import ControlEntity, DirectiveEntity


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[SolarBridgeData],
    async_add_entities: AddEntitiesCallback,
) -> None:
    data = entry.runtime_data
    async_add_entities(
        DirectiveSwitch(data.coordinator, data, spec)
        for spec in data.meta.get("directives", [])
        if spec.get("type") == "bool"
    )

    # Writable control registers with switch constraints (debug tooling).
    async_add_entities(
        ControlSwitch(data.coordinator, data, control)
        for control in data.meta.get("controls", [])
        if control.get("writable") and control["constraint"]["type"] == "switch"
    )


class DirectiveSwitch(DirectiveEntity, SwitchEntity):
    """A bool directive, e.g. planner_enabled."""

    async def async_turn_on(self) -> None:
        await self._hub.async_cmd(self._spec["name"], True)

    async def async_turn_off(self) -> None:
        await self._hub.async_cmd(self._spec["name"], False)

    @property
    def is_on(self) -> bool | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.directives.get(self._spec["name"])
        return None if raw is None else bool(raw)


class ControlSwitch(ControlEntity, SwitchEntity):
    """A switch-constraint control register; on/off maps via constraint."""

    @property
    def is_on(self) -> bool | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.values.get(self._control["name"])
        if raw is None or raw not in (self._constraint["on"], self._constraint["off"]):
            return None
        return raw == self._constraint["on"]

    async def async_turn_on(self) -> None:
        await self._hub.async_write(self._control["name"], True)

    async def async_turn_off(self) -> None:
        await self._hub.async_write(self._control["name"], False)