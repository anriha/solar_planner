//! Prometheus exposition: what the dashboards read out of the plant, per scrape.
//!
//! # Why this exists at all
//!
//! Grafana owns the surfaces (2026-11-09 decision). Prometheus can already see the inverter's
//! odometers through Home Assistant's own `/api/prometheus`, but three things the dashboards
//! need **do not exist anywhere but here**: the prices the plan is priced with, the PV belief
//! the planner acted on, and the plan's own command. Without them a solar dashboard can show
//! energy and no money.
//!
//! # Shape
//!
//! * **Counters** are the inverter's own odometers, passed through unchanged. The register
//!   resolves 0.1 kWh, so `increase()` over any window is exact to that step — no integration,
//!   no restart loss, no drift.
//! * **Gauges** are instantaneous (power, SOC) or step functions (prices, changed once per
//!   15-minute MTU). A price is *not* a counter: it moves up and down, and a per-MTU series
//!   would create 35 000 series a year.
//! * **Nothing is emitted for a missing value.** A renamed register or a withdrawn plan makes
//!   its metric disappear, which is visible in Grafana, rather than appear as a zero — the
//!   lesson of register 619, which read ~0 W through a whole export day and would have made
//!   every MTU look idle.
//!
//! The endpoint is deliberately unauthenticated like `/state` and `/meta`; the plant's numbers
//! are not a secret and Prometheus scrapes over the tailnet. The token still guards `/ws`,
//! which is the only interface that can move an actuator.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::decode::Value;
use crate::planner::{CAPACITY_KWH, Plan, PlanStep};
use crate::protocol::StatePayload;

/// Registers that are cumulative energy and become `# TYPE counter`.
const COUNTERS: [(&str, &str, &str); 6] = [
    (
        "pv_energy_total",
        "solar_pv_energy_kwh_total",
        "DC energy the arrays produced, cumulative (register 534)",
    ),
    (
        "grid_import_total",
        "solar_grid_import_kwh_total",
        "AC energy imported from the grid, cumulative (register 522)",
    ),
    (
        "grid_export_total",
        "solar_grid_export_kwh_total",
        "AC energy exported to the grid, cumulative (register 524)",
    ),
    (
        "battery_charge_total",
        "solar_battery_charge_kwh_total",
        "DC energy into the pack, cumulative (register 516)",
    ),
    (
        "battery_discharge_total",
        "solar_battery_discharge_kwh_total",
        "DC energy out of the pack, cumulative (register 518)",
    ),
    (
        "load_energy_total",
        "solar_load_energy_kwh_total",
        "AC energy the load port carried, cumulative (register 527). Read ~0 since 2026-10-29 — see the design note",
    ),
];

/// Registers that are instantaneous readings and become `# TYPE gauge`.
const GAUGES: [(&str, &str, &str); 7] = [
    (
        "battery_soc",
        "solar_battery_soc_percent",
        "State of charge (register 588)",
    ),
    (
        "grid_internal_power_total",
        "solar_grid_power_w",
        "Grid port power, **import positive / export negative** (register 607, the meter)",
    ),
    (
        "battery_power",
        "solar_battery_power_w",
        "Pack power, positive charging (register 590)",
    ),
    (
        "load_power_total",
        "solar_load_power_w",
        "Load-port power (register 653)",
    ),
    (
        "inverter_power_total",
        "solar_inverter_power_w",
        "Inverter AC output (register 636)",
    ),
    (
        "battery_temperature",
        "solar_battery_temperature_celsius",
        "Battery temperature (register 586)",
    ),
    (
        "dc_temperature",
        "solar_dc_temperature_celsius",
        "DC-side temperature (register 540)",
    ),
];

/// The PV strings that make up [`solar_pv_power_w`].
const PV_POWER: [&str; 2] = ["pv1_power", "pv2_power"];

