"""`plan_view` against payloads shaped exactly like `Plan` in `src/planner.rs`.

No Home Assistant, no config entry, no network: the merge and the wording are imported
straight from the component directory, which is why `plan_view` keeps its HA imports at
zero. Run with `just test-ha` (stdlib unittest; no extra dependency).
"""

from __future__ import annotations

import sys
import unittest
from datetime import timedelta, timezone
from pathlib import Path

COMPONENT = Path(__file__).resolve().parent.parent / "custom_components" / "solar_bridge"
sys.path.insert(0, str(COMPONENT))

import plan_view  # the line above is what makes this work

T0 = 1_793_746_800  # 2026-11-04T00:00 Prague (CET)


def step(start: int, kind: str, kw: float, net: float, energy: float, reason: str,
         hours: float = 0.25, ac: float | None = None) -> dict:
    """One `PlanStep`, serialised the way `serde` serialises it."""
    action = {"kind": kind} if kind == "idle" else {"kind": kind, "kw": kw}
    if ac is None:
        power = 0.0 if kind == "idle" else kw * hours * (1 if kind == "export" else -1)
        ac = power
    return {
        "mtu": {"start": start, "hours": hours},
        "action": action,
        "battery_out_kwh": 0.0,
        "ac_kwh": ac,
        "energy_after_kwh": energy,
        "net_czk": net,
        "marginal_czk_per_kwh": 5.32,
        "reason": {"kind": reason},
    }


def plan(*steps: dict, net: float = 0.0) -> dict:
    ends = [s["mtu"]["start"] + 900 for s in steps if "mtu" in s]
    return {
        "computed_at": T0 - 60,
        "steps": list(steps),
        "expected_net_czk": net,
        "horizon_end": max(ends) if ends else T0,
    }


class MergeTest(unittest.TestCase):
    def test_consecutive_equal_actions_become_one_run_with_summed_money(self) -> None:
        runs = plan_view.segments(
            plan(
                step(T0, "export", 10.8, 13.47, 21.4, "price_above_stored_value"),
                step(T0 + 900, "export", 10.8, 19.80, 22.9, "price_above_stored_value"),
                step(T0 + 1800, "export", 10.8, 9.20, 24.6, "pv_cannot_be_stored"),
            )
        )
        self.assertEqual(len(runs), 1)
        run = runs[0]
        self.assertEqual((run.start, run.end), (T0, T0 + 2700))
        self.assertEqual(run.mtus, 3)
        self.assertAlmostEqual(run.net_czk, 42.47)
        self.assertAlmostEqual(run.ac_kwh, 10.8 * 0.75)
        # Two reasons, in order of first appearance, not three times the last one.
        self.assertEqual(run.reasons, ("price_above_stored_value", "pv_cannot_be_stored"))

    def test_a_different_power_starts_a_new_run(self) -> None:
        runs = plan_view.segments(
            plan(
                step(T0, "export", 10.8, 13.47, 21.4, "price_above_stored_value"),
                step(T0 + 900, "export", 9.6, 12.10, 19.8, "price_above_stored_value"),
            )
        )
        self.assertEqual([round(run.kw, 1) for run in runs], [10.8, 9.6])

    def test_a_gap_in_the_timeline_does_not_merge(self) -> None:
        runs = plan_view.segments(
            plan(
                step(T0, "import", 12.0, -13.59, 18.0, "buy_below_future_value"),
                step(T0 + 1800, "import", 12.0, -13.59, 21.0, "buy_below_future_value"),
            )
        )
        self.assertEqual(len(runs), 2)

    def test_a_step_without_an_mtu_is_skipped_rather_than_guessed(self) -> None:
        broken = {"action": {"kind": "export", "kw": 1.0}, "net_czk": 1.0}
        runs = plan_view.segments(plan(step(T0, "export", 1.2, 1.0, 20.0, "idle"), broken))
        self.assertEqual(len(runs), 1)


