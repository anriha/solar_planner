use serde_json::json;
use solar_controller::directives::{ApplyError, Directives, Percent, Price, directive_schema, load, save};

#[test]
fn defaults_are_the_phase_3a_tuning() {
    let d = Directives::default();
    assert_eq!(d.version, 0);
    assert!(d.planner_enabled);
    assert!(
        !d.actuator_enabled,
        "the planner must be able to run without owning the registers"
    );
    assert!(!d.disarm, "a fresh deploy is armed by the switch, not disarmed");
    assert_eq!(d.soc_floor, Percent::try_from(20.0).unwrap());
    assert_eq!(d.soc_ceiling, Percent::try_from(80.0).unwrap());
    assert_eq!(d.max_power_kw, 12.0);
    assert_eq!(d.buy_distribution_czk, Price::try_from(2.515).unwrap());
    assert_eq!(d.buy_seller_fee_czk, Price::try_from(0.3).unwrap());
    assert_eq!(d.sell_fee_czk, Price::try_from(0.5).unwrap());
    assert_eq!(d.forecast_calibration, 1.0);
    assert_eq!(d.energy_step_kwh, 0.32);
    assert_eq!(d.action_step_kw, 1.2);
}

#[test]
fn apply_each_directive_and_bump_version() {
    let mut d = Directives::default();
    assert!(d.apply("planner_enabled", json!(false)).unwrap());
    assert!(d.apply("actuator_enabled", json!(true)).unwrap());
    assert!(d.apply("disarm", json!(true)).unwrap());
    assert!(d.apply("soc_floor", json!(25)).unwrap());
    assert!(d.apply("soc_ceiling", json!(75)).unwrap());
    assert!(d.apply("max_power_kw", json!(10.0)).unwrap());
    assert!(d.apply("buy_distribution_czk", json!(2.4)).unwrap());
    assert!(d.apply("buy_seller_fee_czk", json!(0.35)).unwrap());
    assert!(d.apply("sell_fee_czk", json!(0.75)).unwrap());
    assert!(d.apply("forecast_calibration", json!(0.45)).unwrap());
    assert!(d.apply("energy_step_kwh", json!(0.4)).unwrap());
    assert!(d.apply("action_step_kw", json!(1.0)).unwrap());

    assert!(!d.planner_enabled);
    assert!(d.actuator_enabled);
    assert!(d.disarm);
    assert_eq!(d.soc_floor.get(), 25.0);
    assert_eq!(d.soc_ceiling.get(), 75.0);
    assert_eq!(d.max_power_kw, 10.0);
    assert_eq!(d.buy_distribution_czk.get(), 2.4);
    assert_eq!(d.buy_seller_fee_czk.get(), 0.35);
    assert_eq!(d.sell_fee_czk.get(), 0.75);
    assert_eq!(d.forecast_calibration, 0.45);
    assert_eq!(d.energy_step_kwh, 0.4);
    assert_eq!(d.action_step_kw, 1.0);
    assert_eq!(d.version, 12);
}

#[test]
fn the_soc_bounds_cannot_cross() {
    let mut d = Directives::default(); // 20 / 80

    assert!(
        matches!(
            d.apply("soc_floor", json!(85)),
            Err(ApplyError::Invalid { .. })
        ),
        "a floor above the ceiling must be rejected, not clamped"
    );
    assert!(
        matches!(
            d.apply("soc_ceiling", json!(15)),
            Err(ApplyError::Invalid { .. })
        ),
        "a ceiling below the floor must be rejected"
    );
    assert_eq!(d.soc_floor.get(), 20.0, "a rejected apply changes nothing");
    assert_eq!(d.soc_ceiling.get(), 80.0);
    assert_eq!(d.version, 0);
}

#[test]
fn no_op_apply_changes_nothing() {
    let mut d = Directives::default(); // planner_enabled is already true
    assert!(!d.apply("planner_enabled", json!(true)).unwrap());
    assert!(!d.apply("soc_floor", json!(20)).unwrap());
    assert_eq!(d.version, 0);
}

