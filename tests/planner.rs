//! End-to-end through the planner's own seams: fake feeds in, a published plan out.
//!
//! No Modbus, no network — the fakes stand in for OTE and forecast.solar, and the real
//! `StateActor` / `DirectivesActor` are used, because the point is the wiring, not the DP
//! (which is pinned by the unit tests in `planner.rs`).

use std::collections::BTreeMap;
use std::future::Future;

use kameo::actor::Spawn;
use solar_controller::{
    decode::Value,
    directives::{ApplyDirective, Directives, DirectivesActor, GetCurrent},
    planner::{
        Mtu,
        actor::{PlannerActor, horizon_params},
        forecast::{ForecastError, PvPoint, PvSource},
        horizon::HorizonActor,
        price::{PriceError, PricePoint, PriceSource},
    },
    state::{GetState, StateActor, Update},
    telemetry::Snapshot,
    units::Hours,
};

/// 2026-11-04T00:00 Prague (CET, UTC+1).
const T0: i64 = 1_793_746_800;

struct FakePrices(Vec<PricePoint>);

impl PriceSource for FakePrices {
    fn fetch(
        &self,
        _from: jiff::civil::Date,
        _to: jiff::civil::Date,
    ) -> impl Future<Output = Result<Vec<PricePoint>, PriceError>> + Send {
        let points = self.0.clone();
        async move { Ok(points) }
    }
}

struct FakePv(Vec<PvPoint>);

impl PvSource for FakePv {
    fn fetch(
        &self,
        _from: jiff::civil::Date,
        _to: jiff::civil::Date,
    ) -> impl Future<Output = Result<Vec<PvPoint>, ForecastError>> + Send {
        let points = self.0.clone();
        async move { Ok(points) }
    }
}

fn window(count: usize, eur_per_mwh: f64) -> Vec<PricePoint> {
    (0..count)
        .map(|k| PricePoint {
            mtu: Mtu {
                start: T0 + k as i64 * 900,
                hours: Hours::new(0.25),
            },
            eur_per_mwh,
            eur_rate: 25.0,
        })
        .collect()
}

fn snapshot_with_soc(soc: f64) -> Snapshot {
    let mut values = BTreeMap::new();
    values.insert("battery_soc".to_string(), Value::Number(soc));
    values.insert("battery_power".to_string(), Value::Number(0.0));
    Snapshot {
        link_ok: true,
        ts: 1,
        values,
    }
}

#[tokio::test]
async fn a_cycle_publishes_a_plan_through_the_state_actor() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateActor::spawn(StateActor::default());
    let directives = DirectivesActor::spawn(
        DirectivesActor::new(dir.path().join("directives.json"), state.clone()).unwrap(),
    );

    // The bridge's last poll, as the planner reads it.
    let _ = state.tell(Update::Good(snapshot_with_soc(60.0))).send().await;

    let horizon = HorizonActor::new(
        FakePrices(window(8, 300.0)),
        FakePv(Vec::new()),
        horizon_params(&Directives::default()),
    );
    let mut planner = PlannerActor::new(horizon, state.clone(), directives, None);

    let plan = planner.cycle(T0).await.expect("a cycle with feeds and a reading");

    assert_eq!(plan.steps.len(), 8, "one step per published MTU");
    assert_eq!(plan.steps[0].mtu.start, T0);

    let payload = state.ask(GetState).await.expect("state actor alive");
    let published = payload.plan.expect("the plan rides on the state payload");
    assert_eq!(published.steps.len(), plan.steps.len());
    assert!(payload.link_ok, "planning does not disturb the link flag");
}

#[tokio::test]
async fn the_default_deploy_plans_while_the_actuator_is_off() {
    // A deploy that hands the planner a bus handle but leaves `actuator_enabled` at its
    // default must still do the whole cycle: solve, publish, nothing written. If the guard
    // ever short-circuits the cycle instead of only the write, the plan disappears.
    let dir = tempfile::tempdir().unwrap();
    let state = StateActor::spawn(StateActor::default());
    let directives = DirectivesActor::spawn(
        DirectivesActor::new(dir.path().join("directives.json"), state.clone()).unwrap(),
    );
    let _ = state.tell(Update::Good(snapshot_with_soc(60.0))).send().await;

    let horizon = HorizonActor::new(
        FakePrices(window(4, 300.0)),
        FakePv(Vec::new()),
        horizon_params(&Directives::default()),
    );
    let mut planner = PlannerActor::new(horizon, state.clone(), directives.clone(), None);

    let current = directives
        .ask(GetCurrent)
        .await
        .expect("directives actor alive");
    assert!(!current.actuator_enabled);

    let plan = planner.cycle(T0).await.expect("a read-only cycle plans");
    assert_eq!(plan.steps.len(), 4);

    let payload = state.ask(GetState).await.expect("state actor alive");
    assert!(payload.plan.is_some(), "the report-only deploy still reports");
}

