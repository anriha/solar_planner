//! Prague-clock formatting for the axes and the header.
//!
//! The plan is published in unix seconds and the bridge splits its days in Prague time, so
//! the viewer formats in the same zone. jiff carries the tzdb compiled in
//! (`tzdb-bundle-always`), which is what makes this work in a browser too.

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The zone the plan and its day buckets are in.
pub fn prague() -> TimeZone {
    TimeZone::get("Europe/Prague").expect("the Prague zone is compiled in")
}

/// `HH:MM` at `unix`, e.g. `14:45`.
pub fn clock(unix: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(unix)
        .expect("plan timestamps are in range")
        .to_zoned(zone.clone())
        .strftime("%H:%M")
        .to_string()
}

/// `YYYY-MM-DD` at `unix`.
pub fn date(unix: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(unix)
        .expect("plan timestamps are in range")
        .to_zoned(zone.clone())
        .strftime("%Y-%m-%d")
        .to_string()
}

/// A short human age, for the "computed 12 s ago" line.
pub fn age(seconds: i64) -> String {
    match seconds {
        s if s < 0 => "just now".to_string(),
        s if s < 90 => format!("{s} s ago"),
        s if s < 90 * 60 => format!("{} min ago", s / 60),
        s if s < 48 * 3600 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-04T00:00 Prague (CET, UTC+1) — the same anchor the bridge's tests use.
    const T0: i64 = 1_793_746_800;

    #[test]
    fn the_axis_reads_prague_time() {
        let zone = prague();
        assert_eq!(clock(T0, &zone), "00:00");
        assert_eq!(clock(T0 + 14 * 3600 + 45 * 60, &zone), "14:45");
        assert_eq!(date(T0, &zone), "2026-11-04");
        assert_eq!(date(T0 + 86_400, &zone), "2026-11-05");
    }

    #[test]
    fn the_dst_fold_is_not_assumed_away() {
        // 2026-10-25T00:00 Prague is still CEST (UTC+2); the clocks fall back at 03:00
        // local. So 22:00 UTC on the 24th is 00:00 local, and 01:00 UTC is 02:00 local
        // (CET) — an hour the other side of the fold.
        let zone = prague();
        let midnight = 1_792_879_200; // 2026-10-25T00:00 Prague, still CEST (UTC+2)
        assert_eq!(clock(midnight, &zone), "00:00");
        // Three hours later the clocks have fallen back: the same wall clock hour is CET.
        assert_eq!(clock(midnight + 3 * 3600, &zone), "02:00");
    }

    #[test]
    fn ages_stay_human() {
        assert_eq!(age(-5), "just now");
        assert_eq!(age(42), "42 s ago");
        assert_eq!(age(600), "10 min ago");
        assert_eq!(age(7200), "2 h ago");
        assert_eq!(age(3 * 86_400), "3 days ago");
    }
}
