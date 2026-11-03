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
