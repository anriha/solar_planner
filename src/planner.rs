//! Price-driven dispatch planner for the offsite PV + battery plant.
//!
//! # Contract
//!
//! [`solve`] is a pure function: horizon, prices, forecasts, params and the measured
//! battery energy in, a [`Plan`] out. It performs no I/O, keeps no state between
//! calls, and never touches the Modbus bus — `BusActor` stays the only writer.
//!
//! # Horizon
//!
//! The horizon is the list of *published* market time units (MTUs), from now to the
//! end of the available spot prices: today's remaining MTUs, plus tomorrow's once OTE
//! publishes them (~13:10 Prague). Never assume 24 h per day or 96 MTUs per day —
//! both the publication edge and DST break that assumption.
//!
//! # Method
//!
//! Dynamic programming over a discretized battery-energy grid. `V_t(E)` is the best
//! revenue obtainable over MTUs `t..T` given energy `E` at the start of `t`:
//!
//! ```text
//! V_T(E) = terminal value
//! V_t(E) = max over actions a: revenue_t(a) + V_{t+1}(E'(E, a))
//! ```
//!
//! The plan is the forward argmax walk from the measured energy. The marginal value of
//! a stored kWh at MTU `t` is `V_{t+1}(E + dE) - V_{t+1}(E)`, which is what turns a
//! plan step into an explanation: "selling at 7.9 beats 6.4 of stored value".
//!
//! # Units
//!
//! Power in kW, energy in kWh, price in CZK/kWh, MTU length in hours. SOC is carried
//! as energy in kWh (`soc = energy / params.capacity_kwh`); efficiencies are one-way
//! factors in `(0, 1]` that depend on the power at which the conversion happens.

use std::marker::PhantomData;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;

use crate::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours, Quantity};

pub mod actor;
pub mod actuator;
pub mod forecast;
pub mod horizon;
pub mod price;

/// One market time unit. `start` is unix seconds, `hours` is its length (0.25 for the
/// 15-minute day-ahead MTU, 1.0 if an hourly feed is ever used).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Mtu {
    pub start: i64,
    pub hours: Hours,
}

/// One MTU's exogenous inputs: what exporting it pays, what importing it costs, how
/// much PV is expected, and how much the site is expected to consume.
#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    pub mtu: Mtu,
    /// Net price received per kWh exported (spot − sell fee).
    pub price_sell: CzkPerKwh,
    /// All-in price paid per kWh imported (single-rate household tariff, tunable).
    pub price_buy: CzkPerKwh,
    /// Forecast PV production on the DC side during this MTU — array output, before any
    /// conversion. Every AC-side quantity is produced through a [`Params`] path curve.
    pub pv_kw: DcKw,
    /// The plant's own AC consumption during this MTU — the inverter plus the Pi, ~0.09 kW
    /// and effectively constant. **Not the house**: it sits behind its own meter, so its
    /// consumption never enters the transition; it enters only through the sharing revenue.
    pub load_kw: AcKw,
}

// Settled 2026-11-04 (plan Phase 3a, scaffold notes a): the sum type. The inverter has
// exactly one work mode per MTU, so "export while charging from grid" is unrepresentable
// rather than rejected at runtime.
/// What the planner chooses for one MTU.
///
/// **PV charges the pack first (corrected 2026-11-06).** The unit's own routing is load →
/// battery → grid (manual §5.7), so PV arrives as DC, serves the site load, and then charges
/// the pack; only the DC the pack refuses reaches the meter directly. An export target is
/// therefore met by the pack (and the PV spill), not by PV straight to the grid. An action
/// names the *grid-facing* power, and the pack flow is derived from it, [`Slot::pv_kw`] and
/// [`Slot::load_kw`] inside [`next_energy`]. There is deliberately no action that could
/// send PV into the pack while the grid supplies the meter — that is a strictly worse
/// route to the same kWh.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// No grid-facing target: PV covers the site load, the surplus charges the pack,
    /// a shortfall discharges it.
    Idle,
    /// Export `kw` of AC from the site (cap written to `max_sell_power`). PV serves the
    /// site load and this target first; the remainder comes out of the pack.
    Export { kw: AcKw },
    /// Import `kw` of AC into the plant on top of whatever PV produces; it charges the
    /// pack once PV has covered the site load.
    Import { kw: AcKw },
}

// Settled 2026-11-05, superseding the knot-union rule of 2026-11-04: the ladder is the
// plain uniform `action_step_kw` grid. Efficiency knots do not belong here — they are a
// property of the conversion curves and of the routing behind a target, not of the set of
// powers the planner may command, and at 1.2 kW steps against a ~7 % spread over the whole
// power range they change no decision. This type enumerates levels and nothing else.
/// The set of powers the planner may pick from in any MTU.
#[derive(Debug, Clone)]
pub struct ActionLadder {
    export: Vec<AcKw>,
    import: Vec<AcKw>,
}

impl ActionLadder {
    /// Build both ladders from `params`: `k · action_step_kw` for `k = 0 ..= floor(
    /// max_power_kw / action_step_kw)`, ascending, so `0.0` is always present and the
    /// default 1.2 kW step reaches exactly 12.0. Export and import are equal today — one
    /// `max_power_kw` bounds all four directions — carried separately only so that a
    /// directional rating would need no new type.
    pub fn new(params: &Params) -> Self {
        let steps = (params.max_power_kw.get() / params.action_step_kw.get()).floor() as usize;
        let levels: Vec<AcKw> = (0..=steps)
            .map(|k| AcKw::new(k as f64 * params.action_step_kw.get()))
            .collect();

        Self {
            export: levels.clone(),
            import: levels,
        }
    }

    /// Ascending export powers in kW, including 0.0, all ≤ `Params::max_power_kw`.
    pub fn export_levels(&self) -> Vec<AcKw> {
        self.export.clone()
    }

    /// Ascending import powers in kW, including 0.0, all ≤ `Params::max_power_kw`.
    pub fn import_levels(&self) -> Vec<AcKw> {
        self.import.clone()
    }
}

// Settled 2026-11-04 (plan Phase 3a, scaffold notes c): one number, "salable at" — the
// median, over the 7 days ending *today*, of each day's maximum spot price minus the sell
// fee. Refreshed on the daily price fetch, not per solve: T is only a level and never
// enters a transition, so a value a few hours stale is harmless. Measured 2026-11-03:
// 8.474 CZK/kWh spot → 7.974 net, against 8.342 (mean of daily maxima) and 5.583 (median
// of all MTUs); the 13.43 CZK/kWh spike of 29 Oct is correctly ignored. Buying against T
// is allowed. Zero and floor variants were rejected: a reserve belongs to `soc_floor`
// policy, not to the objective, and a zero terminal dumps the pack on the last MTU.
/// Value of the pack's energy at the end of the horizon, CZK/kWh.
#[derive(Debug, Clone, Copy)]
pub struct TerminalValue {
    pub czk_per_kwh: CzkPerKwh,
}

/// The window [`TerminalValue::measured`] averages over: the 7 Prague days ending *today*.
pub const TERMINAL_DAYS: i64 = 7;

/// What the solver assumes before it has seen a single day of prices — the median of the 7
/// daily maxima measured on 2026-11-03, net of the 0.5 CZK/kWh sell fee.
pub const BOOTSTRAP_TERMINAL_CZK_PER_KWH: f64 = 7.974;

impl TerminalValue {
    /// The settled rule (2026-11-04): the **median** of each day's maximum spot price over
    /// the [`TERMINAL_DAYS`] Prague days ending today, minus the sell fee.
    ///
    /// `points` may hold more than the window — the price cache also carries the horizon —
    /// so anything outside it is skipped. `None` when the window holds no price at all: the
    /// caller keeps [`BOOTSTRAP_TERMINAL_CZK_PER_KWH`] rather than invent a level, because
    /// the terminal value is a *level*, never a transition, and a wrong one only tilts the
    /// last MTU of the plan.
    ///
    /// Today's maximum is taken from the MTUs OTE has already published, so it is partial
    /// until the day ends. That is deliberate: the value is refreshed on the daily price
    /// fetch, not per solve.
    pub fn measured(points: &[price::PricePoint], now: i64, sell_fee: CzkPerKwh) -> Option<Self> {
        use std::collections::BTreeMap;

        let zone = price::prague().ok()?;
        let today = jiff::Timestamp::from_second(now)
            .ok()?
            .to_zoned(zone.clone())
            .date();
        let first = today
            .checked_sub(jiff::Span::new().days(TERMINAL_DAYS - 1))
            .ok()?;

        // Keyed by civil date, so the map comes out oldest-first and the median is stable
        // whatever order the points arrived in.
        let mut maxima: BTreeMap<jiff::civil::Date, f64> = BTreeMap::new();
        for point in points {
            let date = jiff::Timestamp::from_second(point.mtu.start)
                .ok()?
                .to_zoned(zone.clone())
                .date();
            if date < first || date > today {
                continue;
            }
            let czk = point.czk_per_kwh().get();
            let slot = maxima.entry(date).or_insert(f64::MIN);
            if czk > *slot {
                *slot = czk;
            }
        }

        let mut maxima: Vec<f64> = maxima.into_values().collect();
        if maxima.is_empty() {
            return None;
        }
        maxima.sort_by(f64::total_cmp);
        let middle = maxima.len() / 2;
        let median = if maxima.len() % 2 == 1 {
            maxima[middle]
        } else {
            (maxima[middle - 1] + maxima[middle]) / 2.0
        };
        Some(Self {
            czk_per_kwh: CzkPerKwh::new(median - sell_fee.get()),
        })
    }
}