/// Render one scrape. `now` is passed in rather than read, so the output is a function of its
/// input and a test can pin every line.
pub fn render(state: &StatePayload, now: i64) -> String {
    let mut out = String::with_capacity(4096);

    for (register, metric, help) in COUNTERS {
        if let Some(value) = number(&state.values, register) {
            family(&mut out, metric, "counter", help, value);
        }
    }

    for (register, metric, help) in GAUGES {
        if let Some(value) = number(&state.values, register) {
            family(&mut out, metric, "gauge", help, value);
        }
    }

    // The two strings summed: a per-string series would be a second way to say the same
    // thing, and the planner only ever sees the total.
    let pv: f64 = PV_POWER
        .iter()
        .filter_map(|register| number(&state.values, register))
        .sum();
    if PV_POWER
        .iter()
        .any(|register| number(&state.values, register).is_some())
    {
        family(
            &mut out,
            "solar_pv_power_w",
            "gauge",
            "Forecast-metered DC power from both MPPTs (registers 672 + 673)",
            pv,
        );
    }

    // --- the plan's side -------------------------------------------------------------
    if let Some(plan) = state.plan.as_ref() {
        family(
            &mut out,
            "solar_plan_age_seconds",
            "gauge",
            "Seconds since the plan was computed",
            (now - plan.computed_at) as f64,
        );
        family(
            &mut out,
            "solar_plan_net_czk_horizon",
            "gauge",
            "Money the whole horizon is expected to make (CZK)",
            plan.expected_net_czk.get(),
        );

        if let Some(step) = current_step(plan, now) {
            // A step function, not a series per MTU: the value changes at the boundary and
            // Prometheus stores the step.
            family(
                &mut out,
                "solar_price_buy_czk_per_kwh",
                "gauge",
                "Price paid per imported kWh in the MTU in force, spot × VAT + charges",
                step.price_buy.get(),
            );
            family(
                &mut out,
                "solar_price_sell_czk_per_kwh",
                "gauge",
                "Price received per exported kWh in the MTU in force, spot - sell fee",
                step.price_sell.get(),
            );
            family(
                &mut out,
                "solar_forecast_pv_kw",
                "gauge",
                "The PV forecast the plan is acting on for the MTU in force, DC",
                step.pv_kw.get(),
            );
            family(
                &mut out,
                "solar_plan_power_kw",
                "gauge",
                "AC power the plan commands for the MTU in force, export positive",
                step.action.signed_kw(),
            );
        }

        if let Some(remaining) = forecast_remaining_today(plan, now) {
            family(
                &mut out,
                "solar_forecast_pv_kwh_remaining_today",
                "gauge",
                "Forecast DC energy still expected before the next Prague midnight",
                remaining,
            );
        }

        // The horizon's own aggregates, recorded once per scrape. This is the whole of the
        // plan's history: the per-step timeline is a document (it changes with every replan
        // and Prometheus cannot hold a mutable future), but the *belief about the rest of the
        // day* is one number per question, and a series of it is what "how did the plan hold"
        // means. Four fixed series, no labels, nothing per MTU — 96 MTU-label series a day
        // would grow without bound to answer a question nobody asks.
        if let Some(rest) = rest_of_today(plan, now) {
            family(
                &mut out,
                "solar_plan_expected_net_czk_rest_of_day",
                "gauge",
                "Money the plan still expects to make before the next Prague midnight (CZK)",
                rest.net_czk,
            );
            family(
                &mut out,
                "solar_plan_expected_export_kwh_rest_of_day",
                "gauge",
                "AC energy the plan still expects to export before the next Prague midnight",
                rest.export_kwh,
            );
            family(
                &mut out,
                "solar_plan_expected_import_kwh_rest_of_day",
                "gauge",
                "AC energy the plan still expects to import before the next Prague midnight",
                rest.import_kwh,
            );
            family(
                &mut out,
                "solar_plan_expected_soc_end_of_day_pct",
                "gauge",
                "State of charge the plan expects to hold at the next Prague midnight",
                rest.soc_end_pct,
            );
        }
    }

    // --- quality ---------------------------------------------------------------------
    // Without these, a dead RS485 link is indistinguishable from a quiet plant.
    family(
        &mut out,
        "solar_bridge_up",
        "gauge",
        "1 while the bridge answers a scrape",
        1.0,
    );
    family(
        &mut out,
        "solar_link_ok",
        "gauge",
        "1 when the last Modbus poll succeeded, 0 when the link is down",
        if state.link_ok { 1.0 } else { 0.0 },
    );
    if state.ts > 0 {
        family(
            &mut out,
            "solar_state_age_seconds",
            "gauge",
            "Seconds since the last decoded poll",
            (now - state.ts as i64) as f64,
        );
    }

    out
}

/// The MTU in force at `now`, if the plan still covers it.
fn current_step(plan: &Plan, now: i64) -> Option<&PlanStep> {
    plan.steps.iter().find(|step| {
        let width = (step.mtu.hours.get() * 3600.0) as i64;
        now >= step.mtu.start && now < step.mtu.start + width
    })
}

/// Unix seconds of the first Prague midnight after `now`.
fn midnight_after(now: i64) -> Option<i64> {
    let zone = TimeZone::get("Europe/Prague").ok()?;
    let today = Timestamp::from_second(now)
        .ok()?
        .to_zoned(zone.clone())
        .date();
    let tomorrow = today.tomorrow().ok()?;
    Some(
        tomorrow
            .at(0, 0, 0, 0)
            .to_zoned(zone)
            .ok()?
            .timestamp()
            .as_second(),
    )
}

