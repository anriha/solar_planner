//! `PlannerActor`: once a minute, turn telemetry + the horizon + directives into a plan.
//!
//! # Contract
//!
//! * One [`Tick`] is one planning cycle: read the directives, read the measured state of
//!   charge, make sure the horizon is fresh, solve, publish.
//! * **The solve never waits on the network.** The horizon refreshes on its own schedule;
//!   a stale window is a legal input, and a failed refresh only costs freshness.
//! * The plan is published through `StateActor`, so it rides on every WebSocket frame.
//!   Withdrawing it (`Update::Plan(None)`) is how "the planner is off" is expressed.
//! * Device facts (pack capacity, pack power limit, the measured efficiency knots) are
//!   constants here: they change when the hardware does, not when a policy does. Policy
//!   lives in [`Directives`].

use kameo::actor::ActorRef;
use kameo::message::Message;
use kameo::prelude::Context;
use kameo::Actor;
use thiserror::Error;

use crate::bus::{BusActor, WriteRegister};
use crate::directives::{Directives, DirectivesActor, GetCurrent};
use crate::planner::actuator::{self, ActuatorParams};
use crate::planner::forecast::PvSource;
use crate::planner::horizon::{HorizonActor, HorizonError, HorizonParams};
use crate::planner::price::PriceSource;
use crate::planner::{
    BOOTSTRAP_TERMINAL_CZK_PER_KWH, EfficiencyCurve, Params, Plan, PlanError, Problem,
    TerminalValue, solve,
};
use crate::state::{GetState, StateActor, Update};
use crate::telemetry::{ExtractError, PlanningSignals};
use crate::units::{AcKw, CzkPerKwh, DcKw, DcKwh};

/// Reg 102 = 628 Ah at ~51.2 V.
const CAPACITY_KWH: f64 = 32.0;
/// 200 A at ~51.2 V from the BMS — not regs 108/109.
const BATTERY_MAX_KW: f64 = 10.24;
/// Reg 128's 185 A at the same ~51.2 V; the grid charger is below the pack's own limit.
const GRID_CHARGE_MAX_KW: f64 = 9.472;
/// The plant's own AC draw: inverter standby plus the Pi.
const PLANT_LOAD_KW: f64 = 0.09;
/// Bootstrap terminal value, used until the price cache has produced a measured one.
const TERMINAL_CZK_PER_KWH: f64 = BOOTSTRAP_TERMINAL_CZK_PER_KWH;

/// How often the actor re-plans.
pub const PLANNER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the actuator may keep executing an old plan when solves stop succeeding. A
/// healthy planner produces one every minute; ten minutes of silence means something is
/// wrong, and the inverter is put in the safe state instead of following a stale schedule.
pub const DISARM_AFTER_SECS: i64 = 600;