/// One-way efficiency as a function of the power at which the conversion happens.
///
/// Configured as **anchor points**: the power knots are device facts and live in boot
/// config; the efficiencies are measurements and are tunable from HA, one per knot.
/// The actor pairs them into this curve. Piecewise-linear between knots, clamped to the
/// end segments.
#[derive(Debug, Clone)]
pub struct EfficiencyCurve<I: Quantity, O: Quantity> {
    /// `(input power, efficiency)` pairs, ascending by input power.
    points: Vec<(I, f64)>,
    output: PhantomData<O>,
}

impl<I: Quantity, O: Quantity> EfficiencyCurve<I, O> {
    pub fn new(mut points: Vec<(I, f64)>) -> Result<Self, PlanError> {
        if points.is_empty() {
            return Err(PlanError::InvalidParams(
                "No points in efficiency curve".to_string(),
            ));
        }
        if points
            .iter()
            .any(|(_, efficiency)| !(efficiency > &0.0 && *efficiency <= 1.0))
        {
            return Err(PlanError::InvalidParams(
                "Invalid efficiency values not in range (0, 1]".to_string(),
            ));
        }

        points.sort_by(|(a, _), (b, _)| a.get().total_cmp(&b.get()));
        Ok(Self {
            points,
            output: PhantomData,
        })
    }

    /// A constant curve — the single-number case, used by tests and as a fallback.
    pub fn flat(eta: f64) -> Self {
        debug_assert!(eta > 0.0 && eta <= 1.0);

        Self {
            points: vec![(I::from_f64(0.0), eta)],
            output: PhantomData,
        }
    }

    /// Efficiency at `kw`, piecewise-linear between knots and clamped outside them.
    /// Panics on an empty curve.
    pub fn at(&self, input: I) -> f64 {
        let i = self
            .points
            .partition_point(|(power, _)| power.get() < input.get());

        match i {
            0 => {
                // Smaller than any point take first available
                self.points[i].1
            }
            i if i == self.points.len() => self.points[self.points.len() - 1].1,
            i => {
                let (l_p, l_e) = self.points[i - 1];
                let (h_p, h_e) = self.points[i];

                l_e + (h_e - l_e) * (input.get() - l_p.get()) / (h_p.get() - l_p.get())
            }
        }
    }

    /// Forward direction: the output produced by `input` on the input side,
    /// `input_kw · η(input_kw)`. The knots are keyed on the input power, which is the
    /// side each path was measured into.
    pub fn through(&self, input: I) -> O {
        O::from_f64(input.get() * self.at(input))
    }

    /// Inverse direction: the input power needed to deliver `output` on the output
    /// side — solves `x · η(x) = output_kw` for `x`.
    ///
    /// The product `x · η(x)` is strictly increasing (η is validated into `(0, 1]`), so
    /// the root is unique and the input always exceeds the output. Fixed-point iteration
    /// from above (`x ← output / η(x)`) converges in a handful of steps because η varies
    /// slowly; a curve steep enough to make that stall falls back to bisection on a
    /// bracket that is guaranteed to contain the root.
    pub fn input_for(&self, output: O) -> I {
        let output_kw = output.get();

        if output_kw <= 0.0 {
            return I::from_f64(0.0);
        }

        let mut x = output_kw;
        for _ in 0..32 {
            let next = output_kw / self.at(I::from_f64(x));
            if (next - x).abs() <= 1e-9 {
                return I::from_f64(next);
            }
            x = next;
        }

        // `x >= output_kw` because η <= 1, so this bracket always contains the root.
        let mut lo = output_kw;
        let mut hi = output_kw + 1.0;
        while hi * self.at(I::from_f64(hi)) < output_kw {
            lo = hi;
            hi *= 2.0;
        }
        for _ in 0..64 {
            let mid = 0.5 * (lo + hi);
            if mid * self.at(I::from_f64(mid)) < output_kw {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        I::from_f64(0.5 * (lo + hi))
    }
}

/// What one solve needs to know about the plant: physics, bounds, grid resolution.
///
/// Assembled by the actor from boot config (device facts: pack energy, interface
/// rating, efficiency knots) and `Directives` (policy: SOC bounds, efficiency
/// measurements). Prices are **not** here — they arrive per MTU in [`Slot`].
///
/// The efficiency model is the **four measured paths and nothing else** (wiki
/// `entities/deye-inverter-energy-model.md`): PV→pack, PV→grid, pack→grid, grid→pack.
/// Each is a power-dependent curve with its own measured values. There is no global
/// round-trip factor and no fifth stage — the daily 0.918–0.932 terminal ratio is what
/// these paths multiply out to over a cycle, used as a consistency check on the curve
/// values rather than as a number in the model.
#[derive(Debug, Clone)]
pub struct Params {
    /// Nominal pack energy (boot config; reg 102 = 628 Ah at ~51.2 V ≈ 32 kWh).
    pub capacity_kwh: DcKwh,
    /// Single AC interface rating (boot config). Bounds export, import, charge and
    /// discharge alike — the inverter has one rating, not four. It bounds the plant's
    /// **total** AC, PV included, which is why the ladder tops out here.
    pub max_power_kw: AcKw,
    /// The pack's own DC power limit, both directions (boot config): 200 A at ~51.2 V
    /// ≈ 10.24 kW from the BMS, **not** regs 108/109. `max_power_kw` can be reached with
    /// PV contributing, so this bounds only the pack's share of a flow.
    pub battery_max_kw: DcKw,
    /// The grid charger's own DC limit: reg 128 tops out at 185 A, ≈ 9.47 kW at the pack
    /// voltage — below the pack's 10.24 kW, so a purchase is capped here unless PV is
    /// already charging. The actuator clamps its current register to the same number.
    pub grid_charge_max_kw: DcKw,
    /// PV DC → pack terminals, by power. Measured 94.7 % at 5–9 kW, 94.3 % at 3–5,
    /// 92.8 % at 1.5–3, **87.0 % below 1.5** — the low-power collapse is why the knots
    /// matter.
    pub pv_to_battery: EfficiencyCurve<DcKw, DcKw>,
    /// PV DC → AC bus, by power; serves the site load and the export alike. Measured
    /// 96–98 % — nameplate 97.6 max / 97.0 Euro.
    pub pv_to_grid: EfficiencyCurve<DcKw, AcKw>,
    /// Pack terminals → AC bus, by power; serves the site load and the export alike.
    /// Measured 96.9 % at 6–11 kW, 96.0 % at 3–6, 93.9 % below 3.
    pub battery_to_grid: EfficiencyCurve<DcKw, AcKw>,
    /// Grid → pack terminals, by power — the AC charger leg. **Unmeasured**; the 95 %
    /// figure is the working assumption and should be replaced when measured.
    pub grid_to_battery: EfficiencyCurve<AcKw, DcKw>,
    /// Hard lower SOC bound, `0.0..=1.0`.
    pub soc_floor: f64,
    /// Hard upper SOC bound, `0.0..=1.0`.
    pub soc_ceiling: f64,
    /// Energy-grid resolution in kWh.
    pub energy_step_kwh: DcKwh,
    /// Power-grid resolution in kW.
    pub action_step_kw: AcKw,
    pub terminal: TerminalValue,
}

/// Everything one solve needs.
#[derive(Debug, Clone)]
pub struct Problem {
    pub slots: Vec<Slot>,
    /// Battery energy measured at the instant `now`, which is inside `slots[0]` (the
    /// horizon starts at the MTU containing `now`). The first MTU is planned for the part
    /// that is still ahead, not for its full length.
    pub initial_energy_kwh: DcKwh,
    /// Unix seconds the measurement was taken.
    pub now: i64,
    pub params: Params,
}

/// DM: the plan and why.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// Price beat what a stored kWh is worth later.
    PriceAboveStoredValue {
        price_sell: CzkPerKwh,
        marginal: CzkPerKwh,
    },
    /// The pack cannot absorb the forecast PV, so the surplus has to leave.
    PvCannotBeStored { pv_kw: DcKw, headroom_kw: DcKw },
    /// Buying was cheaper than the energy is worth later.
    BuyBelowFutureValue {
        price_buy: CzkPerKwh,
        marginal: CzkPerKwh,
    },
    /// Keeping the energy; it is worth more later in the horizon.
    HoldForLater {
        marginal: CzkPerKwh,
        best_later_price: CzkPerKwh,
    },
    /// Nothing to trade this MTU.
    Idle,
}

/// One MTU of the plan.
#[derive(Debug, Clone, Serialize)]
pub struct PlanStep {
    pub mtu: Mtu,
    pub action: Action,
    /// Energy that left the pack this MTU (kWh), derived as `E − E'` whenever the pack
    /// discharged and 0 otherwise.
    pub battery_out_kwh: DcKwh,
    /// AC energy that crossed the meter this MTU, **signed**: positive is exported,
    /// negative imported. Deliberately not `action.kw × hours` — on an import the target
    /// is an offer, and only the part the charger and the load actually took is billed.
    pub ac_kwh: AcKwh,
    /// Battery energy at the end of this MTU (kWh).
    pub energy_after_kwh: DcKwh,
    /// Revenue minus cost for this MTU (CZK).
    pub net_czk: Czk,
    /// Marginal value of a stored kWh at the start of this MTU (CZK/kWh).
    pub marginal_czk_per_kwh: CzkPerKwh,
    pub reason: Reason,
}

/// The DP result, ready to publish to HA and to turn into register writes.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// Unix seconds when this plan was computed.
    pub computed_at: i64,
    pub steps: Vec<PlanStep>,
    /// Sum of `PlanStep::net_czk` over the horizon (CZK).
    pub expected_net_czk: Czk,
    /// Unix seconds of the end of the last MTU in the horizon.
    pub horizon_end: i64,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("horizon is empty")]
    EmptyHorizon,
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("no feasible action from {energy_kwh:.2} kWh in MTU {index}")]
    NoFeasibleAction { index: usize, energy_kwh: DcKwh },
}

