use solar_controller::decode::{decode, DecodeError, Value};
use solar_controller::map::{BaseWord, Encoding, EnumOptions, SensorDef};

fn def(encoding: Encoding, scale: f64, offset: f64) -> SensorDef {
    SensorDef {
        name: "test".into(),
        address: 0,
        encoding,
        scale,
        offset,
        unit: None,
        device_class: None,
        state_class: None,
        options: None,
    }
}

fn num(v: f64) -> Value {
    Value::Number(v)
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---- canned vectors from the 2026-09-27 mbpoll captures ----
// Pair orders per the re-probe: the original plan.md capture listed
// (534,535) transposed; rated power (12 kW on a 12K) is the independent
// anchor that settles base_word = low.

#[test]
fn battery_voltage_verified_capture() {
    // 0x1545 = 5445, scale 0.01 -> 54.45 V (reg 587)
    let d = def(Encoding::U16, 0.01, 0.0);
    match decode(&d, &[0x1545]).unwrap() {
        Value::Number(v) => assert!(approx(v, 54.45), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn s16_negative_power() {
    // 0xFFF6 sign-extends to -10 W
    let d = def(Encoding::S16, 1.0, 0.0);
    match decode(&d, &[0xFFF6]).unwrap() {
        Value::Number(v) => assert!(approx(v, -10.0), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn u16_is_never_sign_extended() {
    let d = def(Encoding::U16, 1.0, 0.0);
    match decode(&d, &[0xFFFF]).unwrap() {
        Value::Number(v) => assert!(approx(v, 65535.0), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn temperature_offset_applies_in_raw_domain() {
    // (1165 - 1000) * 0.1 = 16.5 °C (reg 586 capture)
    let d = def(Encoding::U16, 0.1, 1000.0);
    match decode(&d, &[1165]).unwrap() {
        Value::Number(v) => assert!(approx(v, 16.5), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn u32_base_low_verified_capture() {
    // (534,535) = (0x022E, 0x0000): base reg holds the LOW word.
    // 0x022E = 558, scale 0.1 -> 55.8 kWh. The anchor for base_word: low.
    let d = def(Encoding::U32 { base_word: BaseWord::Low }, 0.1, 0.0);
    match decode(&d, &[0x022E, 0x0000]).unwrap() {
        Value::Number(v) => assert!(approx(v, 55.8), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn u32_base_low_rated_power_capture() {
    // (20,21) = (0xD4C0, 0x0001): low word at base.
    // 0x0001D4C0 = 120000, scale 0.1 -> 12000.0 W on a 12K inverter -
    // the independent anchor that settles the word order.
    let d = def(Encoding::U32 { base_word: BaseWord::Low }, 0.1, 0.0);
    match decode(&d, &[0xD4C0, 0x0001]).unwrap() {
        Value::Number(v) => assert!(approx(v, 12000.0), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn u32_base_high_assembly_mirror() {
    // Pure assembly check for the High variant - hardware uses low.
    // Same 558 value with the words swapped and base = high.
    let d = def(Encoding::U32 { base_word: BaseWord::High }, 0.1, 0.0);
    match decode(&d, &[0x0000, 0x022E]).unwrap() {
        Value::Number(v) => assert!(approx(v, 55.8), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn s32_sign_extension_crosses_words() {
    // words (0xFFFF, 0xFFF6) base-low -> 0xFFFFFFF6 as i32 = -10
    // (assembly check; no s32 sensor in the current map)
    let d = def(Encoding::S32 { base_word: BaseWord::Low }, 1.0, 0.0);
    match decode(&d, &[0xFFF6, 0xFFFF]).unwrap() {
        Value::Number(v) => assert!(approx(v, -10.0), "got {v}"),
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn options_enum_decodes_to_text() {
    let mut d = def(Encoding::U16, 1.0, 0.0);
    d.options = Some(vec![EnumOptions {
        value: 2,
        label: "normal".into(),
    }]);
    assert_eq!(decode(&d, &[2]).unwrap(), Value::Text("normal".into()));
    assert_eq!(decode(&d, &[7]).unwrap_err(), DecodeError::UnknownOption(7));
}

#[test]
fn serial_number_from_verified_capture() {
    // regs 3-7 hold "2605096069" as 5 words, high byte first
    let d = def(Encoding::Str { words: 5 }, 1.0, 0.0);
    let words = [0x3236, 0x3035, 0x3039, 0x3630, 0x3639];
    assert_eq!(decode(&d, &words).unwrap(), Value::Text("2605096069".into()));
}

#[test]
fn word_count_mismatch_is_an_error() {
    let d = def(Encoding::U16, 1.0, 0.0);
    assert_eq!(
        decode(&d, &[1, 2]).unwrap_err(),
        DecodeError::WordCount { expected: 1, got: 2 }
    );
}
