use serde_json::json;
use solar_controller::{
    bus::{WriteError, encode_write},
    decode::{Field, decode_field},
    map::{Constraint, ControlDef, Encoding, EMBEDDED_MAP, load_from_str},
};

fn def(name: &str, encoding: Encoding, scale: f64, offset: f64, constraint: Constraint) -> ControlDef {
    ControlDef {
        name: name.into(),
        address: 100,
        encoding,
        scale,
        offset,
        unit: None,
        writable: true,
        constraint,
    }
}

#[test]
fn switch_maps_bool_to_on_off_raws() {
    let d = def(
        "sw",
        Encoding::U16,
        1.0,
        0.0,
        Constraint::Switch { on: 1, off: 0 },
    );
    assert_eq!(encode_write(&d, &json!(true)).unwrap(), 1);
    assert_eq!(encode_write(&d, &json!(false)).unwrap(), 0);
    assert!(matches!(
        encode_write(&d, &json!("on")),
        Err(WriteError::Invalid { ref reason, .. }) if reason.contains("true/false")
    ));
}

#[test]
fn select_takes_option_value_not_label() {
    let d = def(
        "sel",
        Encoding::U16,
        1.0,
        0.0,
        Constraint::Select {
            options: vec![
                solar_controller::map::EnumOptions { value: 0, label: "generator".into() },
                solar_controller::map::EnumOptions { value: 2, label: "micro_inverter".into() },
            ],
        },
    );
    assert_eq!(encode_write(&d, &json!(2)).unwrap(), 2);
    // 1b: wire carries the u16 value — labels and non-member values reject.
    assert!(encode_write(&d, &json!("micro_inverter")).is_err());
    assert!(encode_write(&d, &json!(1)).is_err());
    assert!(encode_write(&d, &json!(2.0)).is_err());
}

#[test]
fn number_rejects_out_of_range_never_clamps() {
    let d = def(
        "p",
        Encoding::U16,
        1.0,
        0.0,
        Constraint::Number { min: 0.0, max: 16000.0, step: Some(100.0) },
    );
    assert_eq!(encode_write(&d, &json!(0)).unwrap(), 0);
    assert_eq!(encode_write(&d, &json!(16000)).unwrap(), 16000);
    assert!(matches!(
        encode_write(&d, &json!(16001)),
        Err(WriteError::Invalid { ref reason, .. }) if reason.contains("rejected, never clamped")
    ));
    assert!(encode_write(&d, &json!(-1)).is_err());
}

#[test]
fn number_rejects_non_representable_values() {
    let d = def(
        "fine",
        Encoding::U16,
        0.1,
        0.0,
        Constraint::Number { min: 0.0, max: 100.0, step: None },
    );
    assert_eq!(encode_write(&d, &json!(55.5)).unwrap(), 555);
    assert!(matches!(
        encode_write(&d, &json!(55.55)),
        Err(WriteError::Invalid { ref reason, .. }) if reason.contains("not representable")
    ));
}

#[test]
fn offset_scale_roundtrip_mirrors_decode_anchor() {
    // decode anchor: raw 1165, offset 1000, scale 0.1 -> 16.5
    let d = def(
        "temp",
        Encoding::U16,
        0.1,
        1000.0,
        Constraint::Number { min: 0.0, max: 100.0, step: None },
    );
    let raw = encode_write(&d, &json!(16.5)).unwrap();
    assert_eq!(raw, 1165);
    assert_eq!(
        decode_field(&Field::from(&d), &[raw]).unwrap(),
        solar_controller::decode::Value::Number(16.5)
    );
}

#[test]
fn s16_negatives_encode_twos_complement() {
    let d = def(
        "ze",
        Encoding::S16,
        1.0,
        0.0,
        Constraint::Number { min: -500.0, max: 500.0, step: None },
    );
    assert_eq!(encode_write(&d, &json!(-300)).unwrap(), 65236); // 0xFEA4
    assert_eq!(encode_write(&d, &json!(300)).unwrap(), 300);
    // in-constraint value whose raw exceeds s16 rejects
    let wide = def(
        "wide",
        Encoding::S16,
        1.0,
        0.0,
        Constraint::Number { min: 0.0, max: 60000.0, step: None },
    );
    assert!(matches!(
        encode_write(&wide, &json!(40000)),
        Err(WriteError::Invalid { ref reason, .. }) if reason.contains("outside")
    ));
}

#[test]
fn non_writable_and_multiword_rejected() {
    let mut d = def(
        "locked",
        Encoding::U16,
        1.0,
        0.0,
        Constraint::Number { min: 0.0, max: 100.0, step: None },
    );
    d.writable = false;
    assert!(matches!(
        encode_write(&d, &json!(5)),
        Err(WriteError::NotWritable { .. })
    ));

    let u32d = def(
        "dbl",
        Encoding::U32 { base_word: solar_controller::map::BaseWord::Low },
        1.0,
        0.0,
        Constraint::Number { min: 0.0, max: 100.0, step: None },
    );
    assert!(matches!(
        encode_write(&u32d, &json!(5)),
        Err(WriteError::Invalid { ref reason, .. }) if reason.contains("u16/s16")
    ));
}

#[test]
fn real_map_numbers_and_options_roundtrip() {
    let map = load_from_str(EMBEDDED_MAP).unwrap();
    for c in &map.controls {
        match &c.constraint {
            Constraint::Number { min, max, .. } => {
                for v in [*min, *max] {
                    let raw = encode_write(c, &json!(v))
                        .unwrap_or_else(|e| panic!("{}: {e}", c.name));
                    match decode_field(&Field::from(c), &[raw]).unwrap() {
                        solar_controller::decode::Value::Number(back) => {
                            assert!((back - v).abs() < 1e-9, "{}: {v} -> {raw} -> {back}", c.name);
                        }
                        other => panic!("{}: decoded to {other:?}", c.name),
                    }
                }
            }
            Constraint::Select { options } => {
                for o in options {
                    let raw = encode_write(c, &json!(o.value))
                        .unwrap_or_else(|e| panic!("{}: {e}", c.name));
                    assert_eq!(raw, o.value);
                    assert_eq!(
                        decode_field(&Field::from(c), &[raw]).unwrap(),
                        solar_controller::decode::Value::Text(o.label.clone())
                    );
                }
            }
            Constraint::Switch { on, off } => {
                assert_eq!(encode_write(c, &json!(true)).unwrap(), *on);
                assert_eq!(encode_write(c, &json!(false)).unwrap(), *off);
            }
        }
    }
}