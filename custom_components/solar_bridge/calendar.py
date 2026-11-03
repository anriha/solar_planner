"""The dispatch plan, rendered as a calendar.

One event per plan *segment* — a run of MTUs the planner wants to do the same thing — so
the calendar timeline answers "what does it intend at 12:00" directly. Consecutive MTUs
with the same action are merged into a single event by `plan_view`, and its figures are
summed over the run rather than taken from the last quarter hour.

The events are derived from the live push state, not stored: the plan is rewritten about
once a minute, so a stored copy would only go stale.
"""

from __future__ import annotations

from datetime import datetime
from typing import Any

from homeassistant.components.calendar import CalendarEntity, CalendarEvent
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import (
    CoordinatorEntity,
    DataUpdateCoordinator,
)
from homeassistant.util import dt as dt_util

from . import SolarBridgeData
from .entity import planner_device
from .plan_view import Segment, at, segments


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[SolarBridgeData],
    async_add_entities: AddEntitiesCallback,
) -> None:
    data = entry.runtime_data
    async_add_entities([PlanCalendar(data.coordinator, data.serial)])


class PlanCalendar(CoordinatorEntity[DataUpdateCoordinator[Any]], CalendarEntity):
    """The bridge's dispatch plan as a read-only calendar."""

    _attr_has_entity_name = True
    _attr_name = "Plan"

    def __init__(self, coordinator: DataUpdateCoordinator[Any], serial: str) -> None:
        super().__init__(coordinator)
        self._serial = serial
        self._attr_unique_id = f"{serial}_plan_calendar"
        self._attr_device_info = planner_device(serial)

    @property
    def _plan(self) -> dict[str, Any] | None:
        if not self.coordinator.last_update_success:
            return None
        data = self.coordinator.data
        return data.plan if data is not None else None

    @property
    def available(self) -> bool:
        # A plan is an intention: it stays meaningful while Modbus is down, so availability
        # follows the websocket, never link_ok.
        return self._plan is not None

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        """The plan's own totals, for templates and dashboards."""
        plan = self._plan
        if not plan:
            return None
        return {
            "computed_at": dt_util.utc_from_timestamp(plan["computed_at"])
            if "computed_at" in plan
            else None,
            "horizon_end": dt_util.utc_from_timestamp(plan["horizon_end"])
            if "horizon_end" in plan
            else None,
            "expected_net_czk": plan.get("expected_net_czk"),
            "segments": len(segments(plan)),
        }

    def _event(self, segment: Segment) -> CalendarEvent:
        return CalendarEvent(
            start=dt_util.utc_from_timestamp(segment.start),
            end=dt_util.utc_from_timestamp(segment.end),
            summary=segment.summary,
            description=segment.description,
            uid=f"{self._serial}-plan-{segment.start}",
        )

    @property
    def event(self) -> CalendarEvent | None:
        """The segment the clock is inside, if any."""
        plan = self._plan
        if not plan:
            return None
        current = at(plan, dt_util.utcnow().timestamp())
        return self._event(current) if current else None

    async def async_get_events(
        self,
        hass: HomeAssistant,
        start_date: datetime,
        end_date: datetime,
    ) -> list[CalendarEvent]:
        plan = self._plan
        if not plan:
            return []
        lower = start_date.timestamp()
        upper = end_date.timestamp()
        return [
            self._event(segment)
            for segment in segments(plan)
            if segment.end > lower and segment.start < upper
        ]