/// Backward pass: `V_t(E)`. Row `t` is the value of starting MTU `t` with the energy
/// on the level grid; row `len(slots)` is the terminal value.
#[derive(Debug, Clone)]
pub struct ValueTable {
    /// Energy levels in kWh, ascending. The policy band is the normal grid; the measured
    /// energy is added as an extra level when it sits outside that band, so the plan starts
    /// from the real number instead of a clamp.
    levels: Vec<DcKwh>,
    /// `rows[t][i]` = `V_t(levels[i])`.
    rows: Vec<Vec<Czk>>,
    /// Uniform spacing of the normal band, used by [`ValueTable::marginal`].
    step: DcKwh,
}

impl ValueTable {
    pub fn levels(&self) -> &[DcKwh] {
        &self.levels
    }

    /// `V_t` at an arbitrary energy, linearly interpolated between levels.
    pub fn at(&self, index: usize, energy_kwh: DcKwh) -> Czk {
        value_at(&self.rows[index], &self.levels, energy_kwh)
    }

    /// Marginal value of one more kWh at the start of MTU `index` (CZK/kWh), i.e.
    /// `V_{index+1}(E + dE) - V_{index+1}(E)` with `dE` one energy level. A forward
    /// difference, because the backward one degenerates at the floor: `E - dE` is off the
    /// grid there and clamps to `E`. At the ceiling this correctly reads zero — nothing
    /// more can be stored.
    pub fn marginal(&self, index: usize, energy_kwh: DcKwh) -> CzkPerKwh {
        let next = index + 1;
        if next >= self.rows.len() || self.levels.len() < 2 {
            return CzkPerKwh::ZERO;
        }
        let step = self.step;
        if step <= DcKwh::ZERO {
            return CzkPerKwh::ZERO;
        }
        let ahead = value_at(&self.rows[next], &self.levels, energy_kwh + step);
        let here = value_at(&self.rows[next], &self.levels, energy_kwh);
        CzkPerKwh::new((ahead - here).get() / step.get())
    }
}

/// Linear interpolation of one value row, clamped to the ends of the level grid.
fn value_at(row: &[Czk], levels: &[DcKwh], energy_kwh: DcKwh) -> Czk {
    let energy = energy_kwh.get();
    let first = levels[0].get();
    let last = levels[levels.len() - 1].get();
    if energy <= first {
        return row[0];
    }
    if energy >= last {
        return row[row.len() - 1];
    }
    let upper = levels.partition_point(|level| level.get() <= energy);
    let lower = upper - 1;
    let span = levels[upper].get() - levels[lower].get();
    let fraction = (energy - levels[lower].get()) / span;
    row[lower] + (row[upper] - row[lower]) * fraction
}

/// A stored DC kWh reaches the meter only through `battery_to_grid`, so the salvage price
/// is applied to AC energy, not to DC energy. The curve is a function of power, not of
/// energy: the tail is valued at the pack's rated discharge rate (`battery_max_kw`), the
/// best measured efficiency on that path, which errors toward holding rather than dumping.
/// Making the reference rate a `Params` field would be more honest, and should happen if
/// anything else ever needs it.
fn salvage_energy(level: DcKwh, params: &Params) -> AcKwh {
    AcKwh::new(level.get() * params.battery_to_grid.at(params.battery_max_kw))
}

/// Revenue minus cost of one MTU: money is a function of the metered AC only.
fn money(slot: &Slot, flow: &Flow) -> Czk {
    slot.price_sell * flow.ac_out_kwh - slot.price_buy * flow.ac_in_kwh
}

/// Every action the planner may command, `Idle` first so ties resolve toward doing nothing.
fn action_set(params: &Params) -> Vec<Action> {
    let ladder = ActionLadder::new(params);
    let mut actions = vec![Action::Idle];
    actions.extend(
        ladder
            .export_levels()
            .into_iter()
            .filter(|kw| !kw.is_zero())
            .map(|kw| Action::Export { kw }),
    );
    actions.extend(
        ladder
            .import_levels()
            .into_iter()
            .filter(|kw| !kw.is_zero())
            .map(|kw| Action::Import { kw }),
    );
    actions
}

fn validate_params(params: &Params) -> Result<(), PlanError> {
    let invalid = |what: &str| PlanError::InvalidParams(what.to_string());
    if params.capacity_kwh <= DcKwh::ZERO {
        return Err(invalid("capacity_kwh must be positive"));
    }
    if params.energy_step_kwh <= DcKwh::ZERO {
        return Err(invalid("energy_step_kwh must be positive"));
    }
    if params.action_step_kw <= AcKw::ZERO {
        return Err(invalid("action_step_kw must be positive"));
    }
    if params.max_power_kw <= AcKw::ZERO {
        return Err(invalid("max_power_kw must be positive"));
    }
    if params.battery_max_kw <= DcKw::ZERO {
        return Err(invalid("battery_max_kw must be positive"));
    }
    if params.grid_charge_max_kw <= DcKw::ZERO {
        return Err(invalid("grid_charge_max_kw must be positive"));
    }
    if params.soc_floor < 0.0 || params.soc_ceiling > 1.0 {
        return Err(invalid("soc bounds must lie in 0..=1"));
    }
    if params.soc_floor >= params.soc_ceiling {
        return Err(invalid("soc_floor must be below soc_ceiling"));
    }
    Ok(())
}

