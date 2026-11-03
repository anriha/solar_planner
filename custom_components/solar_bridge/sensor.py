from __future__ import annotations

from typing import Any

from homeassistant.components.sensor import SensorDeviceClass, SensorEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import (
    CoordinatorEntity,
    DataUpdateCoordinator,
)
from homeassistant.util import dt as dt_util

from . import SolarBridgeData
from .entity import bridge_device, planner_device
from .plan_view import by_day, segments, upcoming


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
    async_add_entities([PlanNetSensor(data.coordinator, data.serial)])


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
            # Options make this an enum sensor, and HA rejects an
            # options-carrying sensor that is not declared as one: on every
            # setup until 2026-11-04 these nine (run_state, work_mode,
            # gen_port_usage, tou_charge_source_1..6) failed to add with
            # "providing enum options, but is missing the enum device class".
            self._attr_options = [o["label"] for o in options]
            self._attr_device_class = SensorDeviceClass.ENUM
            self._attr_native_unit_of_measurement = None
            self._attr_state_class = None
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


class PlanNetSensor(CoordinatorEntity[DataUpdateCoordinator[Any]], SensorEntity):
    """The horizon's expected net, with the actions rolled up per day.

    The calendar carries every segment; this carries the money — the figure that only
    exists once the quarter hours are summed, split per local day so "what does today
    come to" is answerable without a template. The estimate is the planner's own: it is
    worth what the forecast and the terminal value are worth.
    """

    _attr_has_entity_name = True
    _attr_name = "Plan expected net"
    _attr_native_unit_of_measurement = "CZK"
    _attr_device_class = SensorDeviceClass.MONETARY
    _attr_icon = "mdi:cash-clock"

    def __init__(self, coordinator: DataUpdateCoordinator[Any], serial: str) -> None:
        super().__init__(coordinator)
        self._serial = serial
        self._attr_unique_id = f"{serial}_plan_net"
        self._attr_device_info = planner_device(serial)

    @property
    def _plan(self) -> dict[str, Any] | None:
        if not self.coordinator.last_update_success:
            return None
        data = self.coordinator.data
        return data.plan if data is not None else None

    @property
    def available(self) -> bool:
        return self._plan is not None

    @property
    def native_value(self) -> float | None:
        plan = self._plan
        net = plan.get("expected_net_czk") if plan else None
        return round(net, 2) if isinstance(net, (int, float)) else None

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        plan = self._plan
        if not plan:
            return None
        runs = segments(plan)
        attributes: dict[str, Any] = {
            "horizon_end": _moment(plan.get("horizon_end")),
            "computed_at": _moment(plan.get("computed_at")),
            "segments": len(runs),
            "days": by_day(runs, dt_util.DEFAULT_TIME_ZONE),
        }
        if (upcoming_run := upcoming(plan, dt_util.utcnow().timestamp())) is not None:
            attributes["next"] = {
                "start": _moment(upcoming_run.start),
                "end": _moment(upcoming_run.end),
                "what": upcoming_run.summary,
                "net_czk": round(upcoming_run.net_czk, 2),
            }
        return attributes


def _moment(timestamp: Any):
    """Unix seconds -> aware datetime for an attribute; None stays None."""
    if not isinstance(timestamp, (int, float)):
        return None
    return dt_util.utc_from_timestamp(timestamp)
