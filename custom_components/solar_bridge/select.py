"""Select platform: enum directives from meta.directives."""

from __future__ import annotations

from typing import Any

from homeassistant.components.select import SelectEntity
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
        DirectiveSelect(data.coordinator, data, spec)
        for spec in data.meta.get("directives", [])
        if spec.get("type") == "enum"
    )

    # Writable control registers with select constraints. Wire value is the
    # option's u16 value (D1-1b); readbacks decode to labels in values.
    async_add_entities(
        ControlSelect(data.coordinator, data, control)
        for control in data.meta.get("controls", [])
        if control.get("writable") and control["constraint"]["type"] == "select"
    )


class DirectiveSelect(DirectiveEntity, SelectEntity):
    """An enum directive, e.g. force_mode (options are protocol strings)."""

    def __init__(
        self,
        coordinator: Any,
        data: SolarBridgeData,
        spec: dict[str, Any],
    ) -> None:
        super().__init__(coordinator, data, spec)
        self._attr_options = [str(option) for option in spec["options"]]

    async def async_select_option(self, option: str) -> None:
        await self._hub.async_cmd(self._spec["name"], option)

    @property
    def current_option(self) -> str | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.directives.get(self._spec["name"])
        return None if raw is None else str(raw)


class ControlSelect(ControlEntity, SelectEntity):
    """A select-constraint control register; writes send the option u16 value."""

    def __init__(
        self,
        coordinator: Any,
        data: SolarBridgeData,
        control: dict[str, Any],
    ) -> None:
        super().__init__(coordinator, data, control)
        options = self._constraint["options"]
        self._attr_options = [o["label"] for o in options]
        self._by_label = {o["label"]: o["value"] for o in options}

    @property
    def current_option(self) -> str | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.values.get(self._control["name"])
        return None if raw is None else str(raw)

    async def async_select_option(self, option: str) -> None:
        await self._hub.async_write(self._control["name"], self._by_label[option])