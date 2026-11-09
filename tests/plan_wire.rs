//! The wire shape of `Plan` as the HA integration and the browser view read it.
//!
//! `custom_components/solar_bridge/sensor.py` parses `{"kind": ..., "kw": ...}` out of each
//! step's `action` and formats `mtu.start` as a clock. Renaming a serde attribute here would
//! silently blank those entities, so the contract is pinned rather than assumed.
//!
//! Since 2026-11-09 a step also carries the three exogenous curves the DP priced the MTU
//! with — `price_sell`, `price_buy`, `pv_kw`. The plan is then self-describing: the view
//! payload (board-zpe) and any chart can be projected from one frame instead of joining it
//! against a second copy of the horizon.

use solar_controller::planner::{Action, Mtu, Plan, PlanStep, Reason};
use solar_controller::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};

/// 2026-11-04T00:00 Prague.
const T0: i64 = 1_793_746_800;

fn step(start: i64, action: Action) -> PlanStep {
    PlanStep {
        mtu: Mtu {
            start,
            hours: Hours::new(0.25),
        },
        price_sell: CzkPerKwh::new(7.9),
        price_buy: CzkPerKwh::new(3.24),
        pv_kw: DcKw::new(9.5),
        action,
        battery_out_kwh: DcKwh::new(1.5),
        ac_kwh: AcKwh::new(2.7),
        energy_after_kwh: DcKwh::new(21.4),
        net_czk: Czk::new(43.2),
        marginal_czk_per_kwh: CzkPerKwh::new(5.32),
        reason: Reason::PriceAboveStoredValue {
            price_sell: CzkPerKwh::new(7.9),
            marginal: CzkPerKwh::new(5.32),
        },
    }
}

#[test]
fn a_step_serialises_the_way_the_ha_entities_expect() {
    let plan = Plan {
        computed_at: 1_793_746_700,
        steps: vec![step(T0, Action::Export { kw: AcKw::new(6.0) })],
        expected_net_czk: Czk::new(43.2),
        horizon_end: 1_793_750_000,
    };

    let json = serde_json::to_value(&plan).expect("plans always serialise");
    let first = &json["steps"][0];

    // `Action` is internally tagged: the HA sensor matches on `kind` and reads `kw` beside
    // it, not nested under the variant name.
    assert_eq!(first["action"]["kind"], "export");
    assert_eq!(first["action"]["kw"], 6.0);

    // The unit newtypes serialise transparently, so the numbers arrive as numbers.
    assert_eq!(first["mtu"]["start"], 1_793_746_800i64);
    assert_eq!(first["mtu"]["hours"], 0.25);
    assert_eq!(first["ac_kwh"], 2.7);
    assert_eq!(first["energy_after_kwh"], 21.4);
    assert_eq!(first["net_czk"], 43.2);
    assert_eq!(first["marginal_czk_per_kwh"], 5.32);

    // `Reason` is tagged the same way; the sensor shows only the tag.
    assert_eq!(first["reason"]["kind"], "price_above_stored_value");

    // The three plan-level scalars the entities read directly.
    assert_eq!(json["expected_net_czk"], 43.2);
    assert_eq!(json["horizon_end"], 1_793_750_000i64);
    assert_eq!(json["computed_at"], 1_793_746_700i64);
}

/// The curves the DP decided against, on the same step as the decision. Flat numbers, not
/// nested objects: a chart reads `steps[i].price_buy` and nothing else.
#[test]
fn a_step_carries_the_prices_and_the_pv_it_was_decided_against() {
    let plan = Plan {
        computed_at: 1_793_746_700,
        steps: vec![step(T0, Action::Import { kw: AcKw::new(9.6) })],
        expected_net_czk: Czk::new(-43.2),
        horizon_end: 1_793_750_000,
    };
    let json = serde_json::to_value(&plan).expect("plans always serialise");
    let first = &json["steps"][0];

    assert_eq!(first["price_sell"], 7.9);
    assert_eq!(first["price_buy"], 3.24);
    assert_eq!(first["pv_kw"], 9.5);
}

/// The 60 s plan frame carried ~78 KB for a 35 h horizon before the three curves; the
/// ceiling the board set for the extended frame is 90 KB at 140 MTUs. Pinned because the
/// number is only obvious once it is measured, and the worst case is not the fields a
/// planner author writes by hand but the 17-digit floats the DP actually produces
/// (`ac_kwh` from a division, `net_czk` from a sum). So this frame is built with those.
#[test]
fn a_140_mtu_frame_stays_under_90_kb() {
    let steps: Vec<PlanStep> = (0..140)
        .map(|i| {
            let action = match i % 3 {
                0 => Action::Export { kw: AcKw::new(6.0) },
                1 => Action::Import { kw: AcKw::new(9.6) },
                _ => Action::Idle,
            };
            PlanStep {
                mtu: Mtu {
                    start: T0 + i as i64 * 900,
                    hours: Hours::new(0.25),
                },
                price_sell: CzkPerKwh::new(7.912345678901234),
                price_buy: CzkPerKwh::new(3.239876543210987),
                pv_kw: DcKw::new(9.512345678901234),
                action,
                battery_out_kwh: DcKwh::new(1.512345678901234),
                ac_kwh: AcKwh::new(-2.712345678901234),
                energy_after_kwh: DcKwh::new(21.412345678901234),
                net_czk: Czk::new(-43.21234567890123),
                marginal_czk_per_kwh: CzkPerKwh::new(5.321098765432109),
                reason: Reason::HoldForLater {
                    marginal: CzkPerKwh::new(5.321098765432109),
                    best_later_price: CzkPerKwh::new(6.212345678901234),
                },
            }
        })
        .collect();
    let plan = Plan {
        computed_at: 1_793_746_700,
        steps,
        expected_net_czk: Czk::new(120.0),
        horizon_end: T0 + 140 * 900,
    };

    let len = serde_json::to_string(&plan)
        .expect("plans always serialise")
        .len();
    assert!(len < 90_000, "plan frame grew to {len} bytes");
}