#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("the planner is disabled by directives")]
    Disabled,
    #[error("the planner is disarmed by directives")]
    Disarmed,
    #[error("state actor unavailable")]
    State,
    #[error("directives actor unavailable")]
    Directives,
    #[error("no battery reading yet: {0}")]
    Signals(#[from] ExtractError),
    #[error(transparent)]
    Horizon(#[from] HorizonError),
    #[error(transparent)]
    Solve(#[from] PlanError),
}

/// Device physics + the directives' policy, as the DP wants it.
///
/// `terminal` is passed in rather than computed here: it is a statistic over the price
/// history, which the horizon owns, and deriving it from anything else would mean a second
/// copy of the rule.
pub fn device_params(directives: &Directives, terminal: TerminalValue) -> Params {
    Params {
        capacity_kwh: DcKwh::new(CAPACITY_KWH),
        max_power_kw: AcKw::new(directives.max_power_kw),
        battery_max_kw: DcKw::new(BATTERY_MAX_KW),
        grid_charge_max_kw: DcKw::new(GRID_CHARGE_MAX_KW),
        // PV → pack: 94.7 % at 5–9 kW, 94.3 % at 3–5, 92.8 % at 1.5–3, 87.0 % below 1.5.
        pv_to_battery: EfficiencyCurve::new(vec![
            (DcKw::new(0.5), 0.870),
            (DcKw::new(1.5), 0.870),
            (DcKw::new(3.0), 0.928),
            (DcKw::new(6.0), 0.943),
            (DcKw::new(10.0), 0.947),
        ])
        .expect("static knots"),
        // PV → AC: 96–98 % across the range, nameplate 97.0 Euro.
        pv_to_grid: EfficiencyCurve::flat(0.97),
        // Pack → AC: 96.9 % at 6–11 kW, 96.0 % at 3–6, 93.9 % below 3.
        battery_to_grid: EfficiencyCurve::new(vec![
            (DcKw::new(0.5), 0.939),
            (DcKw::new(3.0), 0.939),
            (DcKw::new(6.0), 0.960),
            (DcKw::new(BATTERY_MAX_KW), 0.969),
        ])
        .expect("static knots"),
        // AC → pack: unmeasured, 95 % is the working assumption.
        grid_to_battery: EfficiencyCurve::flat(0.95),
        soc_floor: directives.soc_floor.get() / 100.0,
        soc_ceiling: directives.soc_ceiling.get() / 100.0,
        energy_step_kwh: DcKwh::new(directives.energy_step_kwh),
        action_step_kw: AcKw::new(directives.action_step_kw),
        terminal,
    }
}

/// The terminal value to solve with before the price cache has measured one.
pub fn bootstrap_terminal() -> TerminalValue {
    TerminalValue {
        czk_per_kwh: CzkPerKwh::new(TERMINAL_CZK_PER_KWH),
    }
}

/// The actuator's slice of policy: the SOC band it may write into the TOU targets.
pub fn actuator_params(directives: &Directives) -> ActuatorParams {
    ActuatorParams {
        soc_floor_pct: directives.soc_floor.get(),
        soc_ceiling_pct: directives.soc_ceiling.get(),
    }
}

/// The horizon's slice of policy: what selling nets, what buying costs, what the plant
/// draws on its own. The buy side is spot + the two charges (2026-11-06); there is no flat
/// tariff any more, so nothing here may collapse back into a single price.
pub fn horizon_params(directives: &Directives) -> HorizonParams {
    HorizonParams {
        sell_fee: CzkPerKwh::new(directives.sell_fee_czk.get()),
        buy_distribution: CzkPerKwh::new(directives.buy_distribution_czk.get()),
        buy_seller_fee: CzkPerKwh::new(directives.buy_seller_fee_czk.get()),
        load_kw: AcKw::new(PLANT_LOAD_KW),
        forecast_calibration: directives.forecast_calibration,
    }
}

/// Whether this cycle may put anything on the bus.
///
/// The one place that decides it, so "the planner looked at the inverter but did not
/// touch it" is a property of two inputs rather than of a call site tucked into `cycle`.
pub fn writes_allowed(directives: &Directives, link_ok: bool) -> bool {
    directives.actuator_enabled && link_ok
}

/// One planning cycle.
pub struct Tick;

#[derive(Actor)]
pub struct PlannerActor<P: PriceSource, F: PvSource> {
    horizon: HorizonActor<P, F>,
    state: ActorRef<StateActor>,
    directives: ActorRef<DirectivesActor>,
    /// `None` means dry-run: plan, publish, write nothing.
    bus: Option<ActorRef<BusActor>>,
    /// When the last plan was produced successfully. The dead-man disarms the inverter when
    /// this goes stale (see [`DISARM_AFTER_SECS`]).
    last_plan_at: Option<i64>,
    /// True while the previous cycle was allowed to write. A true→false transition is a
    /// deliberate stop and disarms.
    armed: bool,
}

impl<P: PriceSource, F: PvSource> PlannerActor<P, F> {
    pub fn new(
        horizon: HorizonActor<P, F>,
        state: ActorRef<StateActor>,
        directives: ActorRef<DirectivesActor>,
        bus: Option<ActorRef<BusActor>>,
    ) -> Self {
        Self {
            horizon,
            state,
            directives,
            bus,
            last_plan_at: None,
            armed: false,
        }
    }

    /// Write one target list to the bus, stopping at the first refusal.
    ///
    /// Only registers that disagree with the inverter's readback are written unless `force`
    /// is set — a register the snapshot does not know about is an error, never an excuse to
    /// write every tick.
    async fn write_targets(
        &self,
        targets: &[actuator::Target],
        observed: &std::collections::BTreeMap<String, crate::decode::Value>,
        force: bool,
    ) -> bool {
        let Some(bus) = &self.bus else {
            return false;
        };
        let writes = if force {
            targets
                .iter()
                .map(|target| (target.name.clone(), target.write.clone()))
                .collect()
        } else {
            match actuator::writes_for(targets, observed) {
                Ok(writes) => writes,
                Err(error) => {
                    eprintln!("[planner] {error}; not writing");
                    return false;
                }
            }
        };
        if writes.is_empty() {
            return true;
        }
        let count = writes.len();
        for (name, value) in writes {
            if let Err(error) = bus.ask(WriteRegister { name: name.clone(), value }).await {
                eprintln!("[planner] write {name} refused ({error}); stopping the batch");
                return false;
            }
        }
        eprintln!("[planner] applied {count} register change(s)");
        true
    }

    /// Pushes the plan onto the inverter. Fail closed: any refused write is followed by a
    /// [`Self::disarm`], not by leaving the mode where the batch happened to stop.
    async fn apply(
        &self,
        plan: &Plan,
        observed: &std::collections::BTreeMap<String, crate::decode::Value>,
        directives: &Directives,
        now: i64,
    ) -> bool {
        let desired = actuator::desired(plan, now, &actuator_params(directives));
        if desired.charge_current_limited {
            eprintln!(
                "[planner] grid charge {:.1} kW exceeds the 185 A charger limit; commanding 185 A (≈9.47 kW DC)",
                desired.charge_requested_kw
            );
        }
        let ok = self
            .write_targets(&actuator::targets(&desired), observed, false)
            .await;
        if !ok {
            self.disarm(observed).await;
        }
        ok
    }

    /// Put the inverter in the safe state: hold the pack at 100 %, sell nothing, charge
    /// nothing from the grid. Idempotent; diff-filtered so a repeated call is silent.
    ///
    /// This is the **only** thing that runs when the actuator is switched off, the planner is
    /// disabled or disarmed, the plan has gone stale, the process is shutting down, or a
    /// write was refused. Without it the inverter would keep executing the last schedule —
    /// including `selling_first` — until something else happened to touch it.
    async fn disarm(&self, observed: &std::collections::BTreeMap<String, crate::decode::Value>) {
        let targets = actuator::disarm_targets();
        if !self.write_targets(&targets, observed, false).await {
            // The diff-filtered pass may have failed on a register the snapshot disagrees
            // with; write the safe globals unconditionally before giving up.
            let globals = actuator::disarm_globals();
            let _ = self.write_targets(&globals, observed, true).await;
        }
    }

    /// Disarm when no plan has been produced for [`DISARM_AFTER_SECS`], but only when the
    /// actuator switch is on (a report-only deploy must not touch the registers).
    async fn disarm_if_stale(&mut self, now: i64) {
        let last = self.last_plan_at.unwrap_or(0);
        if now - last < DISARM_AFTER_SECS {
            return;
        }
        let Ok(directives) = self.directives.ask(GetCurrent).await else {
            return;
        };
        if !directives.actuator_enabled {
            return;
        }
        let Ok(payload) = self.state.ask(GetState).await else {
            return;
        };
        if payload.link_ok {
            eprintln!("[planner] no plan for {}s; disarming", now - last);
            self.disarm(&payload.values).await;
            self.armed = false;
        }
    }

    /// Plans once and publishes the result. Returns the plan so the driver can log it.
    pub async fn cycle(&mut self, now: i64) -> Result<Plan, PlannerError> {
        let directives = self
            .directives
            .ask(GetCurrent)
            .await
            .map_err(|_| PlannerError::Directives)?;
        let payload = self.state.ask(GetState).await.map_err(|_| PlannerError::State)?;

        if !directives.planner_enabled {
            // Withdraw the plan rather than leaving a stale one on screen. "Planner off" is
            // a stronger statement than "actuator off": nobody is deciding, so the plant is
            // put in the safe state rather than left on the last schedule.
            if payload.link_ok {
                self.disarm(&payload.values).await;
            }
            self.armed = false;
            let _ = self.state.tell(Update::Plan(None)).send().await;
            return Err(PlannerError::Disabled);
        }

        // An explicit, operator-visible disarmed state. It overrides the actuator switch on
        // purpose: this is how the plant is put in a known state by hand.
        if directives.disarm {
            if payload.link_ok {
                self.disarm(&payload.values).await;
            }
            self.armed = false;
            let _ = self.state.tell(Update::Plan(None)).send().await;
            return Err(PlannerError::Disarmed);
        }

        // Directives are live: the horizon takes the policy half on every cycle.
        self.horizon.set_params(horizon_params(&directives));

        // The horizon owns its own refresh, its own budget and its own per-source commits;
        // a failure here leaves the previous window in place.
        if self.horizon.needs_refresh(now)
            && let Some(report) = self.horizon.refresh(now).await
        {
            // One line per failing feed: they fail for different reasons (OTE publication
            // lag vs a forecast.solar 429) and the fix differs.
            if let Err(error) = &report.prices {
                eprintln!("[planner] price refresh failed, serving the cached window: {error}");
            }
            if let Err(error) = &report.forecast {
                eprintln!("[planner] forecast refresh failed, serving the cached window: {error}");
            }
        }

        let slots = self.horizon.build(now)?;

        let signals = PlanningSignals::extract_values(&payload.values)?;

        let params = device_params(
            &directives,
            self.horizon.terminal().unwrap_or_else(bootstrap_terminal),
        );
        let problem = Problem {
            slots,
            initial_energy_kwh: params.capacity_kwh * signals.soc_fraction(),
            now,
            params,
        };
        let plan = solve(&problem)?;

        // Publish first: the plan is useful to look at even if a write is refused.
        let _ = self.state.tell(Update::Plan(Some(plan.clone()))).send().await;

        // Two independent stops, both fail closed: the directive is off (the deploy
        // default), or the last poll did not land — a write shaped by a stale SOC and
        // compared against a stale register image is worse than no write. Switching the
        // actuator off is an instruction to stop, so it disarms on the transition.
        if writes_allowed(&directives, payload.link_ok) {
            self.armed = self
                .apply(&plan, &payload.values, &directives, now)
                .await;
        } else {
            if self.armed && payload.link_ok {
                self.disarm(&payload.values).await;
            }
            self.armed = false;
            if !payload.link_ok {
                eprintln!("[planner] link down, not writing");
            }
        }

        self.last_plan_at = Some(now);
        Ok(plan)
    }
}

impl<P: PriceSource, F: PvSource> Message<Tick> for PlannerActor<P, F> {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Tick,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let now = crate::telemetry::unix_now() as i64;
        match self.cycle(now).await {
            Ok(plan) => eprintln!(
                "[planner] {} steps to {}, expected {:.2} CZK",
                plan.steps.len(),
                plan.horizon_end,
                plan.expected_net_czk.get()
            ),
            Err(PlannerError::Disabled) | Err(PlannerError::Disarmed) => {}
            Err(error) => {
                eprintln!("[planner] no plan this tick: {error}");
                self.disarm_if_stale(now).await;
            }
        }
    }
}

/// The driver: one `Tick` per [`PLANNER_INTERVAL`], like the bus' own ticker.
pub async fn drive<P: PriceSource, F: PvSource>(
    planner: ActorRef<PlannerActor<P, F>>,
    interval: std::time::Duration,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        planner
            .tell(Tick)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("planner actor died"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each case removes exactly one of the two reasons a write may happen, so a guard that
    // quietly loses either one turns one of these red.
    #[test]
    fn a_write_needs_both_the_switch_and_a_live_link() {
        let mut directives = Directives::default();
        assert!(!directives.actuator_enabled, "a fresh deploy must not write");
        assert!(
            !writes_allowed(&directives, true),
            "the default deploy plans and reports only"
        );

        directives.actuator_enabled = true;
        assert!(writes_allowed(&directives, true), "switched on with a live link");
        assert!(
            !writes_allowed(&directives, false),
            "a dead link never writes, even with the switch on"
        );
    }
}
