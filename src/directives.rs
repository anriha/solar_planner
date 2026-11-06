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

/// Number of editable points per conversion curve. Five is the measured shape (the PV→
/// pack path needs five bands); a path that needs fewer leaves the tail disabled.
pub const CURVE_SLOTS: usize = 5;

/// Lowest efficiency a point may carry, percent. `0.0` disables the slot; anything
/// between 0 and 50 is a typo (a fraction typed into a percent field), not a measurement.
pub const MIN_EFFICIENCY_PCT: f64 = 50.0;

/// Highest input power a point may sit at, kW. The AC interface is 12 kW and the array is
/// 16.5 kWp, so 30 leaves room without admitting a unit mix-up.
pub const MAX_KNOT_KW: f64 = 30.0;

/// The four measured conversion paths (wiki `entities/deye-inverter-energy-model.md`), in
/// the order they appear on the efficiency device. This table drives the meta schema, the
/// wire keys and the name parser, so the three cannot drift apart.
pub const CURVE_NAMES: [&str; 4] = [
    "pv_to_battery",
    "pv_to_grid",
    "battery_to_grid",
    "grid_to_battery",
];

/// One editable point of a conversion curve: an input power and the one-way efficiency
/// measured there.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurvePoint {
    /// Input power the point was measured at, kW. **DC** for `pv_to_*` and
    /// `battery_to_grid` (the curve is keyed on the side energy flows into); **AC** for
    /// `grid_to_battery`, where the grid is the input.
    pub kw: f64,
    /// One-way efficiency, percent. `0.0` disables the slot.
    pub pct: f64,
}

/// A path's efficiency as up to [`CURVE_SLOTS`] `(power, efficiency)` points.
///
/// The operator owns **both** coordinates: a new measurement moves a knot's power as often
/// as its efficiency, so the curve is the whole list rather than a coefficient on fixed
/// bands. Slots are edited in place; active points must ascend by power, so a slot keeps
/// its identity across edits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathCurve {
    pub points: [CurvePoint; CURVE_SLOTS],
}

impl CurvePoint {
    const fn new(kw: f64, pct: f64) -> Self {
        Self { kw, pct }
    }
}

/// Builds a curve from literal `(kw, pct)` pairs; the unused tail is `pct: 0.0`.
fn curve(points: [(f64, f64); CURVE_SLOTS]) -> PathCurve {
    PathCurve {
        points: points.map(|(kw, pct)| CurvePoint::new(kw, pct)),
    }
}

impl PathCurve {
    /// Rejects an unusable curve: no enabled points, a non-positive or non-ascending
    /// power, or an efficiency outside 50–100 %. This is the authority; the HA widget
    /// bounds are cosmetic.
    fn validate(&self) -> Result<(), String> {
        let mut previous: Option<(usize, f64)> = None;
        let mut active = 0;
        for (index, point) in self.points.iter().enumerate() {
            if point.pct == 0.0 {
                continue;
            }
            if !(MIN_EFFICIENCY_PCT..=100.0).contains(&point.pct) {
                return Err(format!(
                    "slot {}: efficiency {} % is outside {}–100 (0 disables the slot)",
                    index + 1,
                    point.pct,
                    MIN_EFFICIENCY_PCT
                ));
            }
            if !(point.kw > 0.0 && point.kw <= MAX_KNOT_KW) {
                return Err(format!(
                    "slot {}: power {} kW is outside 0–{MAX_KNOT_KW}",
                    index + 1,
                    point.kw
                ));
            }
            if let Some((earlier, kw)) = previous
                && point.kw <= kw
            {
                return Err(format!(
                    "slot {}: power {} kW must be above slot {earlier} ({} kW)",
                    index + 1,
                    point.kw,
                    kw
                ));
            }
            previous = Some((index + 1, point.kw));
            active += 1;
        }
        if active == 0 {
            return Err("at least one point needs a non-zero efficiency".to_string());
        }
        Ok(())
    }

    /// The enabled points, in slot order.
    pub fn active(&self) -> impl Iterator<Item = &CurvePoint> {
        self.points.iter().filter(|point| point.pct > 0.0)
    }

