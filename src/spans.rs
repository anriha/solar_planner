use std::collections::BTreeSet;

use crate::map::RegisterMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadSpan {
    pub start: u16,
    pub len: u16,
}

pub const MERGE_GAP: u16 = 8;

pub fn plan_spans(map: &RegisterMap, merge_gap: u16) -> Vec<ReadSpan> {
    let sensor_addresses: Vec<_> = map
        .sensors
        .iter()
        .flat_map(|sensor| sensor.address..sensor.address + sensor.encoding.word_count() as u16)
        .collect();

    let controls_addresses: Vec<_> = map
        .controls
        .iter()
        .flat_map(|control| control.address..control.address + control.encoding.word_count() as u16)
        .collect();

    let all_addresses: BTreeSet<_> = sensor_addresses
        .into_iter()
        .chain(controls_addresses)
        .collect();

    let addresses_vec: Vec<u16> = all_addresses.into_iter().collect();

    addresses_vec
        .chunk_by(|a, b| (b - a) <= merge_gap + 1)
        .flat_map(|run| {
            let start = run[0];
            let total = usize::from(run.last().unwrap() - start) + 1;

            (0..total).step_by(64).map(move |offset| ReadSpan {
                start: start + offset as u16,
                len: (total - offset).min(64) as u16,
            })
        })
        .collect()
}
