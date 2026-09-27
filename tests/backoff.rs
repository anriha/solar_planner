use std::time::Duration;

use solar_controller::bus::backoff_delay;

#[test]
fn first_failure_retries_on_the_regular_ticker() {
    // No added delay before the first retry — the ticker paces it.
    assert_eq!(backoff_delay(Duration::from_secs(5), 0), Duration::ZERO);
    assert_eq!(backoff_delay(Duration::from_secs(5), 1), Duration::ZERO);
}

#[test]
fn exponential_growth_from_the_second_consecutive_failure() {
    let iv = Duration::from_secs(5);
    assert_eq!(backoff_delay(iv, 2), Duration::from_secs(5));
    assert_eq!(backoff_delay(iv, 3), Duration::from_secs(10));
    assert_eq!(backoff_delay(iv, 4), Duration::from_secs(20));
    assert_eq!(backoff_delay(iv, 5), Duration::from_secs(40));
}

#[test]
fn capped_at_60s() {
    let iv = Duration::from_secs(5);
    assert_eq!(backoff_delay(iv, 6), Duration::from_secs(60)); // 80 uncapped
    assert_eq!(backoff_delay(iv, 100), Duration::from_secs(60));
}

#[test]
fn exponent_clamped_so_growth_stops() {
    let iv = Duration::from_secs(5);
    assert_eq!(backoff_delay(iv, 50), backoff_delay(iv, 8));
}