    /// The curve as fractions, ready for [`EfficiencyCurve::new`](crate::planner::EfficiencyCurve).
    pub fn fractions(&self) -> Vec<(f64, f64)> {
        self.active()
            .map(|point| (point.kw, point.pct / 100.0))
            .collect()
    }
}

/// PV DC → pack terminals, measured 2026-11-03: 94.7 % at 5–9 kW, 94.3 % at 3–5, 92.8 %
/// at 1.5–3, **87.0 % below 1.5** — the low-power collapse is why the knots matter.
fn default_pv_to_battery() -> PathCurve {
    curve([
        (0.5, 87.0),
        (1.5, 87.0),
        (3.0, 92.8),
        (6.0, 94.3),
        (10.0, 94.7),
    ])
}

/// PV DC → AC bus: 97.0 % observed at ~2.1 kW, 97.6 % nameplate maximum. The rising tail
/// is the datasheet's, not a measurement; below 2 kW the curve clamps at 97.0 %.
fn default_pv_to_grid() -> PathCurve {
    curve([(2.0, 97.0), (10.0, 97.6), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)])
}

/// Pack terminals → AC bus, measured: 96.9 % at 6–11 kW, 96.0 % at 3–6, 93.9 % below 3.
fn default_battery_to_grid() -> PathCurve {
    curve([
        (0.5, 93.9),
        (3.0, 93.9),
        (6.0, 96.0),
        (10.24, 96.9),
        (0.0, 0.0),
    ])
}

/// Grid → pack terminals, the AC charger leg: **unmeasured**, 95 % is the working
/// assumption, held equal across the range until the leg is observed.
fn default_grid_to_battery() -> PathCurve {
    curve([
        (1.0, 95.0),
        (9.472, 95.0),
        (0.0, 0.0),
        (0.0, 0.0),
        (0.0, 0.0),
    ])
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
    /// Distribution and the other regulated per-kWh charges on imported energy, CZK/kWh,
    /// VAT included. The buy side is `spot + buy_distribution_czk + buy_seller_fee_czk`
    /// (2026-11-06): the plant is on spot pricing both ways, so no flat tariff exists.
    ///
    /// `serde(default)` because a `directives.json` written before that model lacks the
    /// field — its retired `buy_price_czk` is dropped on load — and must still boot.
    #[serde(default = "default_buy_distribution")]
    pub buy_distribution_czk: Price,
    /// The supplier's own fee on imported energy, CZK/kWh, VAT included. 0.3 is the
    /// settled 300 CZK/MWh from the ceník.
    #[serde(default = "default_buy_seller_fee")]
    pub buy_seller_fee_czk: Price,
    /// Deducted from the spot price, CZK/kWh. **Its own fee**, not the buy-side one: what
    /// the trader keeps on an export, separate from what it charges on an import.
    pub sell_fee_czk: Price,
    /// Measured ÷ forecast, applied to every forecast kWh.
    pub forecast_calibration: f64,
    /// Energy-grid resolution the DP walks, kWh.
    pub energy_step_kwh: f64,
    /// Action-grid resolution the DP chooses from, kW.
    pub action_step_kw: f64,
    /// The four conversion paths the DP prices. Each is a list of editable
    /// `(power, efficiency)` points; see [`PathCurve`]. `serde(default)` carries the
    /// measured curves into a `directives.json` written before they were configurable.
    #[serde(default = "default_pv_to_battery")]
    pub pv_to_battery: PathCurve,
    #[serde(default = "default_pv_to_grid")]
    pub pv_to_grid: PathCurve,
    #[serde(default = "default_battery_to_grid")]
    pub battery_to_grid: PathCurve,
    #[serde(default = "default_grid_to_battery")]
    pub grid_to_battery: PathCurve,
}

/// The settled per-kWh buy charges, as `serde` defaults for a legacy file.
fn default_buy_distribution() -> Price {
    Price::try_from(2.515).expect("static bound")
}

