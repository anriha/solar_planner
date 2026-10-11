from __future__ import annotations

from typing import Any

from homeassistant.components.sensor import SensorEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import (
    CoordinatorEntity,
    DataUpdateCoordinator,
)

from . import SolarBridgeData
from .entity import bridge_device


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[SolarBridgeData],
    async_add_entities: AddEntitiesCallback,
) -> None:
    data = entry.runtime_data
    async_add_entities(
        SolarBridgeSensor(data.coordinator, sensor, data.serial)
        for sensor in data.meta["sensors"]
    )
    # Control readbacks: polled by the bridge and decoded into values since
    # the 2026-10-11 meta/state extension; read-only here (writes are the
    # Control* entities in switch/select/number).
    async_add_entities(
        SolarBridgeSensor(data.coordinator, control, data.serial)
        for control in data.meta.get("controls", [])
    )


class SolarBridgeSensor(CoordinatorEntity[DataUpdateCoordinator[Any]], SensorEntity):
    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: DataUpdateCoordinator[Any],
        sensor_def: dict[str, Any],
        serial: str,
    ) -> None:
        super().__init__(coordinator)
        self._def = sensor_def
        self._attr_unique_id = f"{serial}_{sensor_def['name']}"
        self._attr_name = sensor_def["name"].replace("_", " ")
        if unit := sensor_def.get("unit"):
            self._attr_native_unit_of_measurement = unit
        if device_class := sensor_def.get("device_class"):
            self._attr_device_class = device_class
        if state_class := sensor_def.get("state_class"):
            self._attr_state_class = state_class
        options = sensor_def.get("options")
        # Control defs carry enum options under constraint (Select), not top-level.
        if (
            options is None
            and sensor_def.get("constraint", {}).get("type") == "select"
        ):
            options = sensor_def["constraint"]["options"]
        if options:
            self._attr_options = [o["label"] for o in options]
        self._attr_device_info = bridge_device(serial)

    @property
    def native_value(self) -> float | str | None:
        if self.coordinator.data is None:
            return None
        return self.coordinator.data.values.get(self._def["name"])

    @property
    def available(self) -> bool:
        return (
            self.coordinator.last_update_success
            and self.coordinator.data is not None
            and self.coordinator.data.link_ok
        )