class WordingTest(unittest.TestCase):
    def test_export_run_reads_as_sell_and_reports_the_price_it_achieves(self) -> None:
        run = plan_view.segments(
            plan(step(T0, "export", 10.8, 16.2, 24.6, "price_above_stored_value"))
        )[0]
        self.assertEqual(run.summary, "Sell 10.8 kW")
        self.assertAlmostEqual(run.price_czk_per_kwh, 16.2 / 2.7)
        self.assertIn("2.7 kWh", run.description)
        self.assertIn("+16.20 CZK", run.description)
        self.assertIn("price above stored value", run.description)

    def test_import_run_reads_as_buy(self) -> None:
        run = plan_view.segments(
            plan(step(T0, "import", 12.0, -13.50, 20.5, "buy_below_future_value", hours=0.25))
        )[0]
        self.assertEqual(run.summary, "Buy 12.0 kW")
        self.assertAlmostEqual(run.price_czk_per_kwh, 13.50 / 3.0)
        self.assertIn("-13.50 CZK", run.description)

    def test_energy_is_what_the_meter_took_not_what_the_action_offered(self) -> None:
        # A 12 kW import offer the pack can only partly absorb: 0.95 efficiency and the
        # BMS limit cap the charge, so 2.718 kWh crosses the meter, not 3.0.
        run = plan_view.segments(
            plan(step(T0, "import", 12.0, -13.59, 20.5, "buy_below_future_value", ac=-2.718))
        )[0]
        self.assertEqual(run.summary, "Buy 12.0 kW")
        self.assertAlmostEqual(run.traded_kwh, 2.718)
        self.assertAlmostEqual(run.price_czk_per_kwh, 5.0, places=2)
        self.assertIn("2.7 kWh bought", run.description)

    def test_a_payload_without_metered_ac_still_renders(self) -> None:
        # A bridge older than `ac_kwh`: the target is the only energy figure there is.
        legacy = plan(step(T0, "export", 10.8, 13.47, 21.4, "price_above_stored_value"))
        del legacy["steps"][0]["ac_kwh"]
        run = plan_view.segments(legacy)[0]
        self.assertAlmostEqual(run.traded_kwh, 2.7)
        self.assertEqual(run.summary, "Sell 10.8 kW")

    def test_idle_run_is_a_hold_with_no_traded_energy(self) -> None:
        run = plan_view.segments(plan(step(T0, "idle", 0.0, 0.0, 20.0, "idle")))[0]
        self.assertEqual(run.summary, "Hold")
        self.assertEqual(run.ac_kwh, 0.0)
        self.assertIsNone(run.price_czk_per_kwh)
        self.assertNotIn("CZK/kWh", run.description)


class TimelineTest(unittest.TestCase):
    def test_the_run_under_the_clock_is_the_current_one(self) -> None:
        runs = plan(
            step(T0, "import", 12.0, -13.59, 18.0, "buy_below_future_value"),
            step(T0 + 900, "export", 10.8, 13.47, 21.4, "price_above_stored_value"),
        )
        self.assertIsNone(plan_view.at(runs, T0 - 1))
        self.assertEqual(plan_view.at(runs, T0 + 900).kind, "export")
        self.assertIsNone(plan_view.at(runs, T0 + 1800))

    def test_upcoming_returns_the_run_in_progress_or_the_next(self) -> None:
        plan_ = plan(
            step(T0, "import", 12.0, -13.59, 18.0, "buy_below_future_value"),
            step(T0 + 3600, "export", 10.8, 13.47, 21.4, "price_above_stored_value"),
        )
        self.assertEqual(plan_view.upcoming(plan_, T0 + 60).kind, "import")
        self.assertEqual(plan_view.upcoming(plan_, T0 + 1800).kind, "export")
        self.assertIsNone(plan_view.upcoming(plan_, T0 + 7200))

    def test_days_roll_up_across_local_midnight(self) -> None:
        # 23:30 on 2026-11-04 through 00:15 on 2026-11-05, Prague time.
        late = T0 + 23 * 3600 + 1800
        runs = plan_view.segments(
            plan(
                step(late, "export", 10.8, 10.0, 24.0, "price_above_stored_value"),
                step(late + 900, "export", 10.8, 9.0, 23.0, "price_above_stored_value"),
                step(late + 1800, "import", 12.0, -13.5, 20.0, "buy_below_future_value"),
            )
        )
        days = plan_view.by_day(runs, timezone(timedelta(hours=1)))
        self.assertEqual([day["date"] for day in days], ["2026-11-04", "2026-11-05"])
        self.assertAlmostEqual(days[0]["sell_kwh"], 10.8 * 0.5)
        self.assertAlmostEqual(days[0]["net_czk"], 19.0)
        self.assertAlmostEqual(days[1]["buy_kwh"], 3.0)
        self.assertAlmostEqual(days[1]["net_czk"], -13.5)

    def test_no_steps_is_an_empty_plan_not_an_error(self) -> None:
        self.assertEqual(plan_view.segments(plan()), [])
        self.assertIsNone(plan_view.at(plan(), T0))
        self.assertIsNone(plan_view.upcoming(plan(), T0))


if __name__ == "__main__":
    unittest.main()
