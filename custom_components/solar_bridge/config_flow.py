from __future__ import annotations

import logging
from typing import Any

import voluptuous as vol
from homeassistant.config_entries import ConfigFlow, ConfigFlowResult
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .const import CONF_HOST, CONF_PORT, CONF_TOKEN, DEFAULT_PORT, DOMAIN
from .hub import BridgeAuthError, BridgeConnectionError, BridgeHub

_LOGGER = logging.getLogger(__name__)

STEP_USER_SCHEMA = vol.Schema(
    {
        vol.Required(CONF_HOST): str,
        vol.Required(CONF_PORT, default=DEFAULT_PORT): int,
        vol.Required(CONF_TOKEN): str,
    }
)


async def _validate(hass: HomeAssistant, data: dict[str, Any]) -> str:
    hub = BridgeHub(
        data[CONF_HOST],
        data[CONF_PORT],
        data[CONF_TOKEN],
        async_get_clientsession(hass),
    )
    hello, ws = await hub.async_connect()
    await ws.close()
    return str(
        hello.state.values.get("serial_number", f"{data[CONF_HOST]}:{data[CONF_PORT]}")
    )


class SolarBridgeConfigFlow(ConfigFlow, domain=DOMAIN):
    VERSION = 1

    async def async_step_user(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                serial = await _validate(self.hass, user_input)
            except BridgeAuthError:
                errors["base"] = "invalid_auth"
            except (BridgeConnectionError, TimeoutError, HomeAssistantError):
                errors["base"] = "cannot_connect"
            except Exception:  # noqa: BLE001
                _LOGGER.exception("Unexpected error during bridge connection test")
                errors["base"] = "unknown"
            else:
                await self.async_set_unique_id(serial)
                self._abort_if_unique_id_configured()
                return self.async_create_entry(
                    title=f"Deye Solar Bridge {serial}", data=user_input
                )

        return self.async_show_form(
            step_id="user", data_schema=STEP_USER_SCHEMA, errors=errors
        )