fn default_buy_seller_fee() -> Price {
    Price::try_from(0.3).expect("static bound")
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
            buy_distribution_czk: default_buy_distribution(),
            buy_seller_fee_czk: default_buy_seller_fee(),
            sell_fee_czk: price(0.5),
            forecast_calibration: 1.0,
            energy_step_kwh: 0.32,
            action_step_kw: 1.2,
            pv_to_battery: default_pv_to_battery(),
            pv_to_grid: default_pv_to_grid(),
            battery_to_grid: default_battery_to_grid(),
            grid_to_battery: default_grid_to_battery(),
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

/// Which editable point one curve directive name addresses.
struct CurveField {
    curve: usize,
    slot: usize,
    efficiency: bool,
}

/// Parses `pv_to_battery_p2_pct` → curve 0, slot 1, efficiency. The names are generated by
/// [`directive_schema`]; this parser is the only other place that knows their shape.
fn parse_curve_field(name: &str) -> Option<CurveField> {
    let (rest, field) = name.rsplit_once('_')?;
    let efficiency = match field {
        "kw" => false,
        "pct" => true,
        _ => return None,
    };
    let (curve_name, slot) = rest.rsplit_once('_')?;
    let slot: usize = slot.strip_prefix('p')?.parse().ok()?;
    if !(1..=CURVE_SLOTS).contains(&slot) {
        return None;
    }
    let curve = CURVE_NAMES.iter().position(|name| *name == curve_name)?;
    Some(CurveField {
        curve,
        slot: slot - 1,
        efficiency,
    })
}

impl Directives {
    pub fn apply(&mut self, name: &str, value: serde_json::Value) -> Result<bool, ApplyError> {
        if let Some(field) = parse_curve_field(name) {
            return self.apply_curve(field, name, value);
        }
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
            "buy_distribution_czk" => {
                let new = price(&value, name)?;
                if new.get() < 0.0 {
                    return Err(invalid(name, "expected a non-negative charge"));
                }
                set(&mut self.buy_distribution_czk, new)
            }
            "buy_seller_fee_czk" => {
                let new = price(&value, name)?;
                if new.get() < 0.0 {
                    return Err(invalid(name, "expected a non-negative charge"));
                }
                set(&mut self.buy_seller_fee_czk, new)
            }
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

    /// One point edit, validated as a whole curve. Moving a power past its neighbour is
    /// rejected rather than silently reordered, so a slot keeps its identity.
    fn apply_curve(
        &mut self,
        field: CurveField,
        name: &str,
        value: serde_json::Value,
    ) -> Result<bool, ApplyError> {
        let new = number_in(
            &value,
            name,
            if field.efficiency {
                0.0..=100.0
            } else {
                0.0..=MAX_KNOT_KW
            },
        )?;
        let mut candidate = self.clone();
        let point = &mut candidate.curve_mut(field.curve).points[field.slot];
        if field.efficiency {
            point.pct = new;
        } else {
            point.kw = new;
        }
        candidate
            .curve_mut(field.curve)
            .validate()
            .map_err(|reason| invalid(name, &reason))?;
        if candidate == *self {
            return Ok(false);
        }
        *self = candidate;
        self.version += 1;
        Ok(true)
    }

    /// One curve by index, in [`CURVE_NAMES`] order.
    pub fn curve(&self, index: usize) -> &PathCurve {
        assert!(index < CURVE_NAMES.len(), "curve index out of range");
        match index {
            0 => &self.pv_to_battery,
            1 => &self.pv_to_grid,
            2 => &self.battery_to_grid,
            _ => &self.grid_to_battery,
        }
    }

    fn curve_mut(&mut self, index: usize) -> &mut PathCurve {
        assert!(index < CURVE_NAMES.len(), "curve index out of range");
        match index {
            0 => &mut self.pv_to_battery,
            1 => &mut self.pv_to_grid,
            2 => &mut self.battery_to_grid,
            _ => &mut self.grid_to_battery,
        }
    }

    /// The flat directive map the HA integration reads: one key per entity, exactly the
    /// names [`directive_schema`] advertises. `Directives` keeps the curves nested (a file
    /// with five points per path is readable); the wire flattens them because every HA
    /// entity looks up `directives[name]`.
    pub fn wire_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("directives always serialize");
        let map = value
            .as_object_mut()
            .expect("directives serialize as an object");
        for (index, name) in CURVE_NAMES.iter().enumerate() {
            map.remove(*name);
            for (slot, point) in self.curve(index).points.iter().enumerate() {
                map.insert(
                    format!("{name}_p{}_kw", slot + 1),
                    serde_json::json!(point.kw),
                );
                map.insert(
                    format!("{name}_p{}_pct", slot + 1),
                    serde_json::json!(point.pct),
                );
            }
        }
        value
    }
}

pub fn load(path: &Path) -> anyhow::Result<Option<Directives>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let mut value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("directives file {path:?}: {e}"))?;
            // `force_mode` was deleted 2026-11-06, and `buy_price_czk` retired the same day
            // with the spot-buy model (buy is spot + the two charges now). A file written by
            // an older binary still carries them, and `deny_unknown_fields` would turn that
            // into a startup failure; drop the retired keys instead. Every other unknown
            // field is still an error.
            if let Some(object) = value.as_object_mut() {
                for retired in ["force_mode", "buy_price_czk"] {
                    object.remove(retired);
                }
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
    ///
    /// The loaded value is pushed into the state actor here rather than by the caller,
    /// because every telemetry broadcast carries `StateActor`'s copy and a copy left at
    /// `Directives::default()` silently rewrites the persisted policy on screen. Found
    /// live 2026-11-06: `hello` carried the file's `version 4` (with a hand-set buy price
    /// that was retired the same day, when buying became spot + charges), the first `state`
    /// frame 5 s later carried `version 0` — `Directives::default()` — and HA showed the
    /// defaults until the next command. Seeding at construction is the one place that cannot
    /// be forgotten by a new entry point.
    pub fn new(path: PathBuf, state: ActorRef<StateActor>) -> anyhow::Result<Self> {
        let current = load(&path)?.unwrap_or_default();
        state
            .tell(Update::Directives(Box::new(current.clone())))
            .try_send()
            .map_err(|e| anyhow::anyhow!("seeding state with directives: {e}"))?;
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
                let _ = self.state.tell(Update::Directives(Box::new(next))).send().await;
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
//   {"name":"buy_distribution_czk","type":"number","min":0.0,"max":10000.0,"nullable":false,"unit":"CZK/kWh","step":0.001}
//   {"name":"buy_seller_fee_czk","type":"number","min":0.0,"max":10000.0,"nullable":false,"unit":"CZK/kWh","step":0.001}
//   {"name":"sell_fee_czk","type":"number","min":0.0,"max":10000.0,"nullable":false,"unit":"CZK/kWh","step":0.001}
//   {"name":"forecast_calibration","type":"number","min":0.1,"max":3.0,"nullable":false}
//   {"name":"energy_step_kwh","type":"number","min":0.05,"max":5.0,"nullable":false,"unit":"kWh"}
//   {"name":"action_step_kw","type":"number","min":0.1,"max":5.0,"nullable":false,"unit":"kW"}
//
// min/max mirror the Percent/Price newtype bounds; nullable marks Option fields. `step`
// was added 2026-11-06 (additive, HA reads it with `.get("step") or 1.0`): a slider whose
// step is 1.0 cannot reach 2.515 or 0.3 at all, so every sub-unit price needs its own.
// Ranges here are UX clamps only — Directives::apply() stays the validation
// authority (it is also where the two buy charges are held to non-negative, which the
// Price newtype alone would allow negative).
//
// Efficiency curves (2026-11-06): each of the four paths in CURVE_NAMES contributes
// CURVE_SLOTS × 2 numbers, so the operator owns both coordinates of every point:
//   {"type":"number","name":"pv_to_battery_p1_kw","min":0.0,"max":30.0,"nullable":false,
//    "unit":"kW","step":0.01,"label":"PV→battery 1 power",
//    "group":"efficiency","category":"config","mode":"box"}
//   {"type":"number","name":"pv_to_battery_p1_pct","min":0.0,"max":100.0,"nullable":false,
//    "unit":"%","step":0.1,"label":"PV→battery 1 efficiency",
//    "group":"efficiency","category":"config","mode":"box"}
// `label` is the HA friendly name, `group` picks the device ("efficiency" = the
// calibration device), `category: config` hides the entity under the device's
// Configuration section, `mode` is the NumberMode. `pct 0` disables a slot; for these
// the widget bounds and apply()'s bounds coincide on purpose (a 0.943 typed into a
// percent field is a typo, not a curve).

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
        #[serde(skip_serializing_if = "Option::is_none")]
        step: Option<f64>,
    },
    /// One coordinate of one curve point: the same wire shape as [`Self::Number`], plus
    /// the display and placement hints the efficiency entities need. Serialized as
    /// `"type":"number"` so the HA side keeps a single number constructor.
    #[serde(rename = "number")]
    CurvePoint {
        name: String,
        min: f64,
        max: f64,
        nullable: bool,
        unit: &'static str,
        step: f64,
        label: String,
        group: &'static str,
        category: &'static str,
        mode: &'static str,
    },
}

