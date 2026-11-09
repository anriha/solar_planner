use std::collections::BTreeMap;

use serde::Serialize;

use crate::decode::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub link_ok: bool,
    pub ts: u64,
    pub values: BTreeMap<String, Value>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs()
}

/// The only telemetry the planner reads: how much energy the pack holds right now.
///
/// Deliberately tiny. Prices, PV and the plant's own load arrive from the horizon, not
/// from the inverter, so the DP needs one measured number to start its walk — and it must
/// fail loudly when that number is missing, because a defaulted state of charge would be
/// baked into every step of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlanningSignals {
    /// State of charge, percent (register 588).
    pub battery_soc: f64,
    /// Pack power, watts, positive = charging (register 590 — sign still unverified).
    pub battery_power_w: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    #[error("planning signal '{0}' is missing — renamed, disabled, or not in the map")]
    Missing(&'static str),
    #[error("planning signal '{0}' decoded as text, not a number")]
    NotNumeric(&'static str),
}

impl PlanningSignals {
    pub fn extract(snapshot: &Snapshot) -> Result<Self, ExtractError> {
        Self::extract_values(&snapshot.values)
    }

    /// The same extraction from a bare value map — what `StateActor` hands the planner, and
    /// what the coupling test feeds through a decoded snapshot.
    pub fn extract_values(values: &BTreeMap<String, Value>) -> Result<Self, ExtractError> {
        Ok(Self {
            battery_soc: number(values, "battery_soc")?,
            battery_power_w: number(values, "battery_power")?,
        })
    }

    /// State of charge as a fraction in `0..=1`.
    pub fn soc_fraction(&self) -> f64 {
        self.battery_soc / 100.0
    }
}

/// What the ledger integrates: the AC meter and the pack.
///
/// **The meter is register 607 (`grid_internal_power_total`), not 619 (`grid_power_total`).**
/// Measured 2026-11-09 against 2026-11-07, the clearest export day on record: while the plant
/// exported 9.4–12.1 kW, 607 read −9 379 … −12 109 W and 619 read 0 … 12 W for the whole of
/// 2026-11-06 … 09. 619 is not wired to the export on this install, and a ledger that trusted
/// the name would write a plausible-looking zero for every MTU — the worst kind of wrong
/// number, because nothing about it looks broken.
///
/// 607 is import-positive, i.e. **export is negative** — the opposite of the plan's
/// [`PlanStep::ac_kwh`](crate::planner::PlanStep). [`Self::ac_kw`] is the single place that
/// flip happens, so no consumer has to remember it (board-3dx).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeteredSignals {
    /// Grid port power in watts, as the meter reports it: import positive, export negative.
    pub grid_internal_w: f64,
    /// State of charge, percent (register 588) — the pack at the MTU boundary.
    pub battery_soc: f64,
}

impl MeteredSignals {
    pub fn extract(snapshot: &Snapshot) -> Result<Self, ExtractError> {
        Self::extract_values(&snapshot.values)
    }

    /// Extract from a value map; a missing meter is an error, never a zero.
    pub fn extract_values(values: &BTreeMap<String, Value>) -> Result<Self, ExtractError> {
        Ok(Self {
            grid_internal_w: number(values, "grid_internal_power_total")?,
            battery_soc: number(values, "battery_soc")?,
        })
    }

    /// AC power in the plan's convention — **positive exported**, kW.
    pub fn ac_kw(&self) -> f64 {
        -self.grid_internal_w / 1000.0
    }
}

fn number(values: &BTreeMap<String, Value>, name: &'static str) -> Result<f64, ExtractError> {
    match values.get(name) {
        Some(Value::Number(value)) => Ok(*value),
        Some(Value::Text(_)) => Err(ExtractError::NotNumeric(name)),
        None => Err(ExtractError::Missing(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(values: &[(&str, Value)]) -> Snapshot {
        Snapshot {
            link_ok: true,
            ts: 1,
            values: values
                .iter()
                .map(|(name, value)| ((*name).to_string(), value.clone()))
                .collect(),
        }
    }

    #[test]
    fn extracts_the_state_of_charge_the_planner_starts_from() {
        let signals = PlanningSignals::extract(&snapshot(&[
            ("battery_soc", Value::Number(64.0)),
            ("battery_power", Value::Number(-1200.0)),
        ]))
        .expect("both signals present");

        assert_eq!(signals.battery_soc, 64.0);
        assert_eq!(signals.soc_fraction(), 0.64);
        assert_eq!(signals.battery_power_w, -1200.0);
    }

    /// The ledger's one sign rule, pinned to a recorded export: 2026-11-07 11:30 measured
    /// 11 597 W out of the inverter through a grid port reading −11 535 W, with `grid_power_total`
    /// sitting at 1 W beside it.
    #[test]
    fn the_meter_is_register_607_flipped_to_export_positive() {
        let signals = MeteredSignals::extract(&snapshot(&[
            ("grid_internal_power_total", Value::Number(-11_535.0)),
            ("grid_power_total", Value::Number(1.0)),
            ("battery_soc", Value::Number(21.0)),
        ]))
        .expect("both meter signals present");

        assert_eq!(signals.battery_soc, 21.0);
        assert!(
            (signals.ac_kw() - 11.535).abs() < 1e-9,
            "an export must read positive, got {}",
            signals.ac_kw()
        );
        // The import side flips the other way: a grid port drawing 3 kW is −3 kW of export.
        let importing = MeteredSignals {
            grid_internal_w: 3_000.0,
            battery_soc: 50.0,
        };
        assert!((importing.ac_kw() + 3.0).abs() < 1e-9);
    }

    /// 619 cannot stand in for the meter: it is present in every frame and reads ~zero, so a
    /// fallback to it would silently produce perfect-looking rows instead of an error.
    #[test]
    fn grid_power_total_cannot_stand_in_for_the_meter() {
        let err = MeteredSignals::extract(&snapshot(&[
            ("grid_power_total", Value::Number(0.0)),
            ("battery_soc", Value::Number(50.0)),
        ]))
        .expect_err("the meter signal is absent");
        assert_eq!(err, ExtractError::Missing("grid_internal_power_total"));
    }

    #[test]
    fn a_missing_signal_is_an_error_not_a_default() {
        let err = PlanningSignals::extract(&snapshot(&[("battery_power", Value::Number(0.0))]))
            .expect_err("battery_soc is absent");
        assert!(matches!(err, ExtractError::Missing("battery_soc")), "got {err:?}");

        let err = PlanningSignals::extract(&snapshot(&[
            ("battery_soc", Value::Text("normal".into())),
            ("battery_power", Value::Number(0.0)),
        ]))
        .expect_err("a text value is not a state of charge");
        assert!(matches!(err, ExtractError::NotNumeric("battery_soc")), "got {err:?}");
    }
}