/// Compute `V_t(E)` for the whole horizon: `rows[t][i]` is the best net revenue obtainable
/// over MTUs `t..T` when MTU `t` starts with `levels[i]` in the pack; row `T` holds the
/// terminal value.
///
/// The scaffolding around the numbers is already written and is **not** to be re-derived:
///
/// * the grid — `levels` run from `soc_floor · capacity` to `soc_ceiling · capacity` in
///   `params.energy_step_kwh` steps, and the ceiling is always a level, because a full pack
///   must stay representable when the band is not a whole number of steps. The **measured**
///   energy is added as an extra level when it sits outside the band: the plant can be at
///   15 % or 95 % at noon, and the plan must start from that number rather than pretend it is
///   at the policy bound (2026-11-06);
/// * `value_at(row, levels, energy)` — a value row read at an arbitrary energy;
/// * `salvage_energy(level, params)` and `params.terminal.czk_per_kwh` — what the pack is
///   worth at the end of the horizon, applied to **AC** energy because a stored DC kWh only
///   reaches the meter through `battery_to_grid`;
/// * `next_energy(E, slot, action, params)` — the physics of one MTU: the next state and the
///   metered AC. The first MTU is planned for the part that is still ahead of `now`;
/// * `money(slot, flow)` — revenue minus cost of one MTU, priced on the metered AC only;
/// * `action_set(params)` — every action the planner may command, `Idle` first so ties
///   resolve toward doing nothing.
///
/// # Invariants the parked tests pin
///
/// * The recursion is `V_t(E) = max over a of money_t(a) + V_{t+1}(E')`, with `E'` clamped
///   to the **grid**; `V_T(E)` is the terminal value of `E`.
/// * The floor is an **export** bound, not a state bound: an action that would take the pack
///   below the floor **while exporting** is infeasible and is skipped. A discharge that only
///   serves the site load below the floor is allowed — the plant does it, and the pack
///   recovers from PV (2026-11-06).
/// * A state with no feasible action is `PlanError::NoFeasibleAction { index, energy_kwh }`.
///   An error, never `f64::INFINITY` and never a silently skipped level.
/// * An empty horizon is `PlanError::EmptyHorizon`; impossible params are
///   `PlanError::InvalidParams` via `validate_params`. Both are checked before any table.
pub fn build_value_table(problem: &Problem) -> Result<ValueTable, PlanError> {
    let params = &problem.params;

    if problem.slots.is_empty() {
        return Err(PlanError::EmptyHorizon);
    }
    validate_params(params)?;

    let floor = params.capacity_kwh * params.soc_floor;
    let ceiling = params.capacity_kwh * params.soc_ceiling;
    let step = params.energy_step_kwh;

    let steps = (((ceiling - floor).get() / step.get()) + 1e-9).floor() as usize;
    let mut levels: Vec<DcKwh> = (0..=steps).map(|k| floor + step * k as f64).collect();
    if levels[levels.len() - 1] < ceiling {
        levels.push(ceiling);
    }
    // Extend the grid in whole steps so the measured energy is representable on both sides
    // of the policy band. The extra levels are not a policy change: charging above the
    // ceiling is still bounded by `headroom`, and the floor still forbids exporting below
    // itself.
    let measured = problem.initial_energy_kwh.max(DcKwh::ZERO).min(params.capacity_kwh);
    while levels[0] > measured {
        levels.insert(0, levels[0] - step);
    }
    while *levels.last().expect("levels is non-empty") < measured {
        levels.push(*levels.last().expect("levels is non-empty") + step);
    }
    let lowest = levels[0];
    let highest = *levels.last().expect("levels is non-empty");

    let actions = action_set(params);
    let horizon = problem.slots.len();

    let mut rows: Vec<Vec<Czk>> = vec![Vec::new(); horizon + 1];
    rows[horizon] = levels
        .iter()
        .map(|&level| params.terminal.czk_per_kwh * salvage_energy(level, params))
        .collect();

    for index in (0..horizon).rev() {
        let slot = &problem.slots[index];
        let hours = step_hours(problem, index);
        let mut row = Vec::with_capacity(levels.len());

        for &energy in &levels {
            let mut best: Option<Czk> = None;

            for &action in &actions {
                let flow = next_energy_hours(energy, slot, action, params, hours);

                // Never export out of the floor. Serving the site load below it is allowed:
                // the 90 W standby is not worth buying from the grid at the floor.
                if flow.energy_after_kwh < floor && flow.ac_out_kwh > AcKwh::ZERO {
                    continue;
                }

                let after = flow.energy_after_kwh.clamp(lowest, highest);
                let value = money(slot, &flow) + value_at(&rows[index + 1], &levels, after);

                best = Some(match best {
                    Some(current) if current >= value => current,
                    _ => value,
                });
            }

            let value = best.ok_or(PlanError::NoFeasibleAction {
                index,
                energy_kwh: energy,
            })?;
            row.push(value);
        }

        rows[index] = row;
    }

    Ok(ValueTable {
        levels,
        rows,
        step,
    })
}

/// The part of MTU `index` that is still ahead of `problem.now`. Only the first MTU can be
/// partial, and it usually is: the horizon starts at the MTU **containing** the tick time,
/// so a plan computed at 10:07 must not count 15 minutes of a 10:00 MTU.
fn step_hours(problem: &Problem, index: usize) -> Hours {
    let slot = &problem.slots[index];
    if index != 0 {
        return slot.mtu.hours;
    }
    let end = slot.mtu.start + (slot.mtu.hours.get() * 3600.0).round() as i64;
    let remaining = (end - problem.now) as f64 / 3600.0;
    // At least a second, at most the MTU: a clock exactly on the boundary keeps the full
    // interval, and a stale `now` past the end does not produce a zero-length step.
    Hours::new(remaining.clamp(1.0 / 3600.0, slot.mtu.hours.get()))
}

/// Walk the table forward from the measured energy and return the plan plus its per-MTU
/// explanations.
///
/// Same action set and same feasibility rule as the backward pass, one MTU at a time, so the
/// forward walk and the table cannot disagree about what a state is worth — the grid is only
/// ever consulted through `table.at(index + 1, after)`.
///
/// Each `PlanStep` carries:
///
/// * `action` — the command; `action.kw` is a *target*, not what the meter saw;
/// * `ac_kwh` — metered AC, signed (`ac_out − ac_in`);
/// * `battery_out_kwh` — `E − E'`, floored at zero, so an import step reports 0;
/// * `energy_after_kwh` — the next state, clamped to the energy grid;
/// * `net_czk` — `money` for this MTU alone;
/// * `marginal_czk_per_kwh` — `table.marginal(index, energy)` taken **before** the step;
/// * `reason` — `explain(slot, action, energy, marginal, &problem.slots[index + 1..], params)`.
///
/// The plan's `expected_net_czk` is the sum of the steps' `net_czk`; `computed_at` is now;
/// `horizon_end` is the instant the last MTU ends, `start + hours · 3600`.
pub fn solve(problem: &Problem) -> Result<Plan, PlanError> {
    let table = build_value_table(problem)?;
    let params = &problem.params;
    let floor = params.capacity_kwh * params.soc_floor;
    let actions = action_set(params);
    let lowest = table.levels()[0];
    let highest = *table.levels().last().expect("levels is non-empty");

    // Start from the measured energy, not a clamp into the policy band: a pack at 15 % is
    // planned at 15 % (and the floor still forbids exporting below 20 %).
    let mut energy = problem
        .initial_energy_kwh
        .max(DcKwh::ZERO)
        .min(params.capacity_kwh)
        .clamp(lowest, highest);
    let mut steps = Vec::with_capacity(problem.slots.len());
    let mut net = Czk::ZERO;

    for (index, slot) in problem.slots.iter().enumerate() {
        let hours = step_hours(problem, index);
        let mut best: Option<(Czk, Action, Flow, DcKwh)> = None;

        for &action in &actions {
            let flow = next_energy_hours(energy, slot, action, params, hours);

            if flow.energy_after_kwh < floor && flow.ac_out_kwh > AcKwh::ZERO {
                continue;
            }

            let after = flow.energy_after_kwh.clamp(lowest, highest);
            let value = money(slot, &flow) + table.at(index + 1, after);

            let better = match best {
                Some((current, ..)) => value > current,
                None => true,
            };

            if better {
                best = Some((value, action, flow, after));
            }
        }

        let Some((_, action, flow, after)) = best else {
            return Err(PlanError::NoFeasibleAction {
                index,
                energy_kwh: energy,
            });
        };

        let earned = money(slot, &flow);
        let marginal = table.marginal(index, energy);

        steps.push(PlanStep {
            mtu: slot.mtu,
            action,
            battery_out_kwh: (energy - after).max(DcKwh::ZERO),
            ac_kwh: flow.ac_out_kwh - flow.ac_in_kwh,
            energy_after_kwh: after,
            net_czk: earned,
            marginal_czk_per_kwh: marginal,
            reason: explain(
                slot,
                action,
                energy,
                marginal,
                &problem.slots[index + 1..],
                params,
            ),
        });

        net += earned;
        energy = after;
    }

    let last = &problem.slots[problem.slots.len() - 1];

    Ok(Plan {
        computed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0),
        steps,
        expected_net_czk: net,
        horizon_end: last.mtu.start + (last.mtu.hours.get() * 3600.0) as i64,
    })
}

/// Why the solver picked this action, in terms a reader can check against the numbers.
fn explain(
    slot: &Slot,
    action: Action,
    energy: DcKwh,
    marginal: CzkPerKwh,
    later: &[Slot],
    params: &Params,
) -> Reason {
    let ceiling = params.capacity_kwh * params.soc_ceiling;
    match action {
        Action::Import { kw } if kw > AcKw::ZERO => Reason::BuyBelowFutureValue {
            price_buy: slot.price_buy,
            marginal,
        },
        Action::Export { kw } if kw > AcKw::ZERO => {
            if slot.price_sell > marginal {
                Reason::PriceAboveStoredValue {
                    price_sell: slot.price_sell,
                    marginal,
                }
            } else {
                Reason::PvCannotBeStored {
                    pv_kw: slot.pv_kw,
                    headroom_kw: (ceiling - energy).max(DcKwh::ZERO) / slot.mtu.hours,
                }
            }
        }
        _ => {
            let spilling = !slot.pv_kw.is_zero() && energy >= ceiling - DcKwh::new(1e-9);
            if spilling {
                Reason::PvCannotBeStored {
                    pv_kw: slot.pv_kw,
                    headroom_kw: DcKwh::ZERO / slot.mtu.hours,
                }
            } else if marginal > slot.price_sell {
                Reason::HoldForLater {
                    marginal,
                    best_later_price: best_later_price(later, params),
                }
            } else {
                Reason::Idle
            }
        }
    }
}