#[test]
fn unknown_directive_rejected() {
    let mut d = Directives::default();
    assert!(matches!(
        d.apply("grid_feed_in", json!(1)),
        Err(ApplyError::Unknown(_))
    ));
    // Deleted 2026-11-06; the enum directive must not come back through the API.
    assert!(matches!(
        d.apply("force_mode", json!("hold")),
        Err(ApplyError::Unknown(_))
    ));
}

#[test]
fn type_and_range_errors() {
    let mut d = Directives::default();
    assert!(matches!(
        d.apply("planner_enabled", json!("yes")),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("actuator_enabled", json!(1)),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("disarm", json!("yes")),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("soc_floor", json!(100.5)),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("soc_floor", json!(-1)),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("soc_floor", json!("80")),
        Err(ApplyError::Invalid { .. })
    ));
}

#[test]
fn numbers_outside_their_range_are_rejected_not_clamped() {
    let mut d = Directives::default();
    for (name, value) in [
        ("max_power_kw", json!(0.1)),
        ("max_power_kw", json!(31.0)),
        ("buy_distribution_czk", json!(10_001.0)),
        ("buy_distribution_czk", json!(-0.5)),
        ("buy_seller_fee_czk", json!(10_001.0)),
        ("buy_seller_fee_czk", json!(-0.5)),
        ("sell_fee_czk", json!(-0.5)),
        ("forecast_calibration", json!(0.05)),
        ("forecast_calibration", json!(4.0)),
        ("energy_step_kwh", json!(0.01)),
        ("action_step_kw", json!(0.05)),
    ] {
        assert!(
            matches!(d.apply(name, value.clone()), Err(ApplyError::Invalid { .. })),
            "{name} accepted {value}"
        );
    }
    assert_eq!(d, Directives::default(), "no rejected apply left a trace");
}

#[test]
fn json_roundtrip() {
    let mut d = Directives::default();
    d.apply("disarm", json!(true)).unwrap();
    d.apply("soc_floor", json!(35)).unwrap();
    let back: Directives = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert_eq!(d, back);
}

#[test]
fn persistence_roundtrip_and_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    assert!(load(&path).unwrap().is_none());

    let mut d = Directives::default();
    assert!(d.apply("disarm", json!(true)).unwrap());
    save(&path, &d).unwrap();
    assert_eq!(load(&path).unwrap(), Some(d));
    // atomic write leaves no residue
    assert!(!dir.path().join("directives.json.tmp").exists());
}

#[test]
fn a_file_written_before_the_switch_existed_loads_disabled() {
    // The exact shape the pre-switch binary persisted: ten fields, no actuator_enabled and
    // a `force_mode` that has since been deleted. Without the defaults this is a startup
    // failure; with the migration the switch must come up OFF and the retired key dropped.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    std::fs::write(
        &path,
        r#"{"version":3,"planner_enabled":true,"force_mode":"auto",
            "soc_floor":25.0,"soc_ceiling":75.0,"max_power_kw":10.0,
            "buy_price_czk":4.5,"sell_fee_czk":0.75,"forecast_calibration":1.0,
            "energy_step_kwh":0.32,"action_step_kw":1.2}"#,
    )
    .unwrap();

    let loaded = load(&path).unwrap().expect("a legacy file still loads");
    assert_eq!(loaded.version, 3);
    assert_eq!(loaded.soc_floor.get(), 25.0);
    assert!(!loaded.actuator_enabled, "an upgraded deploy stays read-only");
    assert!(!loaded.disarm);
}

#[test]
fn the_retired_flat_tariff_is_dropped_and_the_spot_charges_default() {
    // The Pi's file on 2026-11-06 held a hand-set `buy_price_czk` 7.649 against a flat
    // tariff that turned out to be the wrong model. That key is gone; the file must load,
    // the two buy charges must appear at their settled defaults, and the old number must
    // not survive as a price anywhere.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    std::fs::write(
        &path,
        r#"{"version":4,"planner_enabled":true,"actuator_enabled":true,"disarm":false,
            "soc_floor":30.0,"soc_ceiling":70.0,"max_power_kw":9.0,
            "buy_price_czk":7.649,"sell_fee_czk":0.75,"forecast_calibration":1.1,
            "energy_step_kwh":0.32,"action_step_kw":1.2}"#,
    )
    .unwrap();

    let loaded = load(&path).unwrap().expect("a pre-spot file still loads");
    assert_eq!(loaded.buy_distribution_czk.get(), 2.515);
    assert_eq!(loaded.buy_seller_fee_czk.get(), 0.3);
    assert_eq!(loaded.sell_fee_czk.get(), 0.75, "the sell fee is its own field");
    assert_eq!(loaded.version, 4, "a migrated load is not an edit");
    assert!(loaded.actuator_enabled, "the migration preserves the rest");

    // The next save writes the new shape and no retired key.
    save(&path, &loaded).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("buy_price_czk"), "the retired key is not re-written");
    assert!(raw.contains("buy_distribution_czk"));
}

