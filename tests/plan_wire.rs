//! The wire shape of `Plan` as the HA integration reads it (`sensor.py`, `PlanSensor`).
//!
//! `custom_components/solar_bridge/sensor.py` parses `{"kind": ..., "kw": ...}` out of each
//! step's `action` and formats `mtu.start` as a clock. Renaming a serde attribute here would
//! silently blank those entities, so the contract is pinned rather than assumed.

use solar_controller::planner::{Action, Mtu, Plan, PlanStep, Reason};
use solar_controller::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKwh, Hours};

#[test]
fn a_step_serialises_the_way_the_ha_entities_expect() {
    let step = PlanStep {
        mtu: Mtu {
            start: 1_793_746_800,
            hours: Hours::new(0.25),
        },
        action: Action::Export {
            kw: AcKw::new(6.0),
        },
        battery_out_kwh: DcKwh::new(1.5),
        ac_kwh: AcKwh::new(2.7),
        energy_after_kwh: DcKwh::new(21.4),
        net_czk: Czk::new(43.2),
        marginal_czk_per_kwh: CzkPerKwh::new(5.32),
        reason: Reason::PriceAboveStoredValue {
            price_sell: CzkPerKwh::new(7.9),
            marginal: CzkPerKwh::new(5.32),
        },
    };
    let plan = Plan {
        computed_at: 1_793_746_700,
        steps: vec![step],
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
