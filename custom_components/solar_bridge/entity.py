"""Shared base for directive-backed entities (meta.directives schema)."""

from __future__ import annotations

from typing import Any

from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import (
    CoordinatorEntity,
    DataUpdateCoordinator,
)

from . import SolarBridgeData
from .const import DOMAIN, MANUFACTURER, MODEL
from .hub import SolarBridgeState


def bridge_device(serial: str) -> DeviceInfo:
    """One HA device for the whole bridge: sensors + directives."""
    return DeviceInfo(
        identifiers={(DOMAIN, serial)},
        manufacturer=MANUFACTURER,
        model=MODEL,
        name=f"Deye {MODEL}",
    )


class DirectiveEntity(CoordinatorEntity[DataUpdateCoordinator[SolarBridgeState]]):
    """Base for entities driven by bridge directive state.

    Displayed state comes exclusively from server pushes — never optimistic.
    Availability follows WS connectivity only: directives live on the bridge
    and stay valid even when the inverter Modbus link is down (link_ok false).
    """

    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: DataUpdateCoordinator[SolarBridgeState],
        data: SolarBridgeData,
        spec: dict[str, Any],
    ) -> None:
        super().__init__(coordinator)
        self._hub = data.hub
        self._spec = spec
        self._attr_unique_id = f"{data.serial}_directive_{spec['name']}"
        self._attr_name = spec["name"].replace("_", " ")
        self._attr_device_info = bridge_device(data.serial)


class ControlEntity(CoordinatorEntity[DataUpdateCoordinator[SolarBridgeState]]):
    """Base for writable control-register entities (meta.controls, writable).

    Debug tooling for rare manual writes; the bridge owns validation
    (constraint-checked, readback-verified). native_value shows the polled
    register readback; display is never optimistic.
    """

    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: DataUpdateCoordinator[SolarBridgeState],
        data: SolarBridgeData,
        control: dict[str, Any],
    ) -> None:
        super().__init__(coordinator)
        self._hub = data.hub
        self._control = control
        self._constraint = control["constraint"]
        name = control["name"]
        self._attr_unique_id = f"{data.serial}_ctl_{name}"
        self._attr_name = name.replace("_", " ")
        self._attr_device_info = bridge_device(data.serial)

    @property
    def available(self) -> bool:
        return (
            self.coordinator.last_update_success
            and self.coordinator.data is not None
            and self.coordinator.data.link_ok
        )