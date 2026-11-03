import asyncio
import json
import logging
import uuid
from dataclasses import dataclass, replace
from typing import Any

import aiohttp
from homeassistant.exceptions import HomeAssistantError

_LOGGER = logging.getLogger(__name__)

RECONNECT_MIN = 5
RECONNECT_MAX = 60
HELLO_TIMEOUT = 10
CMD_TIMEOUT = 10
# Liveness on the message channel, not on TCP. The bridge answers protocol
# pings by itself, so those stay healthy even when the state feed has been
# cut (2026-11-03: 19 h 30 m of silence that looked like a live connection).
# A pong — or any frame — proves the connection still carries messages both
# ways; silence past SILENCE_TIMEOUT means it does not.
APP_PING_INTERVAL = 30
SILENCE_TIMEOUT = 90


@dataclass
class SolarBridgeState:
    ts: int
    link_ok: bool
    values: dict[str, float | str]
    directives: dict[str, Any]
    plan: dict[str, Any] | None = None


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
        plan=payload.get("plan"),
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
        # The plan arrives on its own frame; the hub keeps the last one and stamps it
        # onto every state it forwards (and onto the state a plan frame re-emits).
        self._plan: dict[str, Any] | None = None
        self._state: SolarBridgeState | None = None

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
        state = _state_from(msg["state"], msg["directives"])
        # Seed the hub from hello, or a plan frame that arrives before the first
        # telemetry frame would have no state to attach itself to.
        self._plan = state.plan
        self._state = state
        return Hello(
            meta=msg["meta"],
            state=state,
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

    def _handle(self, data: dict, on_state) -> None:
        """Apply one decoded server frame.

        The plan has its own frame (`type: "plan"`) because it changes about once a
        minute while telemetry arrives every 5 s: ridden on every poll it would be
        ~1.2 GB/day. So the hub holds the last plan and stamps it onto every state it
        forwards, and a plan frame re-emits the current state with the new plan.
        """
        match data.get("type"):
            case "state":
                if "plan" in data:  # a bridge old enough to still ride it on telemetry
                    self._plan = data["plan"]
                self._state = SolarBridgeState(
                    data["ts"],
                    data["link_ok"],
                    data["values"],
                    data["directives"],
                    self._plan,
                )
                on_state(self._state)
            case "plan":
                self._plan = data.get("plan")
                if self._state is None:
                    return
                self._state = replace(self._state, plan=self._plan)
                on_state(self._state)
            case "hello":  # bridge restart re-sent hello
                self._state = _state_from(data["state"], data["directives"])
                self._plan = self._state.plan
                on_state(self._state)
            case "cmd_ack":
                self._resolve_ack(data)
            case "write_ack":
                self._resolve_ack(data)
            case "error":
                _LOGGER.warning("bridge reported: %s", data.get("message"))

    async def _pump(self, ws, on_state) -> None:
        """Read frames, and prove the link is wired at the application level.

        The socket alone proves nothing: the bridge answers protocol pings
        itself, so a connection whose state feed had been cut still looks
        healthy from here. That is exactly how HA lost the inverter for
        19 h 30 m on 2026-11-03 — no exception, no close, only silence.

        So ping on the message channel (the bridge's own connection task
        answers that — the same task that pushes state) and reconnect when
        nothing at all comes back for SILENCE_TIMEOUT. Any frame counts, not
        just state: a bridge whose RS485 link is down is *legitimately*
        quiet after its one `link_ok: false` push, and reconnecting into
        that would only churn.
        """
        loop = asyncio.get_running_loop()
        last_frame = loop.time()

        async def reader() -> None:
            nonlocal last_frame
            async for msg in ws:
                if msg.type is aiohttp.WSMsgType.TEXT:
                    last_frame = loop.time()
                    self._handle(json.loads(msg.data), on_state)
                elif msg.type in (
                    aiohttp.WSMsgType.CLOSED,
                    aiohttp.WSMsgType.ERROR,
                ):
                    return

        async def keepalive() -> None:
            nonlocal last_frame
            next_ping = last_frame + APP_PING_INTERVAL
            tick = min(1.0, APP_PING_INTERVAL / 2)
            while True:
                await asyncio.sleep(tick)
                now = loop.time()
                if now - last_frame > SILENCE_TIMEOUT:
                    _LOGGER.warning(
                        "no frame from the bridge in %ss, reconnecting",
                        SILENCE_TIMEOUT,
                    )
                    await ws.close()
                    return
                if now >= next_ping:
                    next_ping = now + APP_PING_INTERVAL
                    try:
                        await ws.send_str(json.dumps({"type": "ping"}))
                    except (OSError, RuntimeError) as err:
                        _LOGGER.warning("bridge connection lost sending ping: %s", err)
                        return

        tasks = [asyncio.ensure_future(reader()), asyncio.ensure_future(keepalive())]
        try:
            done, _pending = await asyncio.wait(
                tasks, return_when=asyncio.FIRST_COMPLETED
            )
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        for task in done:
            if not task.cancelled() and (err := task.exception()) is not None:
                raise err

    @staticmethod
    async def _close(ws) -> None:
        """Close a connection for real; best effort on an already broken one."""
        try:
            await ws.close()
        except Exception:  # noqa: BLE001
            pass

    async def run(self, ws, on_state, on_error) -> None:
        backoff = RECONNECT_MIN
        while True:
            self._ws = ws
            try:
                await self._pump(ws, on_state)
            except (aiohttp.ClientError, asyncio.TimeoutError, OSError) as err:
                _LOGGER.warning("bridge connection lost: %s", err)
            except Exception:  # noqa: BLE001 — a dead reader must never freeze the feed
                _LOGGER.exception("bridge reader failed")
            else:
                _LOGGER.warning("bridge stopped sending, reconnecting")
            finally:
                # Never leave the old socket behind: abandoned connections
                # are what put two dead half-sockets on the bridge (the
                # 2026-11-03 zombies) and they keep costing it bandwidth.
                await self._close(ws)
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
