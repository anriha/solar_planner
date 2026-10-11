import asyncio
import json
import logging
import uuid
from dataclasses import dataclass
from typing import Any

import aiohttp
from homeassistant.exceptions import HomeAssistantError

_LOGGER = logging.getLogger(__name__)

RECONNECT_MIN = 5
RECONNECT_MAX = 60
HELLO_TIMEOUT = 10
CMD_TIMEOUT = 10


@dataclass
class SolarBridgeState:
    ts: int
    link_ok: bool
    values: dict[str, float | str]
    directives: dict[str, Any]


class BridgeAuthError(Exception):
    """The bridge rejected the token."""


class BridgeConnectionError(Exception):
    """Could not reach the bridge."""


class BridgeCmdError(HomeAssistantError):
    """A directive command was rejected or lost before its ack."""


@dataclass
class Hello:
    meta: dict
    state: SolarBridgeState
    directives: dict


def _state_from(payload: dict, directives: dict) -> SolarBridgeState:
    return SolarBridgeState(
        ts=payload["ts"],
        link_ok=payload["link_ok"],
        values=payload["values"],
        directives=directives,
    )


class BridgeHub:
    """Owns the WS connection. Push-based: the run loop feeds a coordinator."""

    def __init__(
        self, host: str, port: int, token: str, session: aiohttp.ClientSession
    ) -> None:
        self._url = f"ws://{host}:{port}/ws?token={token}"
        self._session = session
        self._ws: aiohttp.ClientWebSocketResponse | None = None
        self._pending: dict[str, asyncio.Future[None]] = {}

    async def async_connect(
        self,
    ) -> tuple[Hello, aiohttp.ClientWebSocketResponse]:
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
            state=_state_from(msg["state"], msg["directives"]),
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

    async def _request(self, kind: str, name: str, value: Any) -> None:
        """Send a cmd/write frame and await its ack by id.

        Raises BridgeCmdError on rejection, timeout, or connection loss.
        Displayed entity state comes from the server push that follows —
        never from this call (no optimistic updates).
        """
        ws = self._ws
        if ws is None or ws.closed:
            raise BridgeCmdError("bridge connection is down")
        cid = uuid.uuid4().hex
        fut: asyncio.Future[None] = asyncio.get_running_loop().create_future()
        self._pending[cid] = fut
        try:
            try:
                await ws.send_str(
                    json.dumps({"type": kind, "id": cid, "name": name, "value": value})
                )
            except OSError as err:
                raise BridgeCmdError(f"connection lost sending {kind}: {err}") from err
            await asyncio.wait_for(fut, CMD_TIMEOUT)
        except asyncio.TimeoutError:
            raise BridgeCmdError(
                f"no ack for {kind} '{name}' within {CMD_TIMEOUT}s"
            ) from None
        finally:
            self._pending.pop(cid, None)

    async def async_cmd(self, name: str, value: Any) -> None:
        """Send a directive command and await the bridge ack."""
        await self._request("cmd", name, value)

    async def async_write(self, name: str, value: Any) -> None:
        """Send a control-register write and await write_ack."""
        await self._request("write", name, value)

    def _resolve_ack(self, data: dict) -> None:
        """Complete a pending cmd/write request from its ack frame."""
        fut = self._pending.pop(data.get("id"), None)
        if fut is None or fut.done():
            return
        if data.get("ok"):
            fut.set_result(None)
        else:
            fut.set_exception(BridgeCmdError(data.get("error", "directive rejected")))

    def _fail_pending(self, reason: str) -> None:
        """Fail all in-flight commands (connection dropped)."""
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(BridgeCmdError(reason))
        self._pending.clear()

    async def run(self, ws, on_state, on_error) -> None:
        backoff = RECONNECT_MIN
        while True:
            self._ws = ws
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
                                    data["ts"],
                                    data["link_ok"],
                                    data["values"],
                                    data["directives"],
                                )
                            )
                        case "hello":  # bridge restart re-sent hello
                            on_state(_state_from(data["state"], data["directives"]))
                        case "cmd_ack":
                            self._resolve_ack(data)
                        case "write_ack":
                            self._resolve_ack(data)
                        case "error":
                            _LOGGER.warning("bridge reported: %s", data.get("message"))
            except (aiohttp.ClientError, asyncio.TimeoutError) as err:
                _LOGGER.warning("bridge connection lost: %s", err)
            self._fail_pending("bridge connection lost")
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