/// The human label for a curve, used in the entity names on the efficiency device.
fn curve_label(name: &str) -> &'static str {
    match name {
        "pv_to_battery" => "PV→battery",
        "pv_to_grid" => "PV→grid",
        "battery_to_grid" => "battery→grid",
        "grid_to_battery" => "grid→battery",
        _ => unreachable!("CURVE_NAMES is the closed set"),
    }
}

/// Every editable coordinate: `CURVE_NAMES.len() × CURVE_SLOTS` points × `(power, η)`.
/// Power and efficiency are both owned here because a knot's position moves with every
/// new measurement — a fixed-band grid could not carry that.
fn curve_specs() -> Vec<DirectiveSpec> {
    let mut specs = Vec::with_capacity(CURVE_NAMES.len() * CURVE_SLOTS * 2);
    for name in CURVE_NAMES {
        let label = curve_label(name);
        for slot in 1..=CURVE_SLOTS {
            specs.push(DirectiveSpec::CurvePoint {
                name: format!("{name}_p{slot}_kw"),
                min: 0.0,
                max: MAX_KNOT_KW,
                nullable: false,
                unit: "kW",
                step: 0.01,
                label: format!("{label} {slot} power"),
                group: "efficiency",
                category: "config",
                mode: "box",
            });
            specs.push(DirectiveSpec::CurvePoint {
                name: format!("{name}_p{slot}_pct"),
                min: 0.0,
                max: 100.0,
                nullable: false,
                unit: "%",
                step: 0.1,
                label: format!("{label} {slot} efficiency"),
                group: "efficiency",
                category: "config",
                mode: "box",
            });
        }
    }
    specs
}