/// The best sell price left in the horizon, or the terminal value when none is left.
fn best_later_price(later: &[Slot], params: &Params) -> CzkPerKwh {
    later
        .iter()
        .fold(params.terminal.czk_per_kwh, |best, slot| {
            if slot.price_sell > best {
                slot.price_sell
            } else {
                best
            }
        })
}

/// What one MTU does to the plant: the next state, and the AC that crossed the meter.
///
/// The pack-side in/out flows are deliberately **not** here. The solver derives them
/// from `E` and `energy_after_kwh` — `E − E'` is the pack's outflow, `E' − E` its
/// inflow — and money is a function of the metered AC, not of what was commanded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flow {
    /// Energy in the pack at the end of the MTU, **before** clamping to the energy grid.
    pub energy_after_kwh: DcKwh,
    /// AC delivered to the grid this MTU, after `pv_to_grid` and `battery_to_grid`. This
    /// is what happened, not the commanded target: a short delivery is possible, and
    /// `solve` prices the delivered number.
    pub ac_out_kwh: AcKwh,
    /// AC drawn from the grid this MTU, billed at the slot's `price_buy`.
    pub ac_in_kwh: AcKwh,
}

/// One MTU of plant physics: `(E, slot, action, params) → Flow`.
///
/// Routing order, every MTU, no exceptions — **the device charges the battery before it
/// exports** (manual §5.7: *"the PV energy will be used to power the load and charge the
/// battery, then the excess will flow to grid"*; corrected 2026-11-06 after the original
/// PV-direct-to-meter assumption):
///
/// 1. PV ([`Slot::pv_kw`], DC) is converted at `pv_to_grid` to cover the site load. AC
///    targets are turned into input power with [`EfficiencyCurve::input_for`] — the curves
///    are keyed on the input side, so dividing a target by `η` needs the inverse.
/// 2. The rest of the PV charges the pack at `pv_to_battery`, bounded by the pack's power
///    limit and the ceiling's headroom. Only DC the pack refuses goes straight to the meter
///    through `pv_to_grid`; a full pack therefore exports PV directly, a pack with room
///    absorbs it.
/// 3. The action's grid-facing target (`Export{kw}`) is met after that: the direct PV spill
///    first, then the pack through `battery_to_grid`. This is the selling-first + TOU
///    behaviour — PV charges the pack while the pack sells to the grid, so the net pack
///    change is the difference of the two legs. `Import{kw}` adds grid energy on top of PV;
///    `Idle` has no target.
/// 4. A purchase covers the load PV left, then charges the pack through `grid_to_battery`
///    with whatever room the PV charge left; the charger's own 185 A limit
///    ([`Params::grid_charge_max_kw`]) applies. Only the part that lands in the pack is
///    billed, so a full pack costs nothing to have "offered" a charge it refused.
/// 5. The plant's own ~0.09 kW standby is already inside [`Slot::load_kw`]; there is no
///    separate idle term.
///
/// The result is **not clamped** — `solve` clamps to the energy grid. It reports the
/// metered AC and the next state; the pack's own net in/out flow is `E` minus
/// [`Flow::energy_after_kwh`] in whichever direction.
pub fn next_energy(energy_kwh: DcKwh, slot: &Slot, action: Action, params: &Params) -> Flow {
    next_energy_hours(energy_kwh, slot, action, params, slot.mtu.hours)
}

/// [`next_energy`] for an explicit fraction of the MTU. The first MTU of a live plan is
/// usually partly elapsed, and `solve` passes the remaining time here.
pub fn next_energy_hours(
    energy_kwh: DcKwh,
    slot: &Slot,
    action: Action,
    params: &Params,
    hours: Hours,
) -> Flow {
    // A zero-length step would divide by zero in the per-hour caps; one second is the
    // smallest interval the plan can represent.
    let hours = Hours::new(hours.get().max(1.0 / 3600.0));

    // What the plant must put on the AC bus this MTU: its own consumption plus whatever the
    // action asks of the meter. The inverter's single rating bounds the sum.
    let target = match action {
        Action::Export { kw } => kw,
        Action::Idle | Action::Import { .. } => AcKw::ZERO,
    };
    let bought = match action {
        Action::Import { kw } => kw,
        Action::Idle | Action::Export { .. } => AcKw::ZERO,
    };
    let demand = (slot.load_kw + target).min(params.max_power_kw);

    // 1. PV serves the site load first.
    let pv_ac = params.pv_to_grid.through(slot.pv_kw);
    let pv_to_load = pv_ac.min(slot.load_kw);
    let pv_dc_load = params.pv_to_grid.input_for(pv_to_load);

    // 2. The rest charges the pack before anything reaches the meter.
    let headroom = (params.capacity_kwh * params.soc_ceiling - energy_kwh).max(DcKwh::ZERO);
    let spare_dc = (slot.pv_kw - pv_dc_load).max(DcKw::ZERO);
    let pv_charge = params
        .pv_to_battery
        .through(spare_dc)
        .min(params.battery_max_kw)
        .min(headroom / hours);
    let pv_dc_charge = params.pv_to_battery.input_for(pv_charge);

    // 3. Only the DC the pack refused can go straight to the grid.
    let spill_dc = (spare_dc - pv_dc_charge).max(DcKw::ZERO);
    let pv_export = params
        .pv_to_grid
        .through(spill_dc)
        .min((demand - pv_to_load).max(AcKw::ZERO));

    // A purchase covers what PV left of the load, before the pack is touched at all.
    let uncovered = (demand - pv_to_load - pv_export).max(AcKw::ZERO);
    let from_import = bought.min(uncovered);

    // The pack covers the remainder of the demand, bounded by its power limit and by what
    // it holds.
    let shortfall = (uncovered - from_import).max(AcKw::ZERO);
    let from_pack = params
        .battery_to_grid
        .input_for(shortfall)
        .min(params.battery_max_kw)
        .min(energy_kwh / hours);

    // A purchase then fills whatever room the PV charge left, within the charger's own
    // limit. Only what the pack can actually take is stored, and only that is billed.
    let room_left = (params.battery_max_kw - pv_charge)
        .max(DcKw::ZERO)
        .min((headroom - pv_charge * hours).max(DcKwh::ZERO) / hours)
        .min(params.grid_charge_max_kw);
    let purchase_spare = (bought - from_import).max(AcKw::ZERO);
    let grid_charge = params
        .grid_to_battery
        .through(purchase_spare)
        .min(room_left);
    let to_pack = pv_charge + grid_charge;

    // What actually crossed the meter: everything delivered minus the plant's own draw.
    let delivered_ac =
        pv_to_load + pv_export + params.battery_to_grid.through(from_pack) + from_import;
    let ac_out = (delivered_ac - slot.load_kw).max(AcKw::ZERO).min(target);

    Flow {
        energy_after_kwh: energy_kwh + to_pack * hours - from_pack * hours,
        ac_out_kwh: ac_out * hours,
        // Everything the meter supplied: the part that went straight to the bus because the
        // site load needed it, plus the part the charger actually took.
        ac_in_kwh: (from_import + params.grid_to_battery.input_for(grid_charge)) * hours,
    }
}

pub fn soc_of(energy_kwh: DcKwh, capacity_kwh: DcKwh) -> f64 {
    energy_kwh.get() / capacity_kwh.get()
}