/// The steps of the plan that fall before the next midnight, from `now` on.
///
/// The MTU in force is counted **in full** even though part of it is already spent. That is
/// the same rule the PV forecast uses, and it is the right one here: the plan's `net_czk` for
/// an MTU is not divisible by the minute that has elapsed, so splitting it would invent a
/// number. The cost is at most one MTU of over-count, which is why every help text above says
/// "before the next midnight" rather than "today".
fn steps_remaining_today(plan: &Plan, now: i64) -> Vec<&PlanStep> {
    let Some(midnight) = midnight_after(now) else {
        return Vec::new();
    };
    plan.steps
        .iter()
        .filter(|step| step.mtu.start >= now && step.mtu.start < midnight)
        .collect()
}

/// What the plan still expects of the civil day at `now`.
///
/// Read forwards it is "the rest of today, as currently believed"; read across scrapes it is
/// the revision history of that belief, which is the only honest way to put a mutable future
/// into a time-series store.
struct RestOfDay {
    net_czk: f64,
    export_kwh: f64,
    import_kwh: f64,
    soc_end_pct: f64,
}

fn rest_of_today(plan: &Plan, now: i64) -> Option<RestOfDay> {
    let steps = steps_remaining_today(plan, now);
    if steps.is_empty() {
        return None;
    }
    let mut rest = RestOfDay {
        net_czk: 0.0,
        export_kwh: 0.0,
        import_kwh: 0.0,
        soc_end_pct: 0.0,
    };
    for step in steps {
        rest.net_czk += step.net_czk.get();
        // Classified by what crossed the meter, not by the command: a hold that sells its
        // surplus is an export, and its money lands in the same bucket as its kWh.
        if step.ac_kwh.get() > 0.0 {
            rest.export_kwh += step.ac_kwh.get();
        } else {
            rest.import_kwh -= step.ac_kwh.get();
        }
        rest.soc_end_pct = step.energy_after_kwh.get() / CAPACITY_KWH * 100.0;
    }
    // A sum of `f64`s prints as 6.300000000000001, and this exposition is read by humans as
    // well as scraped; the register itself resolves 0.1 kWh, so a thousandth is generous.
    Some(RestOfDay {
        net_czk: round3(rest.net_czk),
        export_kwh: round3(rest.export_kwh),
        import_kwh: round3(rest.import_kwh),
        soc_end_pct: round3(rest.soc_end_pct),
    })
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Forecast DC energy the plan still expects before the next Prague midnight.
///
/// The horizon is ~35 h, so this is exact for the rest of the day; a step that straddles
/// midnight contributes its whole energy, which is at most one MTU of over-count and is why
/// the help text says "before the next midnight" rather than "today".
fn forecast_remaining_today(plan: &Plan, now: i64) -> Option<f64> {
    let steps = steps_remaining_today(plan, now);
    if steps.is_empty() {
        return None;
    }
    Some(
        steps
            .into_iter()
            .map(|step| step.pv_kw.get() * step.mtu.hours.get())
            .sum(),
    )
}

/// One metric family: the two annotation lines then the sample, in text exposition format.
fn family(out: &mut String, name: &str, kind: &str, help: &str, value: f64) {
    if !value.is_finite() {
        return; // a NaN would poison every aggregate downstream
    }
    // A HELP line cannot carry a newline or a backslash; none of the literals above do, and
    // this is the place that would have to escape them if one ever did.
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name} {value}");
}

