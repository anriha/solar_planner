use std::collections::BTreeMap;

use solar_controller::{
    bus::decode_all,
    decode::Value,
    map::{Constraint, EMBEDDED_MAP, load_from_str},
    spans::{MERGE_GAP, ReadSpan, plan_spans},
};

fn synthetic_image(map: &solar_controller::map::RegisterMap, spans: &[ReadSpan]) -> BTreeMap<u16, u16> {
    // Zeros everywhere, except option sensors get their first enum value —
    // raw 0 is not a valid option index for run_state et al.
    let mut image = BTreeMap::new();
    for span in spans {
        for addr in span.start..span.start + span.len {
            image.insert(addr, 0u16);
        }
    }
    for sensor in &map.sensors {
        if let Some(opts) = &sensor.options {
            image.insert(sensor.address, opts.first().expect("non-empty options").value);
        }
    }
    // Select controls likewise need a valid option index (raw 0 may not be one).
    for control in &map.controls {
        if let Constraint::Select { options } = &control.constraint {
            image.insert(
                control.address,
                options.first().expect("non-empty options").value,
            );
        }
    }
    image
}

fn synthetic_reads(
    spans: &[ReadSpan],
    image: &BTreeMap<u16, u16>,
) -> Vec<(ReadSpan, Vec<u16>)> {
    spans
        .iter()
        .map(|s| (*s, (s.start..s.start + s.len).map(|a| image[&a]).collect()))
        .collect()
}

#[test]
fn decodes_every_sensor_from_a_synthetic_image() {
    let map = load_from_str(EMBEDDED_MAP).unwrap();
    let spans = plan_spans(&map, MERGE_GAP);

    let snap = decode_all(&map, &synthetic_reads(&spans, &synthetic_image(&map, &spans))).unwrap();

    assert!(snap.link_ok);
    assert_eq!(snap.values.len(), map.sensors.len() + map.controls.len());
    for sensor in &map.sensors {
        assert!(snap.values.contains_key(&sensor.name), "missing {}", sensor.name);
    }
    for control in &map.controls {
        assert!(snap.values.contains_key(&control.name), "missing {}", control.name);
    }
    // u32 base-low over zero words assembles to 0, scale applies
    assert_eq!(snap.values["rated_power"], Value::Number(0.0));
}

#[test]
fn fails_loudly_when_a_span_is_missing() {
    let map = load_from_str(EMBEDDED_MAP).unwrap();
    let spans = plan_spans(&map, MERGE_GAP);

    let mut reads = synthetic_reads(&spans, &synthetic_image(&map, &spans));
    reads.remove(0); // spans are sorted; index 0 covers the serial-number block

    let err = decode_all(&map, &reads).unwrap_err().to_string();
    assert!(err.contains("not covered"), "unexpected error: {err}");
    assert!(err.contains("serial_number"), "error should name the sensor: {err}");
}
