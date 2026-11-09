//! The plan as **one flat payload**, pre-computed, served at `/plan.json`.
//!
//! A dashboard must not have to parse [`crate::planner::Plan`] or
//! [`crate::protocol::StatePayload`]. Those types carry decisions a chart has no business
//! re-deriving — the metered AC power of an MTU is `ac_kwh / hours`, the bar direction is the
//! sign of that number, the human reason is a match on `Reason` — and each is a chance to
//! disagree with the plan being displayed. They are computed once here and shipped as numbers.
//!
//! The flat shape also keeps the contract testable against a file: `tests/fixtures/view.json`
//! is written from a fixture plan and compared byte for byte on every run.
//!
//! # What this is not
//!
//! The measured side of the plant is **not** projected here. Energy, power and money that
//! actually happened live in Prometheus (`/metrics`, scraped from this same binary) and are
//! graphed in Grafana; this payload is the *plan*, which no time-series store can hold because
//! it is a mutable future timeline. A row that mixed the two would have to be reconciled with a
//! second truth on every replan.

use std::collections::BTreeMap;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use crate::planner::{CAPACITY_KWH, Plan, PlanStep};

/// The longest window `/plan.json` will answer, in seconds (60 civil days).
///
/// Server-side cap, not a caller courtesy: an unclamped request would let one query ask for a
/// decade and spend the bridge's minute on a single frame. Longer requests are **clamped**,
/// never rejected — the reply echoes the window it actually served.
pub const MAX_WINDOW_SECS: i64 = 60 * 24 * 60 * 60;

/// The flat, pre-projected plan the browser renders.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    /// Unix seconds the plan behind this view was computed; `None` when no plan is published
    /// (planner off, or no horizon yet) — a chart draws nothing rather than something stale.
    pub computed_at: Option<i64>,
    /// The server's clock at projection time. The "now" marker uses this, so a browser with a
    /// skewed clock cannot misplace the line.
    pub now: i64,
    /// The window actually served, after clamping — may be narrower than requested.
    pub from: i64,
    pub to: i64,
    pub steps: Vec<ViewStep>,
    pub days: Vec<DayTotals>,
}

/// One MTU, already reduced to what a chart needs.
#[derive(Debug, Clone, Serialize)]
pub struct ViewStep {
    pub start: i64,
    /// `start` as ISO 8601 in the plant's zone, `2026-11-09T20:05:00+01:00`.
    ///
    /// Carried because a JSON datasource cannot type an epoch column as time without the
    /// caller guessing the unit, and because a human reading `/plan.json` by hand should not
    /// have to convert 1 791 232 800 in their head. `start` stays for anything that does the
    /// arithmetic.
    pub at: String,
    pub hours: f64,
    pub price_sell: f64,
    pub price_buy: f64,
    pub pv_kw: f64,
    /// AC power that crossed the meter, **signed**: positive exported, negative imported.
    /// Derived from `ac_kwh / hours`, never from the command — an export target of 8.4 kW
    /// metered 7.9 kW on 2026-11-08, and an import command of 9.6 kW metered 7.8 kW. Bar
    /// lengths come from this number.
    pub ac_kw: f64,
    /// What the DP commanded, signed the same way. A **marker**, not a bar length: the
    /// command is an offer the plant may accept only partly (see [`Self::ac_kw`]).
    pub command_kw: f64,
    /// `idle` | `export` | `import` — the DP's decision, for colouring and filters.
    pub kind: &'static str,
    /// Planned state of charge at the end of the MTU, percent of nameplate.
    pub soc_end_pct: f64,
    pub net_czk: f64,
    /// The decision in words, e.g. `sell 7.90 > stored 5.32 CZK/kWh`.
    pub reason: String,
    /// Machine-readable tag beside [`Self::reason`], so a chart can filter without parsing prose.
    pub reason_kind: &'static str,
}

/// What one Prague day comes to — the only readable summary once the horizon holds a few
/// hundred quarter hours.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DayTotals {
    /// ISO local date, `2026-11-04`.
    pub day: String,
    pub sell_kwh: f64,
    pub buy_kwh: f64,
    pub pv_kwh: f64,
    pub net_czk: f64,
    pub mtus: usize,
}

