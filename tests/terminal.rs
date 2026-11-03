//! `TerminalValue::measured` — the one number the solver assumes a stored kWh is worth at
//! the end of the horizon. Pure: prices and a clock in, a level out.
//!
//! The rule it pins (settled 2026-11-04): the median of each day's maximum spot price over
//! the 7 Prague days ending today, minus the sell fee. The tests below are chosen so that a
//! mean, an off-by-one window or a missing fee all fail one of them.

use solar_controller::planner::{Mtu, TERMINAL_DAYS, TerminalValue};
use solar_controller::planner::price::PricePoint;
use solar_controller::units::{CzkPerKwh, Hours};

/// 2026-11-04T00:00 Prague (CET, UTC+1) — no DST change inside the test window.
const TODAY: i64 = 1_793_746_800;
const DAY: i64 = 86_400;
/// 25 CZK/EUR, so `czk_per_kwh = eur_per_mwh / 40`.
const RATE: f64 = 25.0;

fn point(start: i64, czk_per_kwh: f64) -> PricePoint {
    PricePoint {
        mtu: Mtu {
            start,
            hours: Hours::new(0.25),
        },
        eur_per_mwh: czk_per_kwh * 40.0,
        eur_rate: RATE,
    }
}

/// One day's worth of prices: a quiet morning and a maximum exactly at noon.
fn day(offset: i64, max_czk: f64) -> Vec<PricePoint> {
    let start = TODAY + offset * DAY;
    vec![point(start, max_czk / 2.0), point(start + 12 * 3600, max_czk)]
}

fn median_of(maxima: &[f64]) -> f64 {
    let mut points = Vec::new();
    let offsets: Vec<i64> = (0..maxima.len() as i64).map(|i| i - (maxima.len() as i64 - 1)).collect();
    for (offset, max) in offsets.iter().zip(maxima) {
        points.extend(day(*offset, *max));
    }
    TerminalValue::measured(&points, TODAY + 3600, CzkPerKwh::new(0.5))
        .expect("a window with prices measures a value")
        .czk_per_kwh
        .get()
}

#[test]
fn the_value_is_the_median_of_the_daily_maxima_not_their_mean() {
    // Maxima 1..6 CZK/kWh and one spike. Median = 4.0; a mean would be 10.14, and the
    // spike's whole point is that it must not reach the level.
    assert!((median_of(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 50.0]) - 3.5).abs() < 1e-9);
}

#[test]
fn the_window_is_the_seven_days_ending_today() {
    // Seven maxima, median 4.0 — and the assertion above already proves the count is odd,
    // so the window is 7 and not 6 or 8. Here: a day *older* than the window and a day in
    // the future carry absurd prices and must not move it at all.
    let mut points = Vec::new();
    for (offset, max) in (-6..=0).zip([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]) {
        points.extend(day(offset, max));
    }
    points.extend(day(-TERMINAL_DAYS, 10_000.0));
    points.extend(day(1, 10_000.0));

    let value = TerminalValue::measured(&points, TODAY + 3600, CzkPerKwh::new(0.5))
        .expect("seven days inside the window");
    assert!(
        (value.czk_per_kwh.get() - 3.5).abs() < 1e-9,
        "median 4.0 minus the fee; got {}",
        value.czk_per_kwh.get()
    );
}

#[test]
fn the_fee_is_deducted_after_the_median() {
    let one_day = day(0, 10.0);
    let value = TerminalValue::measured(&one_day, TODAY + 3600, CzkPerKwh::new(2.5))
        .expect("a single day is still a measurement");
    assert!(
        (value.czk_per_kwh.get() - 7.5).abs() < 1e-9,
        "10.0 max minus 2.5 fee = 7.5, got {}",
        value.czk_per_kwh.get()
    );
}

#[test]
fn an_even_number_of_days_averages_the_two_middles() {
    // 2.0 and 4.0 → 3.0, minus the 0.5 fee.
    assert!((median_of(&[2.0, 4.0]) - 2.5).abs() < 1e-9);
}

#[test]
fn no_prices_means_no_measurement() {
    assert!(
        TerminalValue::measured(&[], TODAY, CzkPerKwh::new(0.5)).is_none(),
        "the caller keeps its bootstrap value; a made-up level would hide the gap"
    );
}
