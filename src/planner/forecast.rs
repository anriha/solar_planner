//! forecast.solar: the seam and the type. The client, the hourly→quarter split and the
//! calibration ratio land in their own slice.
//!
//! # Contract
//!
//! PV is **DC** energy for one MTU — array output before any conversion. `Slot::pv_kw` is
//! DC on purpose: every AC-side number is produced by a `Params` efficiency path. One
//! array: 16.5 kWp, azimuth 200°, tilt 37°. The calibration ratio is applied at the
//! source, so the DP only ever sees calibrated numbers.

use std::future::Future;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use thiserror::Error;

use crate::planner::Mtu;
use crate::units::{DcKwh, Hours};

/// The estimate endpoint; the site is appended as path segments.
const ESTIMATE: &str = "https://api.forecast.solar/estimate";

/// Forecast PV for one MTU, DC, already calibrated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PvPoint {
    pub mtu: Mtu,
    pub dc_kwh: DcKwh,
}

/// Where PV forecasts come from.
pub trait PvSource: Send + Sync + 'static {
    fn fetch(
        &self,
        from: Date,
        to: Date,
    ) -> impl Future<Output = Result<Vec<PvPoint>, ForecastError>> + Send;
}

#[derive(Debug, Error)]
pub enum ForecastError {
    #[error("forecast.solar request failed: {0}")]
    Network(String),
    #[error("forecast.solar answered HTTP {0}")]
    Http(u16),
    #[error("could not parse the forecast response: {0}")]
    Parse(String),
}

/// The plant's array, as forecast.solar needs it. Boot config, not policy.
#[derive(Debug, Clone, Copy)]
pub struct Site {
    pub latitude: f64,
    pub longitude: f64,
    /// Panel tilt from horizontal, degrees.
    pub declination: f64,
    /// Compass bearing, degrees — 0 north, 180 south.
    pub azimuth: f64,
    pub kwp: f64,
}

impl Site {
    /// forecast.solar measures azimuth from south: 0 = south, −90 = east, +90 = west.
    /// A compass bearing of 200° is therefore sent as 20°.
    fn api_azimuth(&self) -> f64 {
        self.azimuth - 180.0
    }
}

/// The real forecast.solar client.
pub struct ForecastSolar {
    http: reqwest::Client,
    site: Site,
}

impl ForecastSolar {
    pub fn new(http: reqwest::Client, site: Site) -> Self {
        Self { http, site }
    }
}

impl PvSource for ForecastSolar {
    fn fetch(
        &self,
        _from: Date,
        _to: Date,
    ) -> impl Future<Output = Result<Vec<PvPoint>, ForecastError>> + Send {
        let site = self.site;
        async move {
            // The estimate endpoint has no date range: it always answers with its own
            // horizon, and the horizon module decides which MTUs matter.
            let url = format!(
                "{ESTIMATE}/{:.4}/{:.4}/{:.2}/{:.2}/{:.2}",
                site.latitude,
                site.longitude,
                site.declination,
                site.api_azimuth(),
                site.kwp
            );

            let response = self
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| ForecastError::Network(e.to_string()))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|e| ForecastError::Network(e.to_string()))?;

            if !status.is_success() {
                return Err(ForecastError::Http(status.as_u16()));
            }

            parse_estimate(&body)
        }
    }
}

