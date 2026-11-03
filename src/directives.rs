use std::path::{Path, PathBuf};

use kameo::{Actor, Reply, actor::ActorRef, message::Message};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::{StateActor, Update};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct Percent(f64);

impl TryFrom<f64> for Percent {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if (0.0..=100.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(format!("expected 0–100, got {v}"))
        }
    }
}

impl Percent {
    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct Price(f64);

impl TryFrom<f64> for Price {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if v.abs() <= 10_000.0 {
            Ok(Self(v))
        } else {
            Err(format!("expected at most ±10000 CZK/kWh, got {v}"))
        }
    }
}

impl Price {
    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Reply, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directives {
    pub version: u64,
    pub planner_enabled: bool,
    /// Lets the planner write to the inverter at all. Default **off**: a deploy that hands
    /// the planner a bus handle still only plans and reports until this is switched on.
    /// `planner_enabled` decides whether a plan exists; this decides whether the plan
    /// reaches the registers. Switching it **off** also disarms.
    ///
    /// `serde(default)` because a `directives.json` written before this field existed must
    /// still load — and must load **off**, not as an error.
    #[serde(default)]
    pub actuator_enabled: bool,
    /// Put the inverter in the safe state by hand: hold the pack at 100 %, sell nothing,
    /// charge nothing from the grid. Overrides `actuator_enabled` on purpose — it is the
    /// operator's way to stop the plant without stopping the process.
    #[serde(default)]
    pub disarm: bool,
    /// Hard lower SOC bound, percent. An **export** bound: the planner may serve the site
    /// load below it, but never sells under it.
    pub soc_floor: Percent,
    /// Hard upper SOC bound, percent.
    pub soc_ceiling: Percent,
    /// The inverter's single AC interface rating, kW.
    pub max_power_kw: f64,
    /// All-in household tariff paid per kWh imported, CZK/kWh.
    pub buy_price_czk: Price,
    /// Deducted from the spot price, CZK/kWh.
    pub sell_fee_czk: Price,
    /// Measured ÷ forecast, applied to every forecast kWh.
    pub forecast_calibration: f64,
    /// Energy-grid resolution the DP walks, kWh.
    pub energy_step_kwh: f64,
    /// Action-grid resolution the DP chooses from, kW.
    pub action_step_kw: f64,
}

impl Default for Directives {
    fn default() -> Self {
        let percent = |value: f64| Percent::try_from(value).expect("static bound");
        let price = |value: f64| Price::try_from(value).expect("static bound");
        Self {
            version: 0,
            planner_enabled: true,
            actuator_enabled: false,
            disarm: false,
            soc_floor: percent(20.0),
            soc_ceiling: percent(80.0),
            max_power_kw: 12.0,
            buy_price_czk: price(5.0),
            sell_fee_czk: price(0.5),
            forecast_calibration: 1.0,
            energy_step_kwh: 0.32,
            action_step_kw: 1.2,
        }
    }
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("unknown directive '{0}'")]
    Unknown(String),
    #[error("{name}: {reason}")]
    Invalid { name: String, reason: String },
    #[error("persisting directives failed: {0}")]
    Persist(String),
}

fn invalid(name: &str, reason: &str) -> ApplyError {
    ApplyError::Invalid {
        name: name.to_string(),
        reason: reason.to_string(),
    }
}

fn set<T: PartialEq>(slot: &mut T, new: T) -> bool {
    if *slot == new {
        false
    } else {
        *slot = new;
        true
    }
}

/// A plain number inside a closed range. Rejected, never clamped — the bridge is the
/// authority and HA's widget bounds are cosmetic.
fn number_in(
    value: &serde_json::Value,
    name: &str,
    range: std::ops::RangeInclusive<f64>,
) -> Result<f64, ApplyError> {
    let number = value
        .as_f64()
        .ok_or_else(|| invalid(name, "expected a number"))?;
    if range.contains(&number) {
        Ok(number)
    } else {
        Err(invalid(
            name,
            &format!("expected {}–{}, got {number}", range.start(), range.end()),
        ))
    }
}

fn percent(value: &serde_json::Value, name: &str) -> Result<Percent, ApplyError> {
    let number = value
        .as_f64()
        .ok_or_else(|| invalid(name, "expected a number"))?;
    Percent::try_from(number).map_err(|e| invalid(name, &e))
}

