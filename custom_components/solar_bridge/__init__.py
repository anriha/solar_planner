from __future__ import annotations

import logging
from dataclasses import dataclass

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed, ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .const import CONF_HOST, CONF_PORT, CONF_TOKEN, DEFAULT_PORT, DOMAIN
from .hub import BridgeAuthError, BridgeConnectionError, BridgeHub, SolarBridgeState

_LOGGER = logging.getLogger(__name__)

PLATFORMS = [Platform.SENSOR, Platform.SWITCH, Platform.SELECT, Platform.NUMBER]


@dataclass
class SolarBridgeData:
    hub: BridgeHub
    coordinator: DataUpdateCoordinator[SolarBridgeState]
    meta: dict
    serial: str


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    hub = BridgeHub(
        entry.data[CONF_HOST],
        entry.data.get(CONF_PORT, DEFAULT_PORT),
        entry.data[CONF_TOKEN],
        async_get_clientsession(hass),
    )
    try:
        hello, ws = await hub.async_connect()
    except BridgeAuthError as err:
        raise ConfigEntryAuthFailed from err
    except BridgeConnectionError as err:
        raise ConfigEntryNotReady(str(err)) from err

    serial = str(
        hello.state.values.get(
            "serial_number",
            f"{entry.data[CONF_HOST]}:{entry.data.get(CONF_PORT, DEFAULT_PORT)}",
        )
    )
    coordinator: DataUpdateCoordinator[SolarBridgeState] = DataUpdateCoordinator(
        config_entry=entry, hass=hass, logger=_LOGGER, name=DOMAIN
    )
    coordinator.async_set_updated_data(hello.state)

    entry.async_create_background_task(
        hass,
        hub.run(
            ws, coordinator.async_set_updated_data, coordinator.async_set_update_error
        ),
        "solar_bridge_ws",
    )

    entry.runtime_data = SolarBridgeData(
        hub=hub, coordinator=coordinator, meta=hello.meta, serial=serial
    )

    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


def _mark_disconnected(err: str) -> None:
    """Bridge connection lost; entities flip unavailable via last_update_success."""
    raise UpdateFailed(err)


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
