use serde_json::json;
use solar_controller::directives::{
    ApplyError, Directives, ForceMode, Percent, Price, load, save,
};

#[test]
fn defaults_are_neutral() {
    let d = Directives::default();
    assert_eq!(d.version, 0);
    assert!(d.planner_enabled);
    assert_eq!(d.force_mode, ForceMode::Auto);
    assert_eq!(d.soc_target_hold, None);
    assert_eq!(d.price_override, None);
}

#[test]
fn apply_each_directive_and_bump_version() {
    let mut d = Directives::default();
    assert!(d.apply("planner_enabled", json!(false)).unwrap());
    assert!(d.apply("force_mode", json!("charge")).unwrap());
    assert!(d.apply("soc_target_hold", json!(50)).unwrap());
    assert!(d.apply("price_override", json!(12.5)).unwrap());
    assert!(!d.planner_enabled);
    assert_eq!(d.force_mode, ForceMode::Charge);
    assert_eq!(d.soc_target_hold, Some(Percent::try_from(50.0).unwrap()));
    assert_eq!(d.price_override, Some(Price::try_from(12.5).unwrap()));
    assert_eq!(d.version, 4);
}

#[test]
fn null_clears_optionals() {
    let mut d = Directives::default();
    assert!(d.apply("soc_target_hold", json!(30)).unwrap());
    assert!(d.apply("price_override", json!(1.0)).unwrap());
    assert!(d.apply("soc_target_hold", json!(null)).unwrap());
    assert!(d.apply("price_override", json!(null)).unwrap());
    assert_eq!(d.soc_target_hold, None);
    assert_eq!(d.price_override, None);
}

#[test]
fn no_op_apply_changes_nothing() {
    let mut d = Directives::default(); // planner_enabled is already true
    assert!(!d.apply("planner_enabled", json!(true)).unwrap());
    assert_eq!(d.version, 0);
}

#[test]
fn unknown_directive_rejected() {
    let mut d = Directives::default();
    assert!(matches!(
        d.apply("grid_feed_in", json!(1)),
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
        d.apply("force_mode", json!("turbo")),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("force_mode", json!(3)),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("soc_target_hold", json!(100.5)),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("soc_target_hold", json!(-1)),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("soc_target_hold", json!("80")),
        Err(ApplyError::Invalid { .. })
    ));
    assert!(matches!(
        d.apply("price_override", json!(20_000)),
        Err(ApplyError::Invalid { .. })
    ));
}

#[test]
fn negative_spot_price_is_valid() {
    let mut d = Directives::default();
    assert!(d.apply("price_override", json!(-0.5)).unwrap());
    assert_eq!(d.price_override, Some(Price::try_from(-0.5).unwrap()));
}

#[test]
fn json_roundtrip() {
    let mut d = Directives::default();
    d.apply("force_mode", json!("hold")).unwrap();
    d.apply("soc_target_hold", json!(45)).unwrap();
    let back: Directives = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert_eq!(d, back);
}

#[test]
fn persistence_roundtrip_and_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    assert!(load(&path).unwrap().is_none());

    let mut d = Directives::default();
    assert!(d.apply("force_mode", json!("hold")).unwrap());
    save(&path, &d).unwrap();
    assert_eq!(load(&path).unwrap(), Some(d));
    // atomic write leaves no residue
    assert!(!dir.path().join("directives.json.tmp").exists());
}

#[test]
fn corrupt_file_is_a_loud_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directives.json");
    std::fs::write(&path, "not json").unwrap();
    assert!(load(&path).is_err());
}
