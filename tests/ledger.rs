//! The ledger end to end (board-aru): frames in, one row per closed MTU out.
//!
//! The actor is fed the same `ServerMsg` frames a websocket gets, so this exercises the
//! subscription's payload shape without a socket: a plan, then a run of telemetry, then a
//! sample in the next MTU that closes the first one.

use std::collections::BTreeMap;

use kameo::actor::Spawn;
use solar_controller::decode::Value;
use solar_controller::ledger::{LedgerActor, Observe, QueryRows, mtu_start};
use solar_controller::planner::{
    Action, Mtu, Plan, PlanStep, Reason,
};
use solar_controller::protocol::ServerMsg;
use solar_controller::store::{Flush, StoreActor, StoreConfig};
use solar_controller::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};

/// 2026-11-09 12:00 Prague — an MTU boundary (900 s alignment holds for CET).
const T0: i64 = 1_794_222_000;

fn one_step_plan(start: i64) -> Plan {
    Plan {
        computed_at: start - 60,
        steps: vec![PlanStep {
            mtu: Mtu {
                start,
                hours: Hours::new(0.25),
            },
            action: Action::Export {
                kw: AcKw::new(8.0),
            },
            battery_out_kwh: DcKwh::new(2.0),
            ac_kwh: AcKwh::new(2.0),
            energy_after_kwh: DcKwh::new(20.0),
            net_czk: Czk::new(15.0),
            marginal_czk_per_kwh: CzkPerKwh::new(5.0),
            reason: Reason::PriceAboveStoredValue {
                price_sell: CzkPerKwh::new(7.9),
                marginal: CzkPerKwh::new(5.0),
            },
            price_sell: CzkPerKwh::new(7.9),
            price_buy: CzkPerKwh::new(8.5),
            pv_kw: DcKw::new(9.0),
        }],
        expected_net_czk: Czk::new(15.0),
        horizon_end: start + 900,
    }
}

fn state_frame(ts: i64, link_ok: bool) -> ServerMsg {
    let mut values = BTreeMap::new();
    values.insert(
        "grid_internal_power_total".to_string(),
        Value::Number(-8_000.0),
    );
    values.insert("battery_soc".to_string(), Value::Number(50.0));
    ServerMsg::State {
        ts: ts as u64,
        link_ok,
        values,
        directives: serde_json::json!({}),
    }
}

#[tokio::test]
async fn a_closed_mtu_becomes_one_row_with_both_halves() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreActor::spawn(StoreActor::new(
        StoreConfig::new(dir.path().join("history")).unwrap(),
    ));
    let ledger = LedgerActor::spawn(LedgerActor::new(store.clone()));

    let _ = ledger
        .tell(Observe(ServerMsg::Plan {
            plan: Some(one_step_plan(T0)),
        }))
        .send()
        .await;

    // One poll every five seconds across the MTU, then the sample that closes it.
    for n in 0..=(900 / 5) {
        let _ = ledger
            .tell(Observe(state_frame(T0 + n * 5, true)))
            .send()
            .await;
    }
    store.ask(Flush).await.unwrap();
    let raw = store
        .ask(solar_controller::store::ReadRange {
            from: T0,
            to: T0 + 1800,
        })
        .await
        .unwrap();
    eprintln!("DEBUG raw lines: {raw:?}");

    let rows = ledger
        .ask(QueryRows {
            from: T0,
            to: T0 + 1800,
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one closed MTU, one row");
    let row = &rows[0];
    assert_eq!(row.start, T0);
    assert_eq!(row.hours, 0.25);
    // −8 kW of grid port, 900 s: 2.0 kWh exported, and the plan said the same.
    assert!((row.actual.ac_kwh - 2.0).abs() < 1e-3, "{}", row.actual.ac_kwh);
    assert_eq!(row.actual.soc_end_pct, Some(50.0));
    assert_eq!(row.actual.samples, 180, "the closing sample belongs to the next MTU");

    let plan = row.plan.as_ref().expect("the MTU opened under a plan");
    assert_eq!(plan.kind, "export");
    assert_eq!(plan.computed_at, T0 - 60);
    assert!((plan.ac_kwh - 2.0).abs() < 1e-9);
}

#[tokio::test]
async fn a_dead_link_is_not_integrated() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreActor::spawn(StoreActor::new(
        StoreConfig::new(dir.path().join("history")).unwrap(),
    ));
    let ledger = LedgerActor::spawn(LedgerActor::new(store.clone()));

    // A frame with link_ok=false carries the last good values; billing them would invent
    // energy that may never have flowed.
    for n in 0..=(900 / 5) {
        let _ = ledger
            .tell(Observe(state_frame(T0 + n * 5, false)))
            .send()
            .await;
    }
    store.ask(Flush).await.unwrap();
    let raw = store
        .ask(solar_controller::store::ReadRange {
            from: T0,
            to: T0 + 1800,
        })
        .await
        .unwrap();
    eprintln!("DEBUG raw lines: {raw:?}");

    let rows = ledger
        .ask(QueryRows {
            from: T0,
            to: T0 + 1800,
        })
        .await
        .unwrap();
    assert!(rows.is_empty(), "nothing was measured, so nothing is billed");
}

/// The row is filed under its own MTU start, not the write time: a row closed after midnight
/// still belongs to the day the MTU opened.
#[test]
fn a_row_start_snaps_to_the_mtu_grid() {
    assert_eq!(mtu_start(T0), T0);
    assert_eq!(mtu_start(T0 + 899), T0);
    assert_eq!(mtu_start(T0 + 900), T0 + 900);
}
