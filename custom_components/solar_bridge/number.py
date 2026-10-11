"""Number platform: numeric directives from meta.directives."""

from __future__ import annotations

from typing import Any

from homeassistant.components.number import NumberEntity, NumberMode
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator

from . import SolarBridgeData
from .entity import ControlEntity, DirectiveEntity


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[SolarBridgeData],
    async_add_entities: AddEntitiesCallback,
) -> None:
    data = entry.runtime_data
    async_add_entities(
        DirectiveNumber(data.coordinator, data, spec)
        for spec in data.meta.get("directives", [])
        if spec.get("type") == "number"
    )
    # Writable control registers with number constraints (debug tooling).
    async_add_entities(
        ControlNumber(data.coordinator, data, control)
        for control in data.meta.get("controls", [])
        if control.get("writable") and control["constraint"]["type"] == "number"
    )


class DirectiveNumber(DirectiveEntity, NumberEntity):
    """A numeric directive; null renders as unknown (clearing stays v2)."""

    def __init__(
        self,
        coordinator: Any,
        data: SolarBridgeData,
        spec: dict[str, Any],
    ) -> None:
        super().__init__(coordinator, data, spec)
        self._attr_native_min_value = float(spec["min"])
        self._attr_native_max_value = float(spec["max"])
        self._attr_native_step = float(spec.get("step") or 1.0)
        if unit := spec.get("unit"):
            self._attr_native_unit_of_measurement = unit
        # Negative ranges (price ± CZK/kWh) are unusable as sliders in the UI.
        self._attr_mode = NumberMode.BOX if spec["min"] < 0 else NumberMode.SLIDER

    async def async_set_native_value(self, value: float) -> None:
        await self._hub.async_cmd(self._spec["name"], value)

    @property
    def native_value(self) -> float | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.directives.get(self._spec["name"])
        return None if raw is None else float(raw)


class ControlNumber(ControlEntity, NumberEntity):
    """A number-constraint control register; shows readback, writes via bridge."""

    def __init__(
        self,
        coordinator: DataUpdateCoordinator[SolarBridgeState],
        data: SolarBridgeData,
        control: dict[str, Any],
    ) -> None:
        super().__init__(coordinator, data, control)
        constraint = self._constraint
        self._attr_native_min_value = float(constraint["min"])
        self._attr_native_max_value = float(constraint["max"])
        self._attr_native_step = float(constraint.get("step") or 1.0)
        if unit := control.get("unit"):
            self._attr_native_unit_of_measurement = unit
        # Negative ranges (e.g. zero_export_power -500..500) are unusable as sliders.
        self._attr_mode = NumberMode.BOX if constraint["min"] < 0 else NumberMode.SLIDER

    async def async_set_native_value(self, value: float) -> None:
        await self._hub.async_write(self._control["name"], value)

    @property
    def native_value(self) -> float | None:
        if self.coordinator.data is None:
            return None
        raw = self.coordinator.data.values.get(self._control["name"])
        return None if raw is None else float(raw)