/// Turns `result.watt_hours_period` into quarter-hour MTUs, calibrated.
///
/// The API publishes **Wh per local hour** keyed by a wall-clock stamp
/// (`"2026-11-06 08:00:00"`, occasionally with stray seconds, so only the hour is used).
/// Each hour is spread evenly over its four quarters: the day-ahead grid is quarter-hour
/// and a coarser forecast is not a reason to change that.
pub fn parse_estimate(json: &str) -> Result<Vec<PvPoint>, ForecastError> {
    let doc: serde_json::Value =
        serde_json::from_str(json).map_err(|e| ForecastError::Parse(e.to_string()))?;
    let periods = doc
        .get("result")
        .and_then(|result| result.get("watt_hours_period"))
        .and_then(|periods| periods.as_object())
        .ok_or_else(|| ForecastError::Parse("no result.watt_hours_period".to_string()))?;

    let zone = TimeZone::get("Europe/Prague")
        .map_err(|e| ForecastError::Parse(format!("no Prague tz: {e}")))?;
    let mut points = Vec::with_capacity(periods.len() * 4);

    for (stamp, value) in periods {
        let wh = value
            .as_f64()
            .ok_or_else(|| ForecastError::Parse(format!("non-numeric value at {stamp:?}")))?;
        let hour = hour_start(stamp, &zone)?;
        // Wh → kWh, then a quarter of the hour. Calibration is policy and is applied
        // by the horizon, so the forecast client stays a pure reader.
        let per_quarter = wh / 4000.0;
        for quarter in 0..4 {
            points.push(PvPoint {
                mtu: Mtu {
                    start: hour + quarter * 900,
                    hours: Hours::new(0.25),
                },
                dc_kwh: DcKwh::new(per_quarter),
            });
        }
    }

    points.sort_by_key(|point| point.mtu.start);
    Ok(points)
}

fn hour_start(stamp: &str, zone: &TimeZone) -> Result<i64, ForecastError> {
    let bad = || ForecastError::Parse(format!("bad hour stamp {stamp:?}"));
    let text = stamp.trim();
    let (date, time) = text.split_once(' ').ok_or_else(bad)?;

    let mut parts = date.split('-');
    let year: i16 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let month: i8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let day: i8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let hour: i8 = time
        .split(':')
        .next()
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;

    let date = Date::new(year, month, day).map_err(|_| bad())?;
    date.at(hour, 0, 0, 0)
        .to_zoned(zone.clone())
        .map(|zoned| zoned.timestamp().as_second())
        .map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESTIMATE_BODY: &str = r#"{"result":{"watts":{},"watt_hours_period":{"2026-11-06 07:00:25":0,"2026-11-06 08:00:00":400,"2026-11-06 09:00:00":1200},"watt_hours":{},"watt_hours_day":{}},"message":{"code":0,"type":"info","text":"ok"}}"#;

    /// 2026-11-06T08:00 Prague (CET, UTC+1).
    const HOUR_8: i64 = 1_793_948_400;

    #[test]
    fn spreads_each_published_hour_over_four_mtus() {
        let points = parse_estimate(ESTIMATE_BODY).expect("the captured body parses");

        assert_eq!(points.len(), 12, "three published hours, four quarters each");
        assert_eq!(points[0].mtu.start, HOUR_8 - 3600, "the 07:00 hour comes first");
        assert!(points.windows(2).all(|w| w[1].mtu.start - w[0].mtu.start == 900));

        let eight = points
            .iter()
            .filter(|p| p.mtu.start >= HOUR_8 && p.mtu.start < HOUR_8 + 3600)
            .collect::<Vec<_>>();
        assert_eq!(eight.len(), 4);
        for point in eight {
            assert!(
                (point.dc_kwh.get() - 0.1).abs() < 1e-12,
                "400 Wh over four quarters is 0.1 kWh each, got {}",
                point.dc_kwh
            );
            assert_eq!(point.mtu.hours.get(), 0.25);
        }
    }

    #[test]
    fn azimuth_is_sent_relative_to_south() {
        let site = Site {
            latitude: 50.08,
            longitude: 14.45,
            declination: 37.0,
            azimuth: 200.0,
            kwp: 16.5,
        };
        assert!(
            (site.api_azimuth() - 20.0).abs() < 1e-9,
            "200° compass is 20° west of south"
        );
    }

    #[test]
    fn a_body_without_a_period_map_is_an_error() {
        let err = parse_estimate(r#"{"result":{}}"#).expect_err("no periods");
        assert!(matches!(err, ForecastError::Parse(_)), "got {err:?}");
    }
}