fn price(value: &serde_json::Value, name: &str) -> Result<Price, ApplyError> {
    let number = value
        .as_f64()
        .ok_or_else(|| invalid(name, "expected a number"))?;
    Price::try_from(number).map_err(|e| invalid(name, &e))
}

impl Directives {
    pub fn apply(&mut self, name: &str, value: serde_json::Value) -> Result<bool, ApplyError> {
        let changed = match name {
            "planner_enabled" => {
                let new = value
                    .as_bool()
                    .ok_or_else(|| invalid(name, "expected true/false"))?;
                set(&mut self.planner_enabled, new)
            }
            "actuator_enabled" => {
                let new = value
                    .as_bool()
                    .ok_or_else(|| invalid(name, "expected true/false"))?;
                set(&mut self.actuator_enabled, new)
            }
            "disarm" => {
                let new = value
                    .as_bool()
                    .ok_or_else(|| invalid(name, "expected true/false"))?;
                set(&mut self.disarm, new)
            }
            "soc_floor" => {
                let new = percent(&value, name)?;
                if new.get() >= self.soc_ceiling.get() {
                    return Err(invalid(name, "soc_floor must stay below soc_ceiling"));
                }
                set(&mut self.soc_floor, new)
            }
            "soc_ceiling" => {
                let new = percent(&value, name)?;
                if new.get() <= self.soc_floor.get() {
                    return Err(invalid(name, "soc_ceiling must stay above soc_floor"));
                }
                set(&mut self.soc_ceiling, new)
            }
            "max_power_kw" => {
                set(&mut self.max_power_kw, number_in(&value, name, 0.5..=30.0)?)
            }
            "buy_price_czk" => set(&mut self.buy_price_czk, price(&value, name)?),
            "sell_fee_czk" => {
                let new = price(&value, name)?;
                if new.get() < 0.0 {
                    return Err(invalid(name, "expected a non-negative fee"));
                }
                set(&mut self.sell_fee_czk, new)
            }
            "forecast_calibration" => set(
                &mut self.forecast_calibration,
                number_in(&value, name, 0.1..=3.0)?,
            ),
            "energy_step_kwh" => set(
                &mut self.energy_step_kwh,
                number_in(&value, name, 0.05..=5.0)?,
            ),
            "action_step_kw" => set(
                &mut self.action_step_kw,
                number_in(&value, name, 0.1..=5.0)?,
            ),
            _ => return Err(ApplyError::Unknown(name.to_string())),
        };
        if changed {
            self.version += 1;
        }
        Ok(changed)
    }
}

pub fn load(path: &Path) -> anyhow::Result<Option<Directives>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let mut value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("directives file {path:?}: {e}"))?;
            // `force_mode` was deleted 2026-11-06. A file written by an older binary still
            // carries it, and `deny_unknown_fields` would turn that into a startup failure;
            // drop the retired key instead. Every other unknown field is still an error.
            if let Some(object) = value.as_object_mut() {
                object.remove("force_mode");
            }
            Ok(Some(serde_json::from_value(value).map_err(|e| {
                anyhow::anyhow!("directives file {path:?}: {e}")
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {path:?}: {e}")),
    }
}

/// Atomic write: tmp file + rename within the same directory/filesystem.
pub fn save(path: &Path, d: &Directives) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Actor)]
pub struct DirectivesActor {
    path: PathBuf,
    current: Directives,
    state: ActorRef<StateActor>,
}

impl DirectivesActor {
    /// Loads persisted state; errors on a corrupt file (fail startup loudly).
    pub fn new(path: PathBuf, state: ActorRef<StateActor>) -> anyhow::Result<Self> {
        let current = load(&path)?.unwrap_or_default();
        Ok(Self {
            path,
            current,
            state,
        })
    }
}

pub struct ApplyDirective {
    pub name: String,
    pub value: serde_json::Value,
}

pub struct GetCurrent;

impl Message<ApplyDirective> for DirectivesActor {
    type Reply = Result<Directives, ApplyError>;

    async fn handle(
        &mut self,
        msg: ApplyDirective,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut next = self.current.clone();
        match next.apply(&msg.name, msg.value) {
            Ok(false) => Ok(self.current.clone()),
            Ok(true) => {
                save(&self.path, &next).map_err(|e| ApplyError::Persist(e.to_string()))?;
                self.current = next.clone();
                let _ = self.state.tell(Update::Directives(next)).send().await;
                Ok(self.current.clone())
            }
            Err(e) => Err(e),
        }
    }
}