#[test]
fn other_unknown_fields_are_still_a_loud_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    std::fs::write(
        &path,
        r#"{"version":3,"planner_enabled":true,"soc_floor":25.0,"soc_ceiling":75.0,
            "max_power_kw":10.0,"buy_price_czk":4.5,"sell_fee_czk":0.75,
            "forecast_calibration":1.0,"energy_step_kwh":0.32,"action_step_kw":1.2,
            "typo_field":true}"#,
    )
    .unwrap();
    assert!(load(&path).is_err(), "only force_mode and buy_price_czk are migrated");
}

#[test]
fn corrupt_file_is_a_loud_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    std::fs::write(&path, "not json").unwrap();
    assert!(load(&path).is_err());
}

#[test]
fn directive_schema_matches_frozen_contract() {
    // serde emits internally-tagged variants with alphabetically sorted keys
    // (content buffering), so compare parsed JSON, not bytes. The first twelve entries are
    // the policy scalars; the rest are the four curves in CURVE_NAMES order, five points ×
    // (power, efficiency) each.
    let actual: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&directive_schema()).unwrap()).unwrap();
    let expected: serde_json::Value = serde_json::from_str(
        r#"[
            {"name":"planner_enabled","type":"bool"},
            {"name":"actuator_enabled","type":"bool"},
            {"name":"disarm","type":"bool"},
            {"max":100.0,"min":0.0,"name":"soc_floor","nullable":false,"type":"number","unit":"%"},
            {"max":100.0,"min":0.0,"name":"soc_ceiling","nullable":false,"type":"number","unit":"%"},
            {"max":30.0,"min":0.5,"name":"max_power_kw","nullable":false,"type":"number","unit":"kW"},
            {"max":10000.0,"min":0.0,"name":"buy_distribution_czk","nullable":false,"step":0.001,"type":"number","unit":"CZK/kWh"},
            {"max":10000.0,"min":0.0,"name":"buy_seller_fee_czk","nullable":false,"step":0.001,"type":"number","unit":"CZK/kWh"},
            {"max":10000.0,"min":0.0,"name":"sell_fee_czk","nullable":false,"step":0.001,"type":"number","unit":"CZK/kWh"},
            {"max":3.0,"min":0.1,"name":"forecast_calibration","nullable":false,"type":"number"},
            {"max":5.0,"min":0.05,"name":"energy_step_kwh","nullable":false,"type":"number","unit":"kWh"},
            {"max":5.0,"min":0.1,"name":"action_step_kw","nullable":false,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→battery 1 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_battery_p1_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→battery 1 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_battery_p1_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→battery 2 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_battery_p2_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→battery 2 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_battery_p2_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→battery 3 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_battery_p3_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→battery 3 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_battery_p3_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→battery 4 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_battery_p4_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→battery 4 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_battery_p4_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→battery 5 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_battery_p5_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→battery 5 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_battery_p5_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→grid 1 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_grid_p1_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→grid 1 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_grid_p1_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→grid 2 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_grid_p2_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→grid 2 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_grid_p2_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→grid 3 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_grid_p3_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→grid 3 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_grid_p3_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→grid 4 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_grid_p4_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→grid 4 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_grid_p4_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"PV→grid 5 power","max":30.0,"min":0.0,"mode":"box","name":"pv_to_grid_p5_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"PV→grid 5 efficiency","max":100.0,"min":0.0,"mode":"box","name":"pv_to_grid_p5_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"battery→grid 1 power","max":30.0,"min":0.0,"mode":"box","name":"battery_to_grid_p1_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"battery→grid 1 efficiency","max":100.0,"min":0.0,"mode":"box","name":"battery_to_grid_p1_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"battery→grid 2 power","max":30.0,"min":0.0,"mode":"box","name":"battery_to_grid_p2_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"battery→grid 2 efficiency","max":100.0,"min":0.0,"mode":"box","name":"battery_to_grid_p2_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"battery→grid 3 power","max":30.0,"min":0.0,"mode":"box","name":"battery_to_grid_p3_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"battery→grid 3 efficiency","max":100.0,"min":0.0,"mode":"box","name":"battery_to_grid_p3_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"battery→grid 4 power","max":30.0,"min":0.0,"mode":"box","name":"battery_to_grid_p4_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"battery→grid 4 efficiency","max":100.0,"min":0.0,"mode":"box","name":"battery_to_grid_p4_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"battery→grid 5 power","max":30.0,"min":0.0,"mode":"box","name":"battery_to_grid_p5_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"battery→grid 5 efficiency","max":100.0,"min":0.0,"mode":"box","name":"battery_to_grid_p5_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"grid→battery 1 power","max":30.0,"min":0.0,"mode":"box","name":"grid_to_battery_p1_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"grid→battery 1 efficiency","max":100.0,"min":0.0,"mode":"box","name":"grid_to_battery_p1_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"grid→battery 2 power","max":30.0,"min":0.0,"mode":"box","name":"grid_to_battery_p2_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"grid→battery 2 efficiency","max":100.0,"min":0.0,"mode":"box","name":"grid_to_battery_p2_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"grid→battery 3 power","max":30.0,"min":0.0,"mode":"box","name":"grid_to_battery_p3_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"grid→battery 3 efficiency","max":100.0,"min":0.0,"mode":"box","name":"grid_to_battery_p3_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"grid→battery 4 power","max":30.0,"min":0.0,"mode":"box","name":"grid_to_battery_p4_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"grid→battery 4 efficiency","max":100.0,"min":0.0,"mode":"box","name":"grid_to_battery_p4_pct","nullable":false,"step":0.1,"type":"number","unit":"%"},
            {"category":"config","group":"efficiency","label":"grid→battery 5 power","max":30.0,"min":0.0,"mode":"box","name":"grid_to_battery_p5_kw","nullable":false,"step":0.01,"type":"number","unit":"kW"},
            {"category":"config","group":"efficiency","label":"grid→battery 5 efficiency","max":100.0,"min":0.0,"mode":"box","name":"grid_to_battery_p5_pct","nullable":false,"step":0.1,"type":"number","unit":"%"}
        ]"#,
    )
    .unwrap();
    assert_eq!(actual, expected);
}

