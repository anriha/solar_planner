use std::collections::{BTreeMap, BTreeSet};

use solar_controller::map::{self, RegisterMap};
use solar_controller::spans::{self, ReadSpan};

// The poll-cycle layout constant: bridge gaps up to this many dead registers.
// Rationale (plan.md gotchas): at 9600 8N1 a dead reg costs ~2.3 ms of response
// payload vs ~35-50 ms fixed per extra FC03 request. With G=8 the embedded map
// yields 13 spans; without bridging it would be 33 requests per cycle.
const MERGE_GAP: u16 = 8;

fn embedded() -> RegisterMap {
    map::load_from_str(map::EMBEDDED_MAP).expect("embedded map must parse")
}

fn plan(map: &RegisterMap, merge_gap: u16) -> Vec<ReadSpan> {
    spans::plan_spans(map, merge_gap)
}

fn pairs(spans: &[ReadSpan]) -> Vec<(u16, u16)> {
    spans.iter().map(|s| (s.start, s.len)).collect()
}

/// Every word any sensor or control consumes, by the same rule plan_spans
/// uses: address..address + word_count (exclusive end).
fn expected_words(map: &RegisterMap) -> BTreeSet<u16> {
    map.sensors
        .iter()
        .flat_map(|s| s.address..s.address + s.encoding.word_count() as u16)
        .chain(
            map.controls
                .iter()
                .flat_map(|c| c.address..c.address + c.encoding.word_count() as u16),
        )
        .collect()
}

fn covered_counts(spans: &[ReadSpan]) -> BTreeMap<u16, u32> {
    let mut counts = BTreeMap::new();
    for span in spans {
        for addr in span.start..span.start + span.len {
            *counts.entry(addr).or_default() += 1;
        }
    }
    counts
}

#[test]
fn embedded_map_covers_every_word_exactly_once() {
    let m = embedded();
    let counts = covered_counts(&plan(&m, MERGE_GAP));

    for addr in expected_words(&m) {
        assert_eq!(
            counts.get(&addr),
            Some(&1),
            "sensor/control word {addr} must be covered by exactly one span"
        );
    }
    let doubled: Vec<u16> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(a, _)| *a)
        .collect();
    assert!(
        doubled.is_empty(),
        "no register may be read twice per cycle: {doubled:?}"
    );
}

#[test]
fn embedded_map_gap_zero_covers_exactly_and_nothing_more() {
    // merge_gap = 0 disables bridging: coverage must equal the map's words
    // exactly - spans then never read registers nobody consumes.
    let m = embedded();
    let covered: BTreeSet<u16> = covered_counts(&plan(&m, 0)).into_keys().collect();
    assert_eq!(covered, expected_words(&m));
}

#[test]
fn embedded_map_spans_sorted_disjoint_and_fully_bridged() {
    let spans = plan(&embedded(), MERGE_GAP);
    for pair in spans.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        let prev_end = prev.start + prev.len;
        assert!(
            next.start >= prev_end,
            "spans must be sorted and disjoint: {prev:?} before {next:?}"
        );
        // dead regs between spans: 0 is legal (the 586/650 chunk split),
        // 1..=MERGE_GAP is a missed bridge, > MERGE_GAP is unbridgeable.
        let dead = next.start.saturating_sub(prev_end);
        assert!(
            dead == 0 || dead > MERGE_GAP,
            "{dead} dead regs between {prev:?} and {next:?}: adjacency is only \
             legal as a chunk split, gaps up to {MERGE_GAP} must be bridged"
        );
    }
}

#[test]
fn embedded_map_respects_chunk_limit() {
    for span in plan(&embedded(), MERGE_GAP) {
        assert!(
            span.len <= 64,
            "FC03 chunk limit is 64 regs: {span:?} (start {}, len {})",
            span.start,
            span.len
        );
    }
}

#[test]
fn embedded_map_matches_plan_layout() {
    // The contract against plan.md's poll-cycle enumeration (8 sensor spans
    // + control read-backs, 586-679 chunked at <= 64). Any map edit that
    // changes the layout must update this - and the timing estimate.
    let got = pairs(&plan(&embedded(), MERGE_GAP));
    let expected: Vec<(u16, u16)> = vec![
        (3, 5),    // serial number
        (20, 2),   // rated power
        (80, 1),   // inverter_onoff
        (102, 16), // battery setup + SOC thresholds (102..=117)
        (128, 3),  // charge enables + grid charge current
        (142, 45), // work mode .. grid protection (142..=186, TOU inside)
        (210, 11), // BMS mirror
        (340, 1),  // max sell power (naming conflict, read-back only)
        (500, 1),  // run state
        (514, 28), // energy block (514..=541, bridged over 532-533/537-539)
        (555, 4),  // fault codes
        (586, 64), // live block, first chunk of 94 regs
        (650, 30), // live block, second chunk
    ];
    assert_eq!(got, expected);
}

// ------------------------------------------------------------ synthetic maps

fn parse(s: &str) -> RegisterMap {
    map::load_from_str(s).expect("synthetic map must parse")
}

#[test]
fn empty_map_yields_no_spans() {
    let m = parse("sensors = []\ncontrols = []\n");
    assert!(plan(&m, MERGE_GAP).is_empty());
}

#[test]
fn gap_bridging_follows_merge_gap() {
    let m = parse(
        r#"sensors = [
        { name = "a", address = 10, encoding = { type = "u16" } },
        { name = "b", address = 20, encoding = { type = "u16" } },
    ]"#,
    );
    // 9 dead regs between them: split at G=5, one bridged read at G=15.
    assert_eq!(pairs(&plan(&m, 5)), vec![(10, 1), (20, 1)]);
    assert_eq!(pairs(&plan(&m, 15)), vec![(10, 11)]);
}

#[test]
fn long_run_chunks_at_64() {
    let m = parse(
        r#"sensors = [ { name = "blob", address = 100, encoding = { type = "str", words = 200 } } ]"#,
    );
    assert_eq!(
        pairs(&plan(&m, 0)),
        vec![(100, 64), (164, 64), (228, 64), (292, 8)]
    );
}
