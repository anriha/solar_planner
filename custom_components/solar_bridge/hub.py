import asyncio
import json
import logging
from dataclasses import dataclass

import aiohttp

_LOGGER = logging.getLogger(__name__)

RECONNECT_MIN = 5
RECONNECT_MAX = 60
HELLO_TIMEOUT = 10


@dataclass
class SolarBridgeState:
    ts: int
    link_ok: bool
    values: dict[str, float | str]


class BridgeAuthError(Exception):
    """The bridge rejected the token."""


class BridgeConnectionError(Exception):
    """Could not reach the bridge."""


@dataclass
class Hello:
    meta: dict
    state: SolarBridgeState
    directives: dict


def _state_from(payload: dict) -> SolarBridgeState:
    return SolarBridgeState(
        ts=payload["ts"],
        link_ok=payload["link_ok"],
        values=payload["values"],
    )


class BridgeHub:
    """Owns the WS connection. Push-based: the run loop feeds a coordinator."""

    def __init__(
        self, host: str, port: int, token: str, session: aiohttp.ClientSession
    ) -> None:
        self._url = f"ws://{host}:{port}/ws?token={token}"
        self._session = session

    async def async_connect(self) -> Hello:
        """Connect, await hello, return it (ws stays open in the returned task)."""
        ws = await self._connect()
        try:
            first = await asyncio.wait_for(ws.__anext__(), HELLO_TIMEOUT)
        except asyncio.TimeoutError:
            await ws.close()
            raise BridgeConnectionError("no hello within timeout") from None
        except StopAsyncIteration:
            raise BridgeConnectionError("connection closed before hello") from None
        if first.type is not aiohttp.WSMsgType.TEXT:
            await ws.close()
            raise BridgeConnectionError(f"unexpected frame type {first.type}")
        msg = json.loads(first.data)
        if msg.get("type") != "hello":
            await ws.close()
            raise BridgeConnectionError(
                f"first message was {msg.get('type')!r}, not hello"
            )
        return Hello(
            meta=msg["meta"],
            state=_state_from(msg["state"]),
            directives=msg["directives"],
        ), ws

    async def _connect(self) -> aiohttp.ClientWebSocketResponse:
        try:
            return await self._session.ws_connect(self._url, heartbeat=30)
        except aiohttp.WSServerHandshakeError as err:
            if err.status == 401:
                raise BridgeAuthError("bridge rejected the token") from err
            raise BridgeConnectionError(str(err)) from err
        except (aiohttp.ClientError, asyncio.TimeoutError) as err:
            raise BridgeConnectionError(str(err)) from err

    async def run(self, ws, on_state, on_error) -> None:
        backoff = RECONNECT_MIN
        while True:
            try:
                async for msg in ws:
                    if msg.type is not aiohttp.WSMsgType.TEXT:
                        if msg.type in (
                            aiohttp.WSMsgType.CLOSED,
                            aiohttp.WSMsgType.ERROR,
                        ):
                            break
                        continue
                    data = json.loads(msg.data)
                    match data.get("type"):
                        case "state":
                            on_state(
                                SolarBridgeState(
                                    data["ts"], data["link_ok"], data["values"]
                                )
                            )
                        case "hello":  # bridge restart re-sent hello
                            on_state(_state_from(data["state"]))
                        case "error":
                            _LOGGER.warning("bridge reported: %s", data.get("message"))
            except (aiohttp.ClientError, asyncio.TimeoutError) as err:
                _LOGGER.warning("bridge connection lost: %s", err)
            on_error("bridge connection lost")
            while True:
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, RECONNECT_MAX)
                try:
                    ws = await self._connect()
                    backoff = RECONNECT_MIN
                    break
                except BridgeAuthError:
                    raise  # token revoked: no point retrying, surface via ConfigEntryAuthFailed
                except BridgeConnectionError as err:
                    _LOGGER.info("reconnect failed (%s), retrying in %ss", err, backoff)