#[tokio::test]
async fn a_disarmed_planner_withdraws_its_plan() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateActor::spawn(StateActor::default());
    let directives = DirectivesActor::spawn(
        DirectivesActor::new(dir.path().join("directives.json"), state.clone()).unwrap(),
    );
    let _ = state.tell(Update::Good(snapshot_with_soc(60.0))).send().await;

    let horizon = HorizonActor::new(
        FakePrices(window(4, 300.0)),
        FakePv(Vec::new()),
        horizon_params(&Directives::default()),
    );
    let mut planner = PlannerActor::new(horizon, state.clone(), directives.clone(), None);

    planner.cycle(T0).await.expect("first cycle");
    directives
        .ask(ApplyDirective {
            name: "disarm".to_string(),
            value: serde_json::json!(true),
        })
        .await
        .expect("directives actor alive");

    let error = planner.cycle(T0 + 900).await.expect_err("disarmed");
    assert!(matches!(
        error,
        solar_controller::planner::actor::PlannerError::Disarmed
    ));

    let payload = state.ask(GetState).await.expect("state actor alive");
    assert!(
        payload.plan.is_none(),
        "a disarmed planner leaves no plan behind"
    );
}

#[tokio::test]
async fn a_disabled_planner_withdraws_its_plan() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateActor::spawn(StateActor::default());
    let directives = DirectivesActor::spawn(
        DirectivesActor::new(dir.path().join("directives.json"), state.clone()).unwrap(),
    );
    let _ = state.tell(Update::Good(snapshot_with_soc(60.0))).send().await;

    let horizon = HorizonActor::new(
        FakePrices(window(4, 300.0)),
        FakePv(Vec::new()),
        horizon_params(&Directives::default()),
    );
    let mut planner = PlannerActor::new(horizon, state.clone(), directives.clone(), None);

    // Publish once, then switch the planner off from HA's side.
    planner.cycle(T0).await.expect("first cycle");
    directives
        .ask(ApplyDirective {
            name: "planner_enabled".to_string(),
            value: serde_json::json!(false),
        })
        .await
        .expect("directives actor alive");

    let error = planner.cycle(T0 + 900).await.expect_err("disabled");
    assert!(matches!(error, solar_controller::planner::actor::PlannerError::Disabled));

    let payload = state.ask(GetState).await.expect("state actor alive");
    assert!(payload.plan.is_none(), "a disabled planner leaves no plan behind");
}

#[tokio::test]
async fn a_forecast_past_the_published_prices_still_produces_a_plan() {
    // The 2026-11-06 production failure, end to end through the real actor: the deployed
    // planner ran all morning with "PV is forecast for <tomorrow 07:00>, an MTU that has no
    // published price" and published nothing. forecast.solar answers today and tomorrow;
    // OTE publishes tomorrow only in the early afternoon. The priced window is the horizon,
    // so the PV tail is dropped instead of starving the day.
    let dir = tempfile::tempdir().unwrap();
    let state = StateActor::spawn(StateActor::default());
    let directives = DirectivesActor::spawn(
        DirectivesActor::new(dir.path().join("directives.json"), state.clone()).unwrap(),
    );
    let _ = state.tell(Update::Good(snapshot_with_soc(60.0))).send().await;

    // Today's prices only, and a forecast that reaches all the way to tomorrow evening.
    let mut forecast = Vec::new();
    for k in 0..8 {
        forecast.push(PvPoint {
            mtu: Mtu {
                start: T0 + k as i64 * 900,
                hours: Hours::new(0.25),
            },
            dc_kwh: solar_controller::units::DcKwh::new(0.9),
        });
    }
    for k in 0..8 {
        forecast.push(PvPoint {
            mtu: Mtu {
                start: T0 + 100 * 900 + k * 900,
                hours: Hours::new(0.25),
            },
            dc_kwh: solar_controller::units::DcKwh::new(0.9),
        });
    }

    let horizon = HorizonActor::new(
        FakePrices(window(8, 300.0)),
        FakePv(forecast),
        horizon_params(&Directives::default()),
    );
    let mut planner = PlannerActor::new(horizon, state.clone(), directives, None);

    let plan = planner.cycle(T0).await.expect("the unpriced tail is not fatal");

    assert_eq!(plan.steps.len(), 8, "the horizon stops at the last published price");
    assert_eq!(
        plan.horizon_end,
        T0 + 8 * 900,
        "the plan ends with the prices, not with the forecast"
    );
    let payload = state.ask(GetState).await.expect("state actor alive");
    assert!(payload.plan.is_some(), "the plan is published");
}