impl View {
    /// Project `plan` onto the flat payload, clipped to the requested window.
    ///
    /// `from`/`to` are unix seconds and both optional: omitted, the window is the plan's own
    /// horizon. `now` is passed in so the caller decides what "now" means (server clock in
    /// production, a fixed instant in tests).
    pub fn of_plan(plan: Option<&Plan>, from: Option<i64>, to: Option<i64>, now: i64) -> View {
        let (from, to) = window_for(plan, from, to, now);
        let zone = TimeZone::get("Europe/Prague").expect("the Prague zone is compiled in");
        let mut steps = Vec::new();
        let mut days: BTreeMap<String, DayTotals> = BTreeMap::new();

        for step in plan
            .into_iter()
            .flat_map(|plan| plan.steps.iter())
            .filter(|step| step.mtu.start >= from && step.mtu.start < to)
        {
            let projected = project(step, &zone);
            // Classified by what crossed the meter, not by the action: a hold that sells its
            // surplus is a sale, and its money lands in the same bucket as its kWh.
            let day = local_date(step.mtu.start, &zone);
            let bucket = days.entry(day.clone()).or_insert_with(|| DayTotals {
                day,
                ..DayTotals::default()
            });
            if step.ac_kwh.get() > 0.0 {
                bucket.sell_kwh += step.ac_kwh.get();
            } else {
                bucket.buy_kwh -= step.ac_kwh.get();
            }
            bucket.pv_kwh += step.pv_kw.get() * step.mtu.hours.get();
            bucket.net_czk += step.net_czk.get();
            bucket.mtus += 1;
            steps.push(projected);
        }

        let days = days
            .into_values()
            .map(|day| DayTotals {
                sell_kwh: round2(day.sell_kwh),
                buy_kwh: round2(day.buy_kwh),
                pv_kwh: round2(day.pv_kwh),
                net_czk: round2(day.net_czk),
                ..day
            })
            .collect();

        View {
            computed_at: plan.map(|plan| plan.computed_at),
            now,
            from,
            to,
            steps,
            days,
        }
    }
}

/// The served window: the requested one (defaulting to the plan's horizon), clamped to
/// [`MAX_WINDOW_SECS`].
pub fn window_for(plan: Option<&Plan>, from: Option<i64>, to: Option<i64>, now: i64) -> (i64, i64) {
    let plan_from = plan
        .and_then(|plan| plan.steps.first())
        .map(|step| step.mtu.start);
    let plan_to = plan.map(|plan| plan.horizon_end);
    let from = from.or(plan_from).unwrap_or(now);
    let to = to.or(plan_to).unwrap_or(now);
    (from.max(to - MAX_WINDOW_SECS), to)
}

fn project(step: &PlanStep, zone: &TimeZone) -> ViewStep {
    let hours = step.mtu.hours.get();
    let kind = step.action.kind();
    let command_kw = step.action.signed_kw();
    let ac_kw = if hours > 0.0 {
        step.ac_kwh.get() / hours
    } else {
        0.0
    };
    ViewStep {
        start: step.mtu.start,
        at: iso_local(step.mtu.start, zone),
        hours,
        price_sell: round3(step.price_sell.get()),
        price_buy: round3(step.price_buy.get()),
        pv_kw: round2(step.pv_kw.get()),
        ac_kw: round2(ac_kw),
        command_kw: round2(command_kw),
        kind,
        soc_end_pct: round1(step.energy_after_kwh.get() / CAPACITY_KWH * 100.0),
        net_czk: round2(step.net_czk.get()),
        reason: step.reason.describe(),
        reason_kind: step.reason.kind(),
    }
}

fn iso_local(unix: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(unix)
        .expect("planner timestamps are in range")
        .to_zoned(zone.clone())
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

fn local_date(unix: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(unix)
        .expect("planner timestamps are in range")
        .to_zoned(zone.clone())
        .date()
        .to_string()
}

/// Presentation rounding. A chart cannot show more than this, and the raw `f64` from a
/// division (`2.712345678901234`) makes the pinned fixture unreadable.
fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