impl Message<GetCurrent> for DirectivesActor {
    type Reply = Directives;

    async fn handle(
        &mut self,
        _msg: GetCurrent,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.current.clone()
    }
}

// ---------------------------------------------------------------------------
// Directive schema — meta wire contract (added 2026-10-11, decisions 1b/2b).
//
// meta gains a top-level "directives" array; the HA integration generates
// switch/select/number entities from it. Types describe DATA, not HA widgets
// (bool -> SwitchEntity, enum -> SelectEntity, number -> NumberEntity).
//
// Exact JSON the bridge must emit (frozen contract; HA side ships against it):
//   {"name":"planner_enabled","type":"bool"}
//   {"name":"actuator_enabled","type":"bool"}
//   {"name":"disarm","type":"bool"}
//   {"name":"soc_floor","type":"number","min":0.0,"max":100.0,"nullable":false,"unit":"%"}
//   {"name":"soc_ceiling","type":"number","min":0.0,"max":100.0,"nullable":false,"unit":"%"}
//   {"name":"max_power_kw","type":"number","min":0.5,"max":30.0,"nullable":false,"unit":"kW"}
//   {"name":"buy_price_czk","type":"number","min":0.0,"max":10000.0,"nullable":false,"unit":"CZK/kWh"}
//   {"name":"sell_fee_czk","type":"number","min":0.0,"max":10000.0,"nullable":false,"unit":"CZK/kWh"}
//   {"name":"forecast_calibration","type":"number","min":0.1,"max":3.0,"nullable":false}
//   {"name":"energy_step_kwh","type":"number","min":0.05,"max":5.0,"nullable":false,"unit":"kWh"}
//   {"name":"action_step_kw","type":"number","min":0.1,"max":5.0,"nullable":false,"unit":"kW"}
//
// min/max mirror the Percent/Price newtype bounds; nullable marks Option fields.
// Ranges here are UX clamps only — Directives::apply() stays the validation
// authority.

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum DirectiveSpec {
    Bool {
        name: &'static str,
    },
    Number {
        name: &'static str,
        min: f64,
        max: f64,
        nullable: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<&'static str>,
    },
}

/// The planner-directive surface as `meta.directives`, exactly per the frozen
/// wire contract in the comment block above. Static by design: it mirrors the
/// `Directives` struct + `apply()`, not the register map.
pub fn directive_schema() -> serde_json::Value {
    serde_json::to_value([
        DirectiveSpec::Bool {
            name: "planner_enabled",
        },
        DirectiveSpec::Bool {
            name: "actuator_enabled",
        },
        DirectiveSpec::Bool { name: "disarm" },
        DirectiveSpec::Number {
            name: "soc_floor",
            min: 0.0,
            max: 100.0,
            nullable: false,
            unit: Some("%"),
        },
        DirectiveSpec::Number {
            name: "soc_ceiling",
            min: 0.0,
            max: 100.0,
            nullable: false,
            unit: Some("%"),
        },
        DirectiveSpec::Number {
            name: "max_power_kw",
            min: 0.5,
            max: 30.0,
            nullable: false,
            unit: Some("kW"),
        },
        DirectiveSpec::Number {
            name: "buy_price_czk",
            min: 0.0,
            max: 10_000.0,
            nullable: false,
            unit: Some("CZK/kWh"),
        },
        DirectiveSpec::Number {
            name: "sell_fee_czk",
            min: 0.0,
            max: 10_000.0,
            nullable: false,
            unit: Some("CZK/kWh"),
        },
        DirectiveSpec::Number {
            name: "forecast_calibration",
            min: 0.1,
            max: 3.0,
            nullable: false,
            unit: None,
        },
        DirectiveSpec::Number {
            name: "energy_step_kwh",
            min: 0.05,
            max: 5.0,
            nullable: false,
            unit: Some("kWh"),
        },
        DirectiveSpec::Number {
            name: "action_step_kw",
            min: 0.1,
            max: 5.0,
            nullable: false,
            unit: Some("kW"),
        },
    ])
    .expect("static schema always serializes")
}
