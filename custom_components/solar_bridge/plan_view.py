"""Pure rendering of the bridge's plan payload.

`calendar.py` and `sensor.py` need the same thing twice — the plan cut into runs of equal
action and worded for a human — so the cut and the wording live here, away from Home
Assistant. `tests/test_plan_view.py` imports this module directly, with no HA install and
no config entry: the merge is the part that can silently lie on screen, so it is the part
that is tested.

The payload is `Plan` from `src/planner.rs`, one entry per 15-minute market time unit:

    {"computed_at": 1793922777, "horizon_end": 1794010800, "expected_net_czk": 356.44,
     "steps": [{"mtu": {"start": 1793789100, "hours": 0.25},
                "action": {"kind": "export", "kw": 10.8},
                "battery_out_kwh": 2.7, "ac_kwh": 2.7, "energy_after_kwh": 21.4,
                "net_czk": 13.47, "marginal_czk_per_kwh": 5.32,
                "reason": {"kind": "price_above_stored_value", ...}}, ...]}
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass, replace
from typing import Any

# `Reason` in `src/planner.rs`, as the plan explains itself.
REASON_TEXT = {
    "price_above_stored_value": "price above stored value",
    "pv_cannot_be_stored": "PV cannot be stored",
    "buy_below_future_value": "buy below future value",
    "hold_for_later": "holding for a better price",
    "idle": "nothing to trade",
}

ACTION_TEXT = {"export": "Sell", "import": "Buy", "idle": "Hold"}


@dataclass(frozen=True)
class Segment:
    """A run of consecutive MTUs the planner wants to do the same thing in.

    `kw` is the grid-facing target and `net_czk` is summed over the run, so the figures
    describe the whole run rather than its last quarter hour.
    """

    start: int
    end: int
    kind: str
    kw: float
    mtus: int
    net_czk: float
    battery_out_kwh: float
    #: Metered AC over the run, signed: positive exported, negative imported.
    ac_kwh: float
    energy_after_kwh: float
    reasons: tuple[str, ...]

    @property
    def hours(self) -> float:
        return (self.end - self.start) / 3600.0

    @property
    def traded_kwh(self) -> float:
        return abs(self.ac_kwh)

    @property
    def price_czk_per_kwh(self) -> float | None:
        # Exact by construction: money is linear in metered AC, so the quotient is the
        # price the run actually achieves. `net` is revenue minus cost, hence the sign.
        traded = self.traded_kwh
        return abs(self.net_czk) / traded if traded > 0 else None

    @property
    def summary(self) -> str:
        if self.kind == "idle":
            return "Hold"
        return f"{ACTION_TEXT.get(self.kind, self.kind)} {self.kw:.1f} kW"

    @property
    def description(self) -> str:
        parts = [_duration(self.end - self.start)]
        if self.kind != "idle" and self.traded_kwh > 0:
            direction = "sold" if self.kind == "export" else "bought"
            parts.append(f"{self.traded_kwh:.1f} kWh {direction}")
            if (price := self.price_czk_per_kwh) is not None:
                parts.append(f"{price:.2f} CZK/kWh")
        parts.append(f"net {self.net_czk:+.2f} CZK")
        parts.append(f"pack {self.energy_after_kwh:.1f} kWh")
        parts.append("; ".join(_reason_text(r) for r in self.reasons))
        return " · ".join(parts)


def _metered_ac(step: dict[str, Any], kind: str, kw: float, seconds: int) -> float:
    """Metered AC for one MTU, signed.

    Bridges that predate `ac_kwh` only sent the target, so the fallback assumes the whole
    target crossed the meter — exact for an export, an over-estimate for an import.
    """
    if (value := step.get("ac_kwh")) is not None:
        return float(value)
    if kind == "idle":
        return 0.0
    energy = kw * seconds / 3600.0
    return energy if kind == "export" else -energy


def _duration(seconds: int) -> str:
    minutes = round(seconds / 60)
    hours, minutes = divmod(minutes, 60)
    if hours and minutes:
        return f"{hours} h {minutes} min"
    if hours:
        return f"{hours} h"
    return f"{minutes} min"


def _reason_text(kind: str) -> str:
    return REASON_TEXT.get(kind, kind.replace("_", " "))


def segments(plan: dict[str, Any]) -> list[Segment]:
    """Merge consecutive MTUs with the same action into one run.

    An MTU whose figures are missing is skipped rather than guessed: a silently wrong
    event on a calendar is worse than a gap.
    """
    runs: list[Segment] = []
    for step in plan.get("steps") or ():
        mtu = step.get("mtu") or {}
        if "start" not in mtu:
            continue
        start = int(mtu["start"])
        end = start + round(float(mtu.get("hours", 0.25)) * 3600)
        action = step.get("action") or {}
        kind = action.get("kind") or "idle"
        kw = round(float(action.get("kw") or 0.0), 3)
        reason = (step.get("reason") or {}).get("kind") or "idle"
        net = float(step.get("net_czk") or 0.0)
        out_kwh = float(step.get("battery_out_kwh") or 0.0)
        energy = float(step.get("energy_after_kwh") or 0.0)
        ac = _metered_ac(step, kind, kw, end - start)

        if runs and runs[-1].kind == kind and runs[-1].kw == kw and runs[-1].end == start:
            previous = runs[-1]
            runs[-1] = replace(
                previous,
                end=end,
                mtus=previous.mtus + 1,
                net_czk=previous.net_czk + net,
                battery_out_kwh=previous.battery_out_kwh + out_kwh,
                ac_kwh=previous.ac_kwh + ac,
                energy_after_kwh=energy,  # the pack at the end of the run
                reasons=tuple(dict.fromkeys((*previous.reasons, reason))),
            )
        else:
            runs.append(
                Segment(
                    start=start,
                    end=end,
                    kind=kind,
                    kw=kw,
                    mtus=1,
                    net_czk=net,
                    battery_out_kwh=out_kwh,
                    ac_kwh=ac,
                    energy_after_kwh=energy,
                    reasons=(reason,),
                )
            )
    return runs


def at(plan: dict[str, Any], now: float) -> Segment | None:
    """The run the clock is inside, if any."""
    for run in segments(plan):
        if run.start <= now < run.end:
            return run
    return None


def upcoming(plan: dict[str, Any], now: float) -> Segment | None:
    """The run in progress or the next one — what a dashboard wants in one line."""
    for run in segments(plan):
        if run.end > now:
            return run
    return None


def by_day(runs: Iterable[Segment], tz) -> list[dict[str, Any]]:
    """Per local day: how much is sold, how much is bought, what it nets.

    The calendar answers "what at 12:00"; this answers "what does the day come to",
    which is the only way the estimate stays readable once the plan has a few hundred
    quarter hours in it.
    """
    days: dict[str, dict[str, Any]] = {}
    for run in runs:
        day = _local_date(run.start, tz)
        bucket = days.setdefault(
            day,
            {"date": day, "sell_kwh": 0.0, "buy_kwh": 0.0, "net_czk": 0.0, "segments": 0},
        )
        if run.kind == "export":
            bucket["sell_kwh"] += run.traded_kwh
        elif run.kind == "import":
            bucket["buy_kwh"] += run.traded_kwh
        bucket["net_czk"] += run.net_czk
        bucket["segments"] += 1
    return [
        {**bucket, "sell_kwh": round(bucket["sell_kwh"], 2), "buy_kwh": round(bucket["buy_kwh"], 2),
         "net_czk": round(bucket["net_czk"], 2)}
        for bucket in days.values()
    ]


def _local_date(timestamp: int, tz) -> str:
    from datetime import datetime

    return datetime.fromtimestamp(timestamp, tz).date().isoformat()
