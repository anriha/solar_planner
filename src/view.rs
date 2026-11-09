//! The browser's projection of the plan: **one flat payload**, pre-computed.
//!
//! The wasm viewer must not parse [`crate::planner::Plan`] or [`crate::protocol::StatePayload`].
//! Those types carry decisions the browser has no business re-deriving — the metered AC power
//! of an MTU is `ac_kwh / hours`, the bar direction is the sign of that number, and the human
//! reason is a match on `Reason`. Every one of those is a chance for the viewer to disagree
//! with the plan it is displaying, so they are computed once here and shipped as numbers.
//!
//! The flat shape also keeps the viewer testable against a file: `tests/fixtures/view.json` is
//! written from a fixture plan, and the viewer's own test parses the same file (board-zpe).
//!
//! # What is *not* projected yet
//!
//! `ac_actual_kw`, `soc_actual_pct` and `adherence` are the measured side of the ledger
//! (board-aru). They are `None`/empty until that actor records per-MTU actuals; the fields
//! exist now so the viewer's contract does not change when it lands.

use std::collections::BTreeMap;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use crate::ledger::LedgerRow;
use crate::planner::{Action, CAPACITY_KWH, Plan, PlanStep};

/// The longest window a `get_view` will answer, in seconds (60 civil days).
///
/// Server-side cap, not a client courtesy: a request walking the whole ledger would spend
/// the bridge's minute on one frame. Longer requests are **clamped**, never rejected — a
/// viewer asking for a quarter gets the last 60 days and the window it actually received.
pub const MAX_WINDOW_SECS: i64 = 60 * 24 * 60 * 60;

/// The flat, pre-projected plan the browser renders.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    /// Unix seconds the plan behind this view was computed; `None` when no plan is published
    /// (planner off, or no horizon yet) — the viewer draws an empty chart rather than a stale one.
    pub computed_at: Option<i64>,
    /// The server's clock at projection time. The "now" marker uses this, so a browser with a
    /// skewed clock cannot misplace the line.
    pub now: i64,
    /// The window actually served, after clamping — may be narrower than requested.
    pub from: i64,
    pub to: i64,
    pub steps: Vec<ViewStep>,
    pub days: Vec<DayTotals>,
    /// Per-MTU planned-vs-actual. Empty until the ledger records actuals (board-aru).
    pub adherence: Vec<AdherenceRow>,
}

/// One MTU, already reduced to what a chart needs.
#[derive(Debug, Clone, Serialize)]
pub struct ViewStep {
    pub start: i64,
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
    /// Machine-readable tag beside [`Self::reason`], so a viewer can filter without parsing prose.
    pub reason_kind: &'static str,
    /// Metered AC power for the MTU once the ledger has it (board-aru).
    pub ac_actual_kw: Option<f64>,
    /// Measured state of charge at the MTU boundary (board-aru).
    pub soc_actual_pct: Option<f64>,
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

/// Planned vs actual power for one MTU (board-aru). The adherence strip colours
/// `|delta_kw|`.
#[derive(Debug, Clone, Serialize)]
pub struct AdherenceRow {
    pub start: i64,
    pub planned_kw: f64,
    pub actual_kw: f64,
    pub delta_kw: f64,
}

impl View {
    /// Project `plan` onto the flat payload, clipped to the requested window.
    ///
    /// `from`/`to` are unix seconds and both optional: omitted, the window is the plan's own
    /// horizon. `now` is passed in so the caller decides what "now" means (server clock in
    /// production, a fixed instant in tests).
    pub fn of_plan(
        plan: Option<&Plan>,
        from: Option<i64>,
        to: Option<i64>,
        now: i64,
        rows: &[LedgerRow],
    ) -> View {
        let (from, to) = window_for(plan, from, to, now);
        let zone = TimeZone::get("Europe/Prague").expect("the Prague zone is compiled in");
        let mut steps = Vec::new();
        let mut days: BTreeMap<String, DayTotals> = BTreeMap::new();

        for step in plan
            .into_iter()
            .flat_map(|plan| plan.steps.iter())
            .filter(|step| step.mtu.start >= from && step.mtu.start < to)
        {
            let projected = project(step);
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

        // The ledger's half: fold each closed MTU into the step it belongs to, so the chart
        // can draw the meter against the plan, and keep a row for the windows that hold
        // history the current plan no longer covers.
        let window: Vec<&LedgerRow> = rows
            .iter()
            .filter(|row| row.start >= from && row.start < to)
            .collect();
        let mut by_start: BTreeMap<i64, &LedgerRow> =
            window.iter().map(|row| (row.start, *row)).collect();
        for step in &mut steps {
            let Some(row) = by_start.remove(&step.start) else {
                continue;
            };
            if step.hours > 0.0 {
                step.ac_actual_kw = Some(round2(row.actual.ac_kwh / step.hours));
            }
            step.soc_actual_pct = row.actual.soc_end_pct.map(round1);
        }

        let mut adherence: Vec<AdherenceRow> = window
            .iter()
            .map(|row| {
                let actual_kw = round2(row.actual.ac_kwh / row.hours);
                let planned_kw = planned_kw(row);
                AdherenceRow {
                    start: row.start,
                    planned_kw,
                    actual_kw,
                    delta_kw: round2(actual_kw - planned_kw),
                }
            })
            .collect();
        adherence.sort_by_key(|row| row.start);

        View {
            computed_at: plan.map(|plan| plan.computed_at),
            now,
            from,
            to,
            steps,
            days,
            adherence,
        }
    }
}

/// The metered power the plan expected for a closed MTU: the signed `ac_kwh` the DP priced,
/// which is what the actual is compared against. A row from outside the current plan's
/// horizon carries its own copy of the projection.
fn planned_kw(row: &LedgerRow) -> f64 {
    match &row.plan {
        Some(plan) if row.hours > 0.0 => round2(plan.ac_kwh / row.hours),
        _ => 0.0,
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

fn project(step: &PlanStep) -> ViewStep {
    let hours = step.mtu.hours.get();
    let (kind, command_kw) = match step.action {
        Action::Idle => ("idle", 0.0),
        Action::Export { kw } => ("export", kw.get()),
        Action::Import { kw } => ("import", -kw.get()),
    };
    let ac_kw = if hours > 0.0 {
        step.ac_kwh.get() / hours
    } else {
        0.0
    };
    ViewStep {
        start: step.mtu.start,
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
        ac_actual_kw: None,
        soc_actual_pct: None,
    }
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