/// Float compare for curve coordinates: `pct / 100.0` is not bit-identical to a decimal
/// literal, and these tests are about the measured numbers, not about ulps.
fn assert_curve(actual: Vec<(f64, f64)>, expected: &[(f64, f64)]) {
    assert_eq!(actual.len(), expected.len(), "point count");
    for (index, ((kw, eta), (want_kw, want_eta))) in actual.iter().zip(expected).enumerate() {
        assert!(
            (kw - want_kw).abs() < 1e-12 && (eta - want_eta).abs() < 1e-12,
            "point {index}: got ({kw}, {eta}), want ({want_kw}, {want_eta})"
        );
    }
}

#[test]
fn defaults_are_the_measured_path_curves() {
    let d = Directives::default();
    assert_curve(
        d.pv_to_battery.fractions(),
        &[
            (0.5, 0.87),
            (1.5, 0.87),
            (3.0, 0.928),
            (6.0, 0.943),
            (10.0, 0.947),
        ],
    );
    assert_curve(d.pv_to_grid.fractions(), &[(2.0, 0.97), (10.0, 0.976)]);
    assert_curve(
        d.battery_to_grid.fractions(),
        &[(0.5, 0.939), (3.0, 0.939), (6.0, 0.96), (10.24, 0.969)],
    );
    assert_curve(d.grid_to_battery.fractions(), &[(1.0, 0.95), (9.472, 0.95)]);
}

