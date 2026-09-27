use solar_controller::map::{self, BaseWord, Encoding, RegisterMap};

fn parse(s: &str) -> Result<RegisterMap, map::MapError> {
    map::load_from_str(s)
}
#[test]
fn embedded_map_loads_and_meets_plan_minimums() {
    let m = map::load_from_str(map::EMBEDDED_MAP).expect("embedded map must parse");
    assert!(m.sensors.len() >= 40, "plan requires >= 40 sensors");
    assert!(m.controls.len() >= 20, "expected a full control set");
}

#[test]
fn embedded_map_verified_anchors() {
    let m = map::load_from_str(map::EMBEDDED_MAP).unwrap();
    let pv_total = m
        .sensors
        .iter()
        .find(|s| s.name == "pv_energy_total")
        .unwrap();
    assert_eq!(pv_total.address, 534);
    assert_eq!(
        pv_total.encoding,
        Encoding::U32 {
            base_word: BaseWord::Low
        }
    );
    let run_state = m.sensors.iter().find(|s| s.name == "run_state").unwrap();
    assert_eq!(
        run_state
            .options
            .as_ref()
            .unwrap()
            .iter()
            .find(|o| o.value == 2)
            .unwrap()
            .label,
        "normal"
    );
}

#[test]
fn stray_field_is_rejected() {
    let err = parse(
        r#"sensors = [ { name = "x", address = 1, encoding = { type = "u16" }, scalle = 0.1 } ]"#,
    );
    assert!(
        err.is_err(),
        "typos must be load failures, not silent defaults"
    );
}

#[test]
fn base_word_on_16bit_is_rejected() {
    let err = parse(
        r#"sensors = [ { name = "x", address = 1, encoding = { type = "u16", base_word = "low" } } ]"#,
    );
    assert!(err.is_err());
}

#[test]
fn duplicate_names_are_rejected() {
    let err = parse(
        r#"sensors = [
        { name = "x", address = 1, encoding = { type = "u16" } },
        { name = "x", address = 2, encoding = { type = "u16" } },
    ]"#,
    );
    assert!(err.is_err());
}

#[test]
fn options_requires_u16() {
    let err = parse(
        r#"sensors = [ { name = "x", address = 1, encoding = { type = "s16" }, options = [ { value = 0, label = "a" } ] } ]"#,
    );
    assert!(err.is_err());
}

#[test]
fn u32_without_base_word_is_rejected() {
    let err = parse(r#"sensors = [ { name = "x", address = 1, encoding = { type = "u32" } } ]"#);
    assert!(err.is_err());
}

#[test]
fn str_without_words_is_rejected() {
    let err = parse(r#"sensors = [ { name = "x", address = 1, encoding = { type = "str" } } ]"#);
    assert!(err.is_err());
}

#[test]
fn str_zero_words_is_rejected() {
    let err =
        parse(r#"sensors = [ { name = "x", address = 1, encoding = { type = "str", words = 0 } } ]"#);
    assert!(err.is_err());
}

#[test]
fn unknown_key_inside_encoding_is_rejected() {
    // deny_unknown_fields must fire inside the encoding table too -
    // serde's tagged-enum handling alone silently drops such keys.
    let err = parse(
        r#"sensors = [ { name = "x", address = 1, encoding = { type = "u16", word_order = "low" } } ]"#,
    );
    assert!(err.is_err());
}

#[test]
fn switch_without_on_is_rejected() {
    let err = parse(
        r#"controls = [ { name = "c", address = 80, encoding = { type = "u16" }, constraint = { type = "switch", off = 0 } } ]"#,
    );
    assert!(err.is_err());
}

#[test]
fn select_with_number_field_is_rejected() {
    let err = parse(
        r#"controls = [ { name = "c", address = 142, encoding = { type = "u16" }, constraint = { type = "select", options = [ { value = 0, label = "a" } ], min = 0 } } ]"#,
    );
    assert!(err.is_err());
}

#[test]
fn duplicate_option_values_are_rejected() {
    let err = parse(
        r#"controls = [ { name = "c", address = 142, encoding = { type = "u16" }, constraint = { type = "select", options = [ { value = 0, label = "a" }, { value = 0, label = "b" } ] } } ]"#,
    );
    assert!(err.is_err());
}