/// The planner-directive surface as `meta.directives`, exactly per the frozen
/// wire contract in the comment block above. Static by design: it mirrors the
/// `Directives` struct + `apply()`, not the register map.
pub fn directive_schema() -> serde_json::Value {
    let mut specs = vec![
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
            step: None,
        },
        DirectiveSpec::Number {
            name: "soc_ceiling",
            min: 0.0,
            max: 100.0,
            nullable: false,
            unit: Some("%"),
            step: None,
        },
        DirectiveSpec::Number {
            name: "max_power_kw",
            min: 0.5,
            max: 30.0,
            nullable: false,
            unit: Some("kW"),
            step: None,
        },
        DirectiveSpec::Number {
            name: "buy_distribution_czk",
            min: 0.0,
            max: 10_000.0,
            nullable: false,
            unit: Some("CZK/kWh"),
            step: Some(0.001),
        },
        DirectiveSpec::Number {
            name: "buy_seller_fee_czk",
            min: 0.0,
            max: 10_000.0,
            nullable: false,
            unit: Some("CZK/kWh"),
            step: Some(0.001),
        },
        DirectiveSpec::Number {
            name: "sell_fee_czk",
            min: 0.0,
            max: 10_000.0,
            nullable: false,
            unit: Some("CZK/kWh"),
            step: Some(0.001),
        },
        DirectiveSpec::Number {
            name: "forecast_calibration",
            min: 0.1,
            max: 3.0,
            nullable: false,
            unit: None,
            step: None,
        },
        DirectiveSpec::Number {
            name: "energy_step_kwh",
            min: 0.05,
            max: 5.0,
            nullable: false,
            unit: Some("kWh"),
            step: None,
        },
        DirectiveSpec::Number {
            name: "action_step_kw",
            min: 0.1,
            max: 5.0,
            nullable: false,
            unit: Some("kW"),
            step: None,
        },
    ];
    specs.extend(curve_specs());
    serde_json::to_value(specs).expect("static schema always serializes")
}