pub fn energy_of(soc: f64, capacity_kwh: DcKwh) -> DcKwh {
    capacity_kwh * soc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures follow the Phase 3a defaults: 32 kWh pack, one 12 kW AC interface, 20/80 %
    // SOC bounds, 0.32 kWh energy step, 1.2 kW action step, terminal value 7.974 CZK/kWh
    // (measured 2026-11-03). The base `params()` uses ideal (1.0) efficiency curves so the
    // dispatch tests assert behaviour, not physics; the physics tests set their own.
    const CAPACITY: f64 = 32.0;
    const T0: i64 = 1_767_225_600; // arbitrary fixed origin — only differences matter
    const HOUR: i64 = 3600;
    const Q: i64 = 900;

    fn params() -> Params {
        Params {
            capacity_kwh: DcKwh::new(CAPACITY),
            max_power_kw: AcKw::new(12.0),
            battery_max_kw: DcKw::new(10.24),
            grid_charge_max_kw: DcKw::new(9.472),
            pv_to_battery: EfficiencyCurve::flat(1.0),
            pv_to_grid: EfficiencyCurve::flat(1.0),
            battery_to_grid: EfficiencyCurve::flat(1.0),
            grid_to_battery: EfficiencyCurve::flat(1.0),
            soc_floor: 0.20,
            soc_ceiling: 0.80,
            energy_step_kwh: DcKwh::new(0.32),
            action_step_kw: AcKw::new(1.2),
            terminal: TerminalValue {
                czk_per_kwh: CzkPerKwh::new(7.974),
            },
        }
    }

    fn hourly(sell: &[f64], pv_kw: f64, load_kw: f64) -> Vec<Slot> {
        sell.iter()
            .enumerate()
            .map(|(i, &price_sell)| Slot {
                mtu: Mtu {
                    start: T0 + i as i64 * HOUR,
                    hours: Hours::new(1.0),
                },
                price_sell: CzkPerKwh::new(price_sell),
                price_buy: CzkPerKwh::new(5.0),
                pv_kw: DcKw::new(pv_kw),
                load_kw: AcKw::new(load_kw),
            })
            .collect()
    }

    fn quarter_hours(count: usize, price_sell: f64) -> Vec<Slot> {
        (0..count)
            .map(|i| Slot {
                mtu: Mtu {
                    start: T0 + i as i64 * Q,
                    hours: Hours::new(0.25),
                },
                price_sell: CzkPerKwh::new(price_sell),
                price_buy: CzkPerKwh::new(5.0),
                pv_kw: DcKw::ZERO,
                load_kw: AcKw::ZERO,
            })
            .collect()
    }

    fn problem(slots: Vec<Slot>, initial_soc: f64, params: Params) -> Problem {
        let initial_energy_kwh = params.capacity_kwh * initial_soc;
        let now = slots.first().map(|slot| slot.mtu.start).unwrap_or(T0);
        Problem {
            slots,
            initial_energy_kwh,
            now,
            params,
        }
    }

    fn floor_kwh(params: &Params) -> DcKwh {
        params.capacity_kwh * params.soc_floor
    }

    fn ceiling_kwh(params: &Params) -> DcKwh {
        params.capacity_kwh * params.soc_ceiling
    }

    fn export_levels(plan: &Plan) -> Vec<AcKw> {
        plan.steps
            .iter()
            .filter_map(|step| match step.action {
                Action::Export { kw } => Some(kw),
                _ => None,
            })
            .collect()
    }

    // Horizon and price plumbing.
    #[test]
    fn horizon_edge_is_the_last_published_mtu() {
        let slots = hourly(&[1.0, 1.0, 1.0, 1.0], 0.0, 0.0);
        let last = slots.last().expect("slots");
        let expected_end = last.mtu.start + (last.mtu.hours.get() * 3600.0) as i64;

        let plan = solve(&problem(slots, 0.5, params())).expect("solve");

        assert_eq!(plan.horizon_end, expected_end);
        assert_eq!(plan.steps.len(), 4);
    }

    #[test]
    fn dst_day_does_not_assume_ninety_six_mtus() {
        // A 23-hour civil day has 92 quarter-hour MTUs, a 25-hour one has 100. Nothing in
        // the solver may assume 96.
        for count in [92usize, 100] {
            let slots = quarter_hours(count, 0.5);
            let plan = solve(&problem(slots, 0.5, params())).expect("solve");

            assert_eq!(plan.steps.len(), count);
            assert_eq!(plan.horizon_end, T0 + count as i64 * Q);
        }
    }

    #[test]
    fn empty_horizon_is_an_error() {
        let err = solve(&problem(Vec::new(), 0.5, params())).expect_err("empty horizon");
        assert!(matches!(err, PlanError::EmptyHorizon), "got {err:?}");
    }

    #[test]
    fn the_first_mtu_is_planned_for_the_part_still_ahead() {
        // The horizon starts at the MTU containing `now`, so a plan computed halfway through
        // it must not count the full quarter hour of export.
        let slots = hourly(&[20.0], 0.0, 0.0);
        let mut full = problem(slots.clone(), 0.70, params());
        full.now = slots[0].mtu.start;
        let mut half = problem(slots, 0.70, params());
        half.now = half.slots[0].mtu.start + 1800; // 30 min into a 1 h MTU

        let full = solve(&full).expect("full");
        let half = solve(&half).expect("half");

        assert!(
            (half.steps[0].ac_kwh.get() - full.steps[0].ac_kwh.get() / 2.0).abs() < 1e-9,
            "half an MTU must move half the AC: {} vs {}",
            half.steps[0].ac_kwh,
            full.steps[0].ac_kwh
        );
        assert!(
            (half.expected_net_czk.get() - (full.expected_net_czk.get() - full.steps[0].net_czk.get() / 2.0)).abs() < 1e-6,
            "the first MTU's revenue must be scaled too: {} vs {}",
            half.expected_net_czk,
            full.expected_net_czk
        );
    }

    #[test]
    fn a_measurement_outside_the_band_is_planned_as_measured() {
        // Below the floor: the plan must know the pack is at 15 %, not lift it to 20 %. It may
        // still not export there, and with a terminal below the buy price it just holds.
        let mut cheap = params();
        cheap.terminal = TerminalValue {
            czk_per_kwh: CzkPerKwh::new(4.0),
        };
        // One MTU: with two, buying at 5 to sell at 20 below the floor is a valid trade and
        // the plan legitimately imports first. The point here is only the starting energy.
        let below = solve(&problem(hourly(&[20.0], 0.0, 0.0), 0.15, cheap))
            .expect("below the floor");
        assert!(
            !below
                .steps
                .iter()
                .any(|step| matches!(step.action, Action::Export { .. })),
            "exported while below the floor: {:?}",
            below.steps
        );
        assert!(
            (below.steps[0].energy_after_kwh - DcKwh::new(4.8)).abs().get() < 1e-9,
            "the plan did not start from the measured 4.8 kWh: {}",
            below.steps[0].energy_after_kwh
        );

        // Above the ceiling: the extra energy is real and can be sold, so the walk must start
        // at 28.8 kWh rather than pretend the pack is at 25.6.
        // Terminal 7.974, 20.0 CZK MTU: the extra energy is worth selling.
        let above = solve(&problem(hourly(&[20.0; 2], 0.0, 0.0), 0.90, params()))
            .expect("above the ceiling");
        assert!(
            matches!(above.steps[0].action, Action::Export { kw } if kw > AcKw::ZERO),
            "did not sell the energy above the ceiling: {:?}",
            above.steps[0].action
        );
        assert!(
            above.steps[0].energy_after_kwh < DcKwh::new(28.8),
            "the plan started from the ceiling instead of the measured energy"
        );
    }

    // Physics and bounds.
    #[test]
    fn never_exports_below_soc_floor() {
        let params = params();
        let floor = floor_kwh(&params);
        let initial_soc = params.soc_floor;

        let plan =
            solve(&problem(hourly(&[20.0; 4], 0.0, 0.0), initial_soc, params)).expect("solve");

        assert!(
            !matches!(plan.steps[0].action, Action::Export { .. }),
            "exported from a pack already at the floor: {:?}",
            plan.steps[0].action
        );
        for step in &plan.steps {
            assert!(
                step.energy_after_kwh >= floor - DcKwh::new(1e-9),
                "energy {:0.4} kWh fell below the floor {floor:0.4}",
                step.energy_after_kwh
            );
        }
    }

    #[test]
    fn never_charges_above_soc_ceiling() {
        let params = params();
        let ceiling = ceiling_kwh(&params);
        let initial_soc = params.soc_ceiling;

        let plan =
            solve(&problem(hourly(&[0.0; 4], 12.0, 0.0), initial_soc, params)).expect("solve");

        for step in &plan.steps {
            assert!(
                step.energy_after_kwh <= ceiling + DcKwh::new(1e-9),
                "energy {:0.4} kWh rose above the ceiling {ceiling:0.4}",
                step.energy_after_kwh
            );
        }
    }

    #[test]
    fn export_respects_max_power_kw() {
        let params = params();
        let max = params.max_power_kw;

        let plan = solve(&problem(hourly(&[20.0; 4], 0.0, 0.0), 0.70, params)).expect("solve");

        let levels = export_levels(&plan);
        assert!(
            !levels.is_empty(),
            "20 CZK/kWh against a 7.974 terminal with a 70 % pack must export"
        );
        for kw in levels {
            assert!(
                kw > AcKw::ZERO && kw <= max + AcKw::new(1e-9),
                "export {kw} outside (0, {max}]"
            );
        }
    }

    #[test]
    fn pack_power_limit_bounds_both_directions() {
        // 200 A at ~51.2 V ≈ 10.24 kW DC is the pack's own limit, not the 12 kW AC
        // interface rating. A command may ask for the full interface and still get only
        // what the pack can move — in both directions.
        let params = params();
        // Fixture sanity only — the two ratings live in different domains, which is why
        // this compares raw numbers instead of the newtypes.
        assert!(params.battery_max_kw.get() < params.max_power_kw.get());
        let slots = hourly(&[20.0], 0.0, 0.0);
        let slot = &slots[0];

        // Ideal curves: 12 kW commanded from a full pack delivers 10.24 kWh AC per hour.
        let flow = next_energy(
            DcKwh::new(25.0),
            slot,
            Action::Export {
                kw: AcKw::new(12.0),
            },
            &params,
        );
        assert!(
            (flow.ac_out_kwh - AcKwh::new(10.24)).abs().get() < 1e-9,
            "a 12 kW command must deliver only 10.24 kWh AC per hour, got {}",
            flow.ac_out_kwh
        );

        // The limit sits on the DC side: 10.24 kW DC out through a 0.9 path is 9.216 AC,
        // not 10.8 (which is what capping the delivered AC at 12 would have allowed).
        let mut lossy = params.clone();
        lossy.battery_to_grid = EfficiencyCurve::flat(0.9);
        let flow = next_energy(
            DcKwh::new(25.0),
            slot,
            Action::Export {
                kw: AcKw::new(12.0),
            },
            &lossy,
        );
        assert!(
            (flow.ac_out_kwh - AcKwh::new(10.24 * 0.9)).abs().get() < 1e-9,
            "the pack limit must bind before `battery_to_grid`, got {}",
            flow.ac_out_kwh
        );

        // Same bound on the charge side, but the grid charger's 185 A (≈9.47 kW) is below the
        // pack's 10.24 kW, so a 12 kW purchase deposits 9.472 kWh DC per hour and the pack
        // limit is never reached by a grid charge alone.
        let flow = next_energy(
            DcKwh::new(6.4),
            slot,
            Action::Import {
                kw: AcKw::new(12.0),
            },
            &params,
        );
        let deposited = flow.energy_after_kwh - DcKwh::new(6.4);
        assert!(
            (deposited - DcKwh::new(9.472)).abs().get() < 1e-9,
            "a 12 kW purchase must deposit only the charger's 9.472 kWh DC per hour, got {deposited}"
        );
    }

    #[test]
    fn the_grid_charger_limit_is_independent_of_the_pack_limit() {
        // A smaller charger (5 kW) caps the deposit even though the pack could take 10.24.
        let mut params = params();
        params.grid_charge_max_kw = DcKw::new(5.0);
        let slot = hourly(&[0.0], 0.0, 0.0).remove(0);
        let flow = next_energy(
            DcKwh::new(6.4),
            &slot,
            Action::Import {
                kw: AcKw::new(12.0),
            },
            &params,
        );
        let deposited = flow.energy_after_kwh - DcKwh::new(6.4);
        assert!(
            (deposited - DcKwh::new(5.0)).abs().get() < 1e-9,
            "the 5 kW charger must bind before the 10.24 kW pack: got {deposited}"
        );
    }

    #[test]
    fn round_trip_efficiency_is_applied_on_both_sides() {
        // Four measured paths, PV on the DC side. The unit charges the pack from PV before it
        // exports (manual §5.7), so a sell MTU runs both legs: PV→pack at `pv_to_battery` and
        // pack→grid at `battery_to_grid`; only DC the pack refuses goes direct at
        // `pv_to_grid`. `Flow` reports the metered AC and the next state, so the net pack
        // change is recovered as `E' - E`.
        let slot = |pv_kw: f64, load_kw: f64| Slot {
            mtu: Mtu {
                start: T0,
                hours: Hours::new(1.0),
            },
            price_sell: CzkPerKwh::new(1.0),
            price_buy: CzkPerKwh::new(5.0),
            pv_kw: DcKw::new(pv_kw),
            load_kw: AcKw::new(load_kw),
        };

        let mut lossy = params();
        lossy.pv_to_battery = EfficiencyCurve::flat(0.95);
        lossy.pv_to_grid = EfficiencyCurve::flat(0.97);
        lossy.battery_to_grid = EfficiencyCurve::flat(0.9);
        lossy.grid_to_battery = EfficiencyCurve::flat(0.9);

        // No PV: the whole 6 kWh export comes out of the pack at `battery_to_grid`.
        let flow = next_energy(
            DcKwh::new(20.0),
            &slot(0.0, 0.0),
            Action::Export { kw: AcKw::new(6.0) },
            &lossy,
        );
        assert!((flow.ac_out_kwh - AcKwh::new(6.0)).abs().get() < 1e-9);
        assert!(flow.ac_in_kwh.abs().get() < 1e-9);
        let pack_out = DcKwh::new(20.0) - flow.energy_after_kwh;
        assert!(
            (pack_out - DcKwh::new(6.0 / 0.9)).abs().get() < 1e-9,
            "pack outflow must carry 0.9: got {pack_out}"
        );

        // PV covers the load, then charges the pack before anything reaches the meter; the
        // export target is met by the pack plus the PV the pack could not take. With 10 kW
        // of PV and 5.6 kWh of headroom, the pack absorbs 5.6 kWh from PV and discharges
        // 3.353 kWh to the grid, so the net pack change is 2.247 kWh.
        let flow = next_energy(
            DcKwh::new(20.0),
            &slot(10.0, 1.0),
            Action::Export { kw: AcKw::new(6.0) },
            &lossy,
        );
        assert!((flow.ac_out_kwh - AcKwh::new(6.0)).abs().get() < 1e-9);
        let spare_dc: f64 = 10.0 - 1.0 / 0.97;
        let pv_charge = (spare_dc * 0.95).min(5.6); // headroom binds
        let spill_dc = spare_dc - pv_charge / 0.95;
        let pv_export = spill_dc * 0.97;
        let pack_out = (7.0 - 1.0 - pv_export) / 0.9;
        let pack_net = flow.energy_after_kwh - DcKwh::new(20.0);
        assert!(
            (pack_net - DcKwh::new(pv_charge - pack_out)).abs().get() < 1e-9,
            "PV charge minus export discharge must be the net pack change: got {pack_net}"
        );

        // PV is short of the export target: the pack still absorbs what PV offers and then
        // covers the rest of the target, so both legs run in the same MTU.
        let flow = next_energy(
            DcKwh::new(20.0),
            &slot(4.0, 1.0),
            Action::Export { kw: AcKw::new(6.0) },
            &lossy,
        );
        assert!((flow.ac_out_kwh - AcKwh::new(6.0)).abs().get() < 1e-9);
        let spare_dc: f64 = 4.0 - 1.0 / 0.97;
        let pack_net = flow.energy_after_kwh - DcKwh::new(20.0);
        let want = spare_dc * 0.95 - 6.0 / 0.9;
        assert!(
            (pack_net - DcKwh::new(want)).abs().get() < 1e-9,
            "PV charge minus the full export discharge must be the net pack change: got {pack_net}"
        );

        // Idle with PV and no load: the pack takes the surplus only up to its ceiling —
        // 20.0 kWh in a 25.6 kWh pack leaves 5.6 kWh of headroom, and the rest is lost.
        let flow = next_energy(DcKwh::new(20.0), &slot(10.0, 0.0), Action::Idle, &lossy);
        assert!(flow.ac_out_kwh.abs().get() < 1e-9);
        assert!(
            ((flow.energy_after_kwh - DcKwh::new(20.0)) - DcKwh::new(5.6))
                .abs()
                .get()
                < 1e-9,
            "expected to stop at the ceiling, got {}",
            flow.energy_after_kwh
        );

        // Import charges through `grid_to_battery`, on top of any PV.
        let flow = next_energy(
            DcKwh::new(20.0),
            &slot(0.0, 0.0),
            Action::Import { kw: AcKw::new(6.0) },
            &lossy,
        );
        assert!((flow.ac_in_kwh - AcKwh::new(6.0)).abs().get() < 1e-9);
        assert!(
            ((flow.energy_after_kwh - DcKwh::new(20.0)) - DcKwh::new(6.0 * 0.9))
                .abs()
                .get()
                < 1e-9
        );

        // Idle, no PV, load 1 kW for 1 h: the load leaves the pack as AC / η. Ideal
        // curves hide that, so both cases are pinned.
        let flow = next_energy(DcKwh::new(20.0), &slot(0.0, 1.0), Action::Idle, &lossy);
        assert!(
            (flow.energy_after_kwh - DcKwh::new(20.0 - 1.0 / 0.9))
                .abs()
                .get()
                < 1e-9,
            "the load must be paid for in pack-side kWh: got {}",
            flow.energy_after_kwh
        );

        let flow = next_energy(DcKwh::new(20.0), &slot(0.0, 1.0), Action::Idle, &params());
        assert!((flow.energy_after_kwh - DcKwh::new(19.0)).abs().get() < 1e-9);
    }

    #[test]
    fn efficiency_curve_is_linear_between_knots() {
        let curve = EfficiencyCurve::<DcKw, DcKw>::new(vec![
            (DcKw::new(0.0), 1.0),
            (DcKw::new(6.0), 0.9),
            (DcKw::new(12.0), 0.8),
        ])
        .expect("curve");

        for (kw, want) in [
            (0.0, 1.0),
            (3.0, 0.95),
            (6.0, 0.9),
            (9.0, 0.85),
            (12.0, 0.8),
        ] {
            let got = curve.at(DcKw::new(kw));
            assert!((got - want).abs() < 1e-12, "at({kw}) = {got}, want {want}");
        }
    }

    #[test]
    fn efficiency_curve_clamps_outside_the_end_knots() {
        let curve = EfficiencyCurve::<DcKw, DcKw>::new(vec![
            (DcKw::new(1.0), 0.9),
            (DcKw::new(6.0), 0.8),
            (DcKw::new(12.0), 0.7),
        ])
        .expect("curve");

        assert!((curve.at(DcKw::new(0.0)) - 0.9).abs() < 1e-12);
        assert!((curve.at(DcKw::new(-3.0)) - 0.9).abs() < 1e-12);
        assert!((curve.at(DcKw::new(12.0)) - 0.7).abs() < 1e-12);
        assert!((curve.at(DcKw::new(40.0)) - 0.7).abs() < 1e-12);
    }

    #[test]
    fn efficiency_curve_inverts_its_own_forward_map() {
        // Keyed on the input side, so a required output always costs more input.
        let curve = EfficiencyCurve::<DcKw, DcKw>::new(vec![
            (DcKw::new(0.0), 0.95),
            (DcKw::new(6.0), 0.93),
            (DcKw::new(12.0), 0.88),
        ])
        .expect("curve");

        for output_kw in [0.5, 1.2, 3.0, 6.0, 9.5, 11.9] {
            let input_kw = curve.input_for(DcKw::new(output_kw));
            assert!(
                input_kw >= DcKw::new(output_kw) - DcKw::new(1e-9),
                "input {input_kw} below output {output_kw}"
            );
            let round_trip = curve.through(input_kw);
            assert!(
                (round_trip - DcKw::new(output_kw)).abs().get() < 1e-9,
                "round trip at {output_kw}: got {round_trip}"
            );
        }

        let flat = EfficiencyCurve::<DcKw, DcKw>::flat(0.9);
        assert!(
            (flat.input_for(DcKw::new(9.0)) - DcKw::new(10.0))
                .abs()
                .get()
                < 1e-9
        );
        assert!(flat.input_for(DcKw::new(0.0)).abs().get() < 1e-9);
    }

    #[test]
    fn ladder_is_the_uniform_grid_up_to_the_interface_rating() {
        // Levels only: `k · action_step_kw` up to the inverter's AC rating, no efficiency
        // knots — those shape the routing behind a target, not the candidate set.
        let params = params();
        let ladder = ActionLadder::new(&params);

        for levels in [ladder.export_levels(), ladder.import_levels()] {
            assert_eq!(levels.len(), 11, "0 … 12 kW in 1.2 kW steps");
            assert!(levels[0].abs().get() < 1e-12, "0.0 must be present");
            assert!(
                (levels[10] - AcKw::new(12.0)).abs().get() < 1e-9,
                "top level must reach the rating: {}",
                levels[10]
            );
            for (k, kw) in levels.iter().enumerate() {
                assert!(
                    (*kw - AcKw::new(k as f64 * 1.2)).abs().get() < 1e-9,
                    "level {k} is {kw}, not on the step grid"
                );
                assert!(
                    *kw <= AcKw::new(12.0) + AcKw::new(1e-9),
                    "level {kw} past the rating"
                );
            }
            assert!(
                levels.windows(2).all(|pair| pair[0] < pair[1]),
                "levels must ascend strictly"
            );
        }
    }

    // Dispatch behaviour.
    #[test]
    fn sells_at_the_peak_when_the_pack_can_reach_it() {
        let plan = solve(&problem(
            hourly(&[1.0, 5.0, 9.0, 1.0], 0.0, 0.0),
            0.70,
            params(),
        ))
        .expect("solve");

        assert!(
            matches!(plan.steps[2].action, Action::Export { kw } if kw > AcKw::ZERO),
            "the 9.0 CZK/kWh peak was not sold: {:?}",
            plan.steps[2].action
        );
        for index in [0usize, 1, 3] {
            assert!(
                !matches!(plan.steps[index].action, Action::Export { .. }),
                "MTU {index} sold below the 7.974 terminal, got {:?}",
                plan.steps[index].action
            );
        }
    }

    #[test]
    fn exports_midday_surplus_when_the_pack_is_full() {
        let params = params();
        let initial_soc = params.soc_ceiling;

        let plan =
            solve(&problem(hourly(&[0.5; 4], 10.0, 1.0), initial_soc, params)).expect("solve");

        assert!(
            matches!(plan.steps[0].action, Action::Export { kw } if kw > AcKw::ZERO),
            "PV surplus against a full pack must leave: {:?}",
            plan.steps[0].action
        );
    }

    #[test]
    fn holds_energy_when_a_later_mtu_pays_more() {
        let plan = solve(&problem(hourly(&[1.0, 9.0], 0.0, 0.0), 0.70, params())).expect("solve");

        assert!(
            !matches!(plan.steps[0].action, Action::Export { .. }),
            "sold at 1.0 with a 9.0 MTU next: {:?}",
            plan.steps[0].action
        );
        assert!(
            matches!(plan.steps[1].action, Action::Export { kw } if kw > AcKw::ZERO),
            "did not sell the 9.0 MTU: {:?}",
            plan.steps[1].action
        );
    }

    #[test]
    fn slow_discharge_beats_burst_when_power_efficiency_is_concave() {
        // η falls faster at high power (0.98 → 0.95 → 0.80 across 0…12 kW), so spreading
        // the same energy across three equally-priced MTUs loses less than dumping it.
        let mut params = params();
        params.battery_to_grid = EfficiencyCurve::new(vec![
            (DcKw::new(0.0), 0.98),
            (DcKw::new(6.0), 0.95),
            (DcKw::new(12.0), 0.80),
        ])
        .expect("curve");
        let max = params.max_power_kw;

        let plan = solve(&problem(hourly(&[9.0; 3], 0.0, 0.0), 0.70, params)).expect("solve");

        let levels = export_levels(&plan);
        assert!(
            levels.len() >= 2,
            "dumped the pack in a single MTU: {levels:?}"
        );
        for kw in levels {
            assert!(
                kw < max - AcKw::new(1e-9),
                "exported at the power cap: {kw}"
            );
        }
    }

    #[test]
    fn buys_only_when_the_spread_clears_efficiency_and_fees() {
        // Terminal pinned below the 5.0 buy price, so the tail alone never justifies
        // importing; then one MTU that pays well above buy.
        let mut params = params();
        params.terminal = TerminalValue {
            czk_per_kwh: CzkPerKwh::new(4.0),
        };
        let initial_soc = params.soc_floor;

        let no_spread = solve(&problem(
            hourly(&[4.0; 4], 0.0, 0.0),
            initial_soc,
            params.clone(),
        ))
        .expect("solve");
        assert!(
            !no_spread
                .steps
                .iter()
                .any(|step| matches!(step.action, Action::Import { .. })),
            "imported at 5.0 to sell at 4.0: {:?}",
            no_spread.steps
        );

        let spread = solve(&problem(
            hourly(&[4.0, 12.0], 0.0, 0.0),
            initial_soc,
            params,
        ))
        .expect("solve");
        assert!(
            matches!(spread.steps[0].action, Action::Import { kw } if kw > AcKw::ZERO),
            "did not buy at 5.0 before a 12.0 MTU: {:?}",
            spread.steps[0].action
        );
    }

    // Explanation.
    #[test]
    fn marginal_value_is_monotone_in_energy() {
        let slots = hourly(&[9.0; 4], 0.0, 0.0);
        let horizon = slots.len();
        let table = build_value_table(&problem(slots, 0.70, params())).expect("value table");

        let levels = table.levels();
        assert!(levels.len() >= 2, "energy grid too coarse");
        let low = levels[0];
        let high = *levels.last().expect("levels");

        for index in 0..horizon {
            let m_low = table.marginal(index, low);
            let m_high = table.marginal(index, high);

            assert!(m_low.get().is_finite() && m_high.get().is_finite());
            assert!(
                m_low > CzkPerKwh::ZERO,
                "a stored kWh is worth nothing at {index}"
            );
            assert!(
                m_high <= m_low + CzkPerKwh::new(1e-9),
                "marginal value rises with energy at MTU {index}: {m_low} → {m_high}"
            );
        }
    }

    #[test]
    fn every_step_carries_a_reason_consistent_with_its_action() {
        let plan = solve(&problem(
            hourly(&[1.0, 5.0, 9.0, 1.0], 0.0, 0.0),
            0.70,
            params(),
        ))
        .expect("solve");

        for (index, step) in plan.steps.iter().enumerate() {
            assert!(
                step.net_czk.get().is_finite()
                    && step.marginal_czk_per_kwh.get().is_finite()
                    && step.battery_out_kwh.get().is_finite()
                    && step.energy_after_kwh.get().is_finite(),
                "step {index} carries a non-finite field"
            );

            let consistent = match (step.action, step.reason) {
                (Action::Idle, Reason::Idle | Reason::HoldForLater { .. }) => true,
                (Action::Idle, Reason::PvCannotBeStored { .. }) => true,
                (
                    Action::Export { kw },
                    Reason::PriceAboveStoredValue { .. } | Reason::PvCannotBeStored { .. },
                ) => kw > AcKw::ZERO,
                (Action::Import { kw }, Reason::BuyBelowFutureValue { .. }) => kw > AcKw::ZERO,
                _ => false,
            };
            assert!(
                consistent,
                "step {index}: {:?} explained by {:?}",
                step.action, step.reason
            );
        }
    }
}