#[test]
fn a_curve_point_is_edited_in_place() {
    let mut d = Directives::default();
    let version = d.version;
    assert!(d.apply("pv_to_battery_p3_pct", json!(93.5)).unwrap());
    assert_eq!(d.pv_to_battery.points[2].pct, 93.5);
    assert_eq!(d.pv_to_battery.points[2].kw, 3.0, "power untouched");
    assert_eq!(d.pv_to_battery.points[0].pct, 87.0, "other points untouched");
    assert_eq!(d.version, version + 1);
}

#[test]
fn curve_points_must_ascend_by_power() {
    let mut d = Directives::default();
    let error = d
        .apply("pv_to_battery_p2_kw", json!(0.4))
        .expect_err("0.4 kW cannot sit above the 0.5 kW point");
    assert!(
        error.to_string().contains("must be above slot 1"),
        "{error}"
    );
    assert_eq!(
        d.pv_to_battery.points[1].kw, 1.5,
        "a rejected edit changes nothing"
    );
}

#[test]
fn a_zero_efficiency_disables_a_slot() {
    let mut d = Directives::default();
    assert!(d.apply("pv_to_grid_p2_pct", json!(0.0)).unwrap());
    assert_eq!(d.pv_to_grid.active().count(), 1);
    assert_curve(d.pv_to_grid.fractions(), &[(2.0, 0.97)]);

    // The last active point cannot be switched off: a path without points is not a curve.
    assert!(d.apply("pv_to_grid_p1_pct", json!(0.0)).is_err());
}

#[test]
fn curve_efficiency_outside_fifty_to_hundred_is_rejected() {
    let mut d = Directives::default();
    for bad in [0.943, 20.0, 100.1, -1.0] {
        assert!(
            d.apply("pv_to_battery_p1_pct", json!(bad)).is_err(),
            "{bad} should be rejected"
        );
    }
    assert_eq!(
        d.pv_to_battery.points[0].pct, 87.0,
        "nothing was clamped in"
    );
}

#[test]
fn a_no_op_curve_edit_does_not_bump_the_version() {
    let mut d = Directives::default();
    let version = d.version;
    assert!(!d.apply("pv_to_battery_p1_kw", json!(0.5)).unwrap());
    assert_eq!(d.version, version);
}

#[test]
fn a_file_without_curves_gains_the_measured_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    std::fs::write(
        &path,
        r#"{"version":7,"planner_enabled":true,"actuator_enabled":false,"disarm":false,
            "soc_floor":20.0,"soc_ceiling":80.0,"max_power_kw":12.0,
            "buy_distribution_czk":2.515,"buy_seller_fee_czk":0.3,"sell_fee_czk":0.5,
            "forecast_calibration":1.0,"energy_step_kwh":0.32,"action_step_kw":1.2}"#,
    )
    .unwrap();
    let loaded = load(&path).unwrap().expect("file exists");
    assert_eq!(loaded.version, 7);
    assert_curve(
        loaded.pv_to_battery.fractions(),
        &[
            (0.5, 0.87),
            (1.5, 0.87),
            (3.0, 0.928),
            (6.0, 0.943),
            (10.0, 0.947),
        ],
    );
    assert_curve(
        loaded.grid_to_battery.fractions(),
        &[(1.0, 0.95), (9.472, 0.95)],
    );
}

#[test]
fn every_wire_key_has_a_schema_entry() {
    // The HA number entity reads `directives[name]`: a wire key with no schema entry is an
    // invisible value, and a schema entry with no wire key is a dead entity.
    let wire = Directives::default().wire_json();
    let wire_names: std::collections::BTreeSet<String> = wire
        .as_object()
        .unwrap()
        .keys()
        .filter(|name| *name != "version")
        .cloned()
        .collect();
    let schema_names: std::collections::BTreeSet<String> = directive_schema()
        .as_array()
        .unwrap()
        .iter()
        .map(|spec| spec["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(wire_names, schema_names);
    assert!(
        !wire.as_object().unwrap().contains_key("pv_to_battery"),
        "the curve is flattened on the wire, never nested"
    );
}