fn number(values: &BTreeMap<String, Value>, name: &str) -> Option<f64> {
    match values.get(name) {
        Some(Value::Number(value)) if value.is_finite() => Some(*value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{Action, CAPACITY_KWH, Mtu, Reason};
    use crate::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};

    /// 2026-11-09 12:00 Prague — inside the first MTU of the fixture plan.
    const NOON: i64 = 1_794_222_000;

    fn values(pairs: &[(&str, f64)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), Value::Number(*value)))
            .collect()
    }

    fn step(start: i64, pv_kw: f64, action: Action) -> PlanStep {
        PlanStep {
            mtu: Mtu {
                start,
                hours: Hours::new(0.25),
            },
            price_sell: CzkPerKwh::new(4.94),
            price_buy: CzkPerKwh::new(8.8),
            pv_kw: DcKw::new(pv_kw),
            action,
            battery_out_kwh: DcKwh::new(0.0),
            ac_kwh: AcKwh::new(2.1),
            energy_after_kwh: DcKwh::new(CAPACITY_KWH),
            net_czk: Czk::new(10.0),
            marginal_czk_per_kwh: CzkPerKwh::new(5.0),
            reason: Reason::Idle,
        }
    }

    fn plan() -> Plan {
        Plan {
            computed_at: NOON - 60,
            steps: vec![
                step(NOON, 1.9, Action::Idle),
                step(NOON + 900, 2.4, Action::Export { kw: AcKw::new(8.4) }),
                step(NOON + 1800, 0.0, Action::Import { kw: AcKw::new(9.6) }),
            ],
            expected_net_czk: Czk::new(41.5),
            horizon_end: NOON + 2700,
        }
    }

    fn state() -> StatePayload {
        StatePayload {
            ts: NOON as u64,
            link_ok: true,
            values: values(&[
                ("pv_energy_total", 1112.1),
                ("grid_import_total", 34.2),
                ("grid_export_total", 335.1),
                ("battery_charge_total", 523.4),
                ("battery_discharge_total", 471.3),
                ("load_energy_total", 631.4),
                ("battery_soc", 100.0),
                ("grid_internal_power_total", -8650.0),
                ("battery_power", 6.0),
                ("load_power_total", 0.0),
                ("inverter_power_total", 8600.0),
                ("battery_temperature", 16.5),
                ("dc_temperature", 25.0),
                ("pv1_power", 62.0),
                ("pv2_power", 66.0),
            ]),
            plan: Some(plan()),
        }
    }

    fn line<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        text.lines()
            .find(|line| line.starts_with(&format!("{name} ")))
    }

    #[test]
    fn the_odometers_are_counters_with_their_exact_values() {
        let text = render(&state(), NOON);
        assert_eq!(
            line(&text, "solar_pv_energy_kwh_total"),
            Some("solar_pv_energy_kwh_total 1112.1")
        );
        assert_eq!(
            line(&text, "solar_grid_export_kwh_total"),
            Some("solar_grid_export_kwh_total 335.1")
        );
        assert!(
            text.contains("# TYPE solar_pv_energy_kwh_total counter"),
            "an odometer must not be typed as a gauge"
        );
    }

    #[test]
    fn the_meter_keeps_its_own_sign() {
        let text = render(&state(), NOON);
        // Register 607 is import-positive; the plan is export-positive. This endpoint does
        // not flip it, because two conventions in one system is how a dashboard lies.
        assert_eq!(
            line(&text, "solar_grid_power_w"),
            Some("solar_grid_power_w -8650")
        );
        assert!(text.contains("# TYPE solar_grid_power_w gauge"));
    }

    #[test]
    fn the_two_strings_are_summed_into_one_pv_power() {
        let text = render(&state(), NOON);
        assert_eq!(
            line(&text, "solar_pv_power_w"),
            Some("solar_pv_power_w 128")
        );
    }

    #[test]
    fn the_plan_in_force_gives_prices_forecast_and_command() {
        let text = render(&state(), NOON);
        assert_eq!(
            line(&text, "solar_price_buy_czk_per_kwh"),
            Some("solar_price_buy_czk_per_kwh 8.8")
        );
        assert_eq!(
            line(&text, "solar_price_sell_czk_per_kwh"),
            Some("solar_price_sell_czk_per_kwh 4.94")
        );
        assert_eq!(
            line(&text, "solar_forecast_pv_kw"),
            Some("solar_forecast_pv_kw 1.9")
        );
        // An idle hold commands nothing, and that is a zero, not an absent metric.
        assert_eq!(
            line(&text, "solar_plan_power_kw"),
            Some("solar_plan_power_kw 0")
        );
        assert_eq!(
            line(&text, "solar_plan_net_czk_horizon"),
            Some("solar_plan_net_czk_horizon 41.5")
        );
        assert_eq!(
            line(&text, "solar_plan_age_seconds"),
            Some("solar_plan_age_seconds 60")
        );
    }

    #[test]
    fn an_import_command_reads_negative_and_an_export_positive() {
        let text = render(&state(), NOON + 900);
        assert_eq!(
            line(&text, "solar_plan_power_kw"),
            Some("solar_plan_power_kw 8.4")
        );
        let text = render(&state(), NOON + 1800);
        assert_eq!(
            line(&text, "solar_plan_power_kw"),
            Some("solar_plan_power_kw -9.6")
        );
    }

    #[test]
    fn the_forecast_still_expected_today_sums_only_the_remaining_steps() {
        let text = render(&state(), NOON);
        // 1.9 + 2.4 + 0.0 kW over a quarter hour each = (4.3) * 0.25 = 1.075 kWh.
        assert_eq!(
            line(&text, "solar_forecast_pv_kwh_remaining_today"),
            Some("solar_forecast_pv_kwh_remaining_today 1.075")
        );
        // At the last step, only that step remains.
        let late = render(&state(), NOON + 1800);
        assert_eq!(
            line(&late, "solar_forecast_pv_kwh_remaining_today"),
            Some("solar_forecast_pv_kwh_remaining_today 0")
        );
    }

    #[test]
    fn the_rest_of_the_day_is_the_remaining_steps_summed() {
        let text = render(&state(), NOON);
        // Three fixture MTUs of 2.1 kWh out and 10 CZK each, the last ending at nameplate.
        assert_eq!(
            line(&text, "solar_plan_expected_net_czk_rest_of_day"),
            Some("solar_plan_expected_net_czk_rest_of_day 30")
        );
        assert_eq!(
            line(&text, "solar_plan_expected_export_kwh_rest_of_day"),
            Some("solar_plan_expected_export_kwh_rest_of_day 6.3")
        );
        assert_eq!(
            line(&text, "solar_plan_expected_import_kwh_rest_of_day"),
            Some("solar_plan_expected_import_kwh_rest_of_day 0")
        );
        assert_eq!(
            line(&text, "solar_plan_expected_soc_end_of_day_pct"),
            Some("solar_plan_expected_soc_end_of_day_pct 100")
        );
        // Half an hour later the first MTU is behind us and no longer counted.
        let later = render(&state(), NOON + 1800);
        assert_eq!(
            line(&later, "solar_plan_expected_net_czk_rest_of_day"),
            Some("solar_plan_expected_net_czk_rest_of_day 10")
        );
    }

    #[test]
    fn a_plan_that_covers_no_rest_of_today_emits_no_aggregate() {
        // Prague midnight: every fixture step is now behind us, so no aggregate is emitted
        // rather than four zeros that would look like a plan expecting nothing.
        let text = render(&state(), NOON + 12 * 3600);
        assert!(line(&text, "solar_plan_expected_net_czk_rest_of_day").is_none());
    }

    #[test]
    fn a_withdrawn_plan_removes_the_plan_metrics_instead_of_zeroing_them() {
        let mut state = state();
        state.plan = None;
        let text = render(&state, NOON);
        assert!(line(&text, "solar_price_buy_czk_per_kwh").is_none());
        assert!(line(&text, "solar_plan_power_kw").is_none());
        // The plant's own numbers are unaffected: they come from the registers, not the plan.
        assert!(line(&text, "solar_pv_energy_kwh_total").is_some());
        assert_eq!(line(&text, "solar_bridge_up"), Some("solar_bridge_up 1"));
    }

    #[test]
    fn a_missing_register_disappears_rather_than_reading_zero() {
        let mut state = state();
        state.values = values(&[("battery_soc", 55.0)]);
        let text = render(&state, NOON);
        assert!(line(&text, "solar_grid_power_w").is_none());
        assert!(line(&text, "solar_pv_power_w").is_none());
        assert_eq!(
            line(&text, "solar_battery_soc_percent"),
            Some("solar_battery_soc_percent 55")
        );
    }

    #[test]
    fn a_dead_link_is_visible_as_a_gauge() {
        let mut state = state();
        state.link_ok = false;
        state.ts = (NOON - 90) as u64;
        let text = render(&state, NOON);
        assert_eq!(line(&text, "solar_link_ok"), Some("solar_link_ok 0"));
        assert_eq!(
            line(&text, "solar_state_age_seconds"),
            Some("solar_state_age_seconds 90")
        );
    }

    #[test]
    fn a_non_finite_reading_is_dropped_instead_of_poisoning_the_series() {
        let mut state = state();
        state
            .values
            .insert("grid_internal_power_total".into(), Value::Number(f64::NAN));
        state
            .values
            .insert("battery_soc".into(), Value::Number(f64::INFINITY));
        let text = render(&state, NOON);
        assert!(line(&text, "solar_grid_power_w").is_none());
        assert!(line(&text, "solar_battery_soc_percent").is_none());
    }

    #[test]
    fn every_metric_name_is_a_valid_prometheus_name() {
        let text = render(&state(), NOON);
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let name = line.split(' ').next().expect("a sample has a name");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                "invalid metric name {name:?}"
            );
            let value: f64 = line
                .split(' ')
                .nth(1)
                .expect("a sample has a value")
                .parse()
                .expect("a sample value parses as a float");
            assert!(value.is_finite());
        }
    }
}
