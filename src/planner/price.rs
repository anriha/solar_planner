//! OTE day-ahead prices, the daily EUR rate, and the cache that serves both.
//!
//! # Contract
//!
//! Two key-less SOAP calls against `https://www.ote-cr.cz/pw-data/services/PublicDataService`,
//! both verified live on 2026-11-05:
//!
//! * `GetDamPricePeriodE` — `StartDate`, `EndDate` (`xs:date`, Prague civil dates),
//!   `PeriodResolution = PT15M`. One `Item` per MTU: `Date`, `PeriodIndex`,
//!   `PeriodInterval` (`"00:00-00:15"`), `Price`, `HourlyPrice`, `VolumeTotal`.
//! * `GetDamIndexE` — the same window. One `DamIndex` per day, carrying `EurRate`.
//!
//! Measured: `2026-11-04..2026-11-07` returned 288 items (96 per published day, tomorrow
//! already included at 21:00) with no `Price` missing, and an `EurRate` for every delivery
//! date, tomorrow included. One request pair therefore covers the whole horizon.
//!
//! # Units
//!
//! `Price` is **EUR/MWh**. On 2026-11-04 min / mean / max were 215.85 / 282.07 / 349.06 —
//! identical to an independent EUR series for the same day, where a CZK series would be
//! ~24× larger. `GetDamPricePeriodE` has no currency switch, so the conversion is ours:
//! `czk_per_kwh = eur_per_mwh * eur_rate / 1000`.
//!
//! `Price` is `minOccurs="0"`: an item without one is **skipped**, never read as zero.
//!
//! # Boundary
//!
//! This module fetches a window and converts it. It never decides which MTUs are in the
//! horizon and never filters on `now` — that is `horizon.rs`.

use std::collections::BTreeMap;
use std::future::Future;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use thiserror::Error;

use crate::planner::Mtu;
use crate::units::{CzkPerKwh, Hours};

/// One published MTU price, unconverted, with the rate that converts it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricePoint {
    pub mtu: Mtu,
    /// EUR/MWh, exactly as published.
    pub eur_per_mwh: f64,
    /// CZK per EUR for this point's delivery date, from `GetDamIndexE`.
    pub eur_rate: f64,
}

impl PricePoint {
    /// CZK/kWh — the unit `Slot::price_sell` and `TerminalValue` speak.
    pub fn czk_per_kwh(&self) -> CzkPerKwh {
        CzkPerKwh::new(self.eur_per_mwh * self.eur_rate / 1000.0)
    }
}

/// Where a price window comes from. Generic rather than `dyn`: an async trait object needs
/// boxing, and the only two implementors are the real client and a test fake.
pub trait PriceSource: Send + Sync + 'static {
    fn fetch(
        &self,
        from: Date,
        to: Date,
    ) -> impl Future<Output = Result<Vec<PricePoint>, PriceError>> + Send;
}

#[derive(Debug, Error)]
pub enum PriceError {
    #[error("OTE request failed: {0}")]
    Network(String),
    #[error("OTE answered HTTP {0}")]
    Http(u16),
    #[error("could not parse the OTE response: {0}")]
    Parse(String),
    #[error("no EUR rate for {date}, a day that has published prices")]
    MissingRate { date: Date },
}

impl PriceError {
    /// Whether waiting and retrying could succeed. Network failures and HTTP statuses are
    /// the retryable classes; a parse failure is a contract break that a retry cannot fix,
    /// so the caller surfaces it instead of looping.
    pub fn is_retryable(&self) -> bool {
        match self {
            PriceError::Network(_) | PriceError::Http(_) => true,
            PriceError::Parse(_) | PriceError::MissingRate { .. } => false,
        }
    }
}

/// The one endpoint both calls share.
pub const ENDPOINT: &str = "https://www.ote-cr.cz/pw-data/services/PublicDataService";

/// The request body for `GetDamPricePeriodE`, SOAP envelope included. Pure, so the request
/// shape is a unit test rather than a live call.
pub fn soap_price_request(from: Date, to: Date) -> String {
    soap_request(
        "GetDamPricePeriodE",
        &format!(
            "<pub:StartDate>{from}</pub:StartDate>\
             <pub:EndDate>{to}</pub:EndDate>\
             <pub:PeriodResolution>PT15M</pub:PeriodResolution>"
        ),
    )
}

/// The same for `GetDamIndexE`.
pub fn soap_index_request(from: Date, to: Date) -> String {
    soap_request(
        "GetDamIndexE",
        &format!("<pub:StartDate>{from}</pub:StartDate><pub:EndDate>{to}</pub:EndDate>"),
    )
}

/// One document-style SOAP envelope. The request prefixes the service namespace as `pub`;
/// the response declares it as the *default* namespace instead, which is why the parser
/// compares local names only.
fn soap_request(operation: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\" \
xmlns:pub=\"http://www.ote-cr.cz/schema/service/public\">\
<soapenv:Body><pub:{operation}>{body}</pub:{operation}></soapenv:Body></soapenv:Envelope>"
    )
}

// Settled 2026-11-05: **reuse the most recent known rate.** A delivery day that publishes
// prices but carries no `EurRate` row is converted at the newest rate seen for an earlier
// delivery date, and the point records that rate — a fraction of a percent on a number
// that is explicitly not precious. A hard error would blind the whole horizon over one
// missing index row. `PriceError::MissingRate` therefore survives only for the case where
// a priced day has no rate at all behind it, not even a historical one.

/// Parses a `GetDamPricePeriodEResponse`, converting with `rates` keyed by delivery date.
/// Pure: the fixture under `tests/fixtures/` is the whole input.
pub fn parse_prices(xml: &str, rates: &BTreeMap<Date, f64>) -> Result<Vec<PricePoint>, PriceError> {
    let zone = prague()?;
    let mut points = Vec::with_capacity(96);

    for raw in scan(xml, "Item")? {
        // `Price` is `minOccurs="0"`: a quarter-hour without a price is not a free one.
        let Some(price) = raw.price else { continue };
        let eur_per_mwh = price
            .trim()
            .parse::<f64>()
            .map_err(|_| parse_failed(format!("bad Price {price:?}")))?;

        let date = parse_date(raw.date.as_deref())?;
        let eur_rate =
            rate_for(date, rates).ok_or(PriceError::MissingRate { date })?;
        let index: i64 = raw
            .period_index
            .as_deref()
            .ok_or_else(|| parse_failed("item without a PeriodIndex"))?
            .trim()
            .parse()
            .map_err(|_| parse_failed("bad PeriodIndex"))?;

        points.push(PricePoint {
            mtu: Mtu {
                start: period_start(&zone, date, index)?,
                hours: Hours::new(0.25),
            },
            eur_per_mwh,
            eur_rate,
        });
    }

    Ok(points)
}

/// Parses a `GetDamIndexEResponse` into delivery date → CZK per EUR.
pub fn parse_rates(xml: &str) -> Result<BTreeMap<Date, f64>, PriceError> {
    let mut rates = BTreeMap::new();

    for raw in scan(xml, "DamIndex")? {
        let date = parse_date(raw.date.as_deref())?;
        // `EurRate` is optional too; a row without one simply contributes nothing, and
        // `rate_for` decides whether the horizon can live without it.
        let Some(rate) = raw.eur_rate else { continue };
        let rate = rate
            .trim()
            .parse::<f64>()
            .map_err(|_| parse_failed(format!("bad EurRate {rate:?}")))?;
        rates.insert(date, rate);
    }

    Ok(rates)
}

/// The zone every civil date in this module is expressed in.
pub(crate) fn prague() -> Result<TimeZone, PriceError> {
    TimeZone::get("Europe/Prague").map_err(|e| parse_failed(format!("no Prague tz: {e}")))
}

fn parse_failed(detail: impl Into<String>) -> PriceError {
    PriceError::Parse(detail.into())
}

/// The rate for `date`, or the newest one behind it — the settled fallback for a delivery
/// day that publishes prices without an index row.
fn rate_for(date: Date, rates: &BTreeMap<Date, f64>) -> Option<f64> {
    rates
        .get(&date)
        .copied()
        .or_else(|| rates.range(..date).next_back().map(|(_, rate)| *rate))
}

fn parse_date(raw: Option<&str>) -> Result<Date, PriceError> {
    let text = raw
        .ok_or_else(|| parse_failed("record without a Date"))?
        .trim();
    let bad = || parse_failed(format!("bad Date {text:?}"));

    let mut parts = text.split('-');
    let year: i16 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let month: i8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let day: i8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;

    Date::new(year, month, day).map_err(|e| parse_failed(format!("bad Date {text:?}: {e}")))
}

/// Period 1 is the first quarter-hour of the local civil day, and the grid is uniform in
/// absolute time — so a start is local midnight plus `index - 1` quarters. That is what
/// makes a 25-hour day 100 MTUs and a 23-hour day 92, with no special case for either.
fn period_start(zone: &TimeZone, date: Date, index: i64) -> Result<i64, PriceError> {
    let midnight = date
        .at(0, 0, 0, 0)
        .to_zoned(zone.clone())
        .map_err(|e| parse_failed(format!("{date} has no local midnight: {e}")))?;
    Ok(midnight.timestamp().as_second() + (index - 1) * 900)
}

/// One `<Item>` or `<DamIndex>` as text. Every field stays optional: the schema marks
/// `Price`, `EurRate` and the volumes `minOccurs="0"`.
#[derive(Default)]
struct Raw {
    date: Option<String>,
    period_index: Option<String>,
    price: Option<String>,
    eur_rate: Option<String>,
}

#[derive(Clone, Copy)]
enum Field {
    Date,
    PeriodIndex,
    Price,
    EurRate,
}

impl Field {
    fn from_tag(tag: &[u8]) -> Option<Field> {
        match tag {
            b"Date" => Some(Field::Date),
            b"PeriodIndex" => Some(Field::PeriodIndex),
            b"Price" => Some(Field::Price),
            b"EurRate" => Some(Field::EurRate),
            _ => None,
        }
    }
}

/// Walks a response and collects every `record` element. Namespace-agnostic by design:
/// only local names are compared, because the request and the response declare the same
/// namespace in different ways.
fn scan(xml: &str, record: &str) -> Result<Vec<Raw>, PriceError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text_start = true;
    reader.config_mut().trim_text_end = true;

    let mut records = Vec::new();
    let mut current: Option<Raw> = None;
    let mut field: Option<Field> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                let tag = start.local_name();
                let tag = tag.as_ref();
                if tag == record.as_bytes() {
                    current = Some(Raw::default());
                } else {
                    field = Field::from_tag(tag);
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(raw), Some(which)) = (current.as_mut(), field) {
                    // The reader hands text over already entity-decoded; `xml10_content`
                    // is the XML 1.0 decode plus the required EOL normalization.
                    let value = text
                        .xml10_content()
                        .map_err(|e| parse_failed(format!("bad text: {e}")))?
                        .into_owned();
                    match which {
                        Field::Date => raw.date = Some(value),
                        Field::PeriodIndex => raw.period_index = Some(value),
                        Field::Price => raw.price = Some(value),
                        Field::EurRate => raw.eur_rate = Some(value),
                    }
                }
            }
            Ok(Event::End(end)) => {
                if end.local_name().as_ref() == record.as_bytes()
                    && let Some(raw) = current.take()
                {
                    records.push(raw);
                }
                field = None;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(parse_failed(format!("malformed XML: {e}"))),
        }
    }

    Ok(records)
}

/// The SOAP action namespace both operations share.
const NAMESPACE: &str = "http://www.ote-cr.cz/schema/service/public";

/// The real OTE client: two SOAP POSTs per fetch, the index first so the prices can be
/// converted the moment they arrive.
pub struct OteClient {
    http: reqwest::Client,
    endpoint: String,
}

impl OteClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            endpoint: ENDPOINT.to_string(),
        }
    }

    /// One SOAP POST. A transport failure or a non-2xx answer is a retryable
    /// [`PriceError`]; nothing here interprets the body.
    async fn post(&self, operation: &str, body: String) -> Result<String, PriceError> {
        let response = self
            .http
            .post(&self.endpoint)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{NAMESPACE}/{operation}\""))
            .body(body)
            .send()
            .await
            .map_err(|e| PriceError::Network(e.to_string()))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| PriceError::Network(e.to_string()))?;

        if !status.is_success() {
            return Err(PriceError::Http(status.as_u16()));
        }
        Ok(text)
    }
}

impl PriceSource for OteClient {
    async fn fetch(&self, from: Date, to: Date) -> Result<Vec<PricePoint>, PriceError> {
        // The index first: prices without rates cannot be converted, so a failed index
        // call costs one round-trip and nothing else.
        let index = self
            .post("GetDamIndexE", soap_index_request(from, to))
            .await?;
        let rates = parse_rates(&index)?;

        let prices = self
            .post("GetDamPricePeriodE", soap_price_request(from, to))
            .await?;
        parse_prices(&prices, &rates)
    }
}

/// A live check, not part of the default run: `cargo test -- --ignored live_ote`.
#[tokio::test]
#[ignore = "hits the public OTE service"]
async fn live_ote_returns_a_convertible_window() {
    let client = OteClient::new(reqwest::Client::new());
    let to = jiff::civil::date(2026, 11, 7);
    let from = jiff::civil::date(2026, 11, 4);
    let points = client.fetch(from, to).await.expect("live fetch");

    assert!(points.len() >= 96, "at least today, got {}", points.len());
    assert!(points.iter().all(|p| p.eur_rate > 5.0), "rates converted");
}

/// The widened fetch the terminal value depends on, checked against the service instead of
/// assumed: `cargo test --lib -- --ignored live_ote_week`.
///
/// The service is asked for a week more than the horizon needs. If it refused, or silently
/// trimmed the range, the terminal value would fall back to the bootstrap constant forever
/// and nothing else would notice.
#[tokio::test]
#[ignore = "hits the public OTE service"]
async fn live_ote_serves_a_week_of_history() {
    use crate::planner::{TERMINAL_DAYS, TerminalValue};
    use crate::units::CzkPerKwh;

    let client = OteClient::new(reqwest::Client::new());
    let today = jiff::civil::date(2026, 11, 4);
    let from = today
        .checked_sub(jiff::Span::new().days(TERMINAL_DAYS - 1))
        .expect("in range");
    let to = today
        .tomorrow()
        .and_then(|day| day.tomorrow())
        .expect("in range");

    let points = client
        .fetch(from, to)
        .await
        .expect("live fetch of the widened window");
    let days: std::collections::BTreeSet<i64> = points
        .iter()
        .map(|p| p.mtu.start.div_euclid(86_400))
        .collect();
    assert!(
        days.len() >= 7,
        "expected a week of civil days, got {} days over {} points",
        days.len(),
        points.len()
    );

    // 2026-11-04T12:00 Prague: the clock the refresh would hold.
    let now = 1_793_746_800 + 12 * 3600;
    let value = TerminalValue::measured(&points, now, CzkPerKwh::new(0.5))
        .expect("a week of prices measures a terminal value");
    assert!(
        (0.0..=30.0).contains(&value.czk_per_kwh.get()),
        "implausible terminal value {} CZK/kWh",
        value.czk_per_kwh.get()
    );
    eprintln!(
        "live terminal value: {:.3} CZK/kWh over {days:?}",
        value.czk_per_kwh.get()
    );
}

/// The last good window and when it was taken.
#[derive(Debug, Default)]
pub struct PriceCache {
    points: Vec<PricePoint>,
    fetched_at: i64,
}

impl PriceCache {
    pub fn points(&self) -> &[PricePoint] {
        &self.points
    }

    pub fn fetched_at(&self) -> i64 {
        self.fetched_at
    }

    pub fn replace(&mut self, points: Vec<PricePoint>, at: i64) {
        self.points = points;
        self.fetched_at = at;
    }

    /// True when the cached window covers every MTU starting in `now..until`.
    pub fn covers(&self, now: i64, until: i64) -> bool {
        let (Some(first), Some(last)) = (self.points.first(), self.points.last()) else {
            return false;
        };
        let needed_until = quarter_floor(until - 1);
        first.mtu.start <= quarter_floor(now) && last.mtu.start >= needed_until
    }

    /// Seconds since the window was fetched; 0 for an empty cache.
    pub fn age_secs(&self, now: i64) -> i64 {
        if self.points.is_empty() {
            0
        } else {
            (now - self.fetched_at).max(0)
        }
    }
}

/// The start of the quarter-hour containing `instant`.
fn quarter_floor(instant: i64) -> i64 {
    instant - instant.rem_euclid(900)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRICES: &str =
        include_str!("../../tests/fixtures/ote_price_period_2026-11-04_2026-11-07.xml");
    const INDEX: &str = include_str!("../../tests/fixtures/ote_index_2026-11-04_2026-11-07.xml");

    use crate::units::Hours;

    /// 2026-11-04T00:00 Prague (CET, UTC+1) — the first MTU of the captured window.
    const FIRST_MTU: i64 = 1_793_746_800;
    /// 2026-11-06T23:45 Prague — the last one.
    const LAST_MTU: i64 = 1_794_005_100;

    fn day(year: i16, month: i8, day: i8) -> Date {
        jiff::civil::date(year, month, day)
    }

    fn point(start: i64) -> PricePoint {
        PricePoint {
            mtu: Mtu {
                start,
                hours: Hours::new(0.25),
            },
            eur_per_mwh: 100.0,
            eur_rate: 24.0,
        }
    }

    #[test]
    fn parses_every_mtu_of_every_published_day() {
        let rates = parse_rates(INDEX).expect("the captured index must parse");
        let prices = parse_prices(PRICES, &rates).expect("the captured window must parse");

        assert_eq!(prices.len(), 288, "three published days of 96 MTUs each");
        assert_eq!(prices[0].mtu.start, FIRST_MTU);
        assert_eq!(prices[287].mtu.start, LAST_MTU);
        assert!(
            prices.windows(2).all(|w| w[1].mtu.start - w[0].mtu.start == 900),
            "the response is one quarter-hour grid, in order, with no gaps"
        );
        assert!(prices.iter().all(|p| p.mtu.hours.get() == 0.25));
    }

    #[test]
    fn skips_an_item_without_a_price_instead_of_reading_it_as_zero() {
        // `Price` is `minOccurs="0"`: the second item below has volume and no price.
        let xml = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><GetDamPricePeriodEResponse xmlns="http://www.ote-cr.cz/schema/service/public"><Result><Item><Date>2026-11-04</Date><PeriodResolution>PT15M</PeriodResolution><PeriodIndex>1</PeriodIndex><PeriodInterval>00:00-00:15</PeriodInterval><Price>215.85</Price><VolumeTotal>901.425</VolumeTotal></Item><Item><Date>2026-11-04</Date><PeriodResolution>PT15M</PeriodResolution><PeriodIndex>2</PeriodIndex><PeriodInterval>00:15-00:30</PeriodInterval><VolumeTotal>906.400</VolumeTotal></Item></Result></GetDamPricePeriodEResponse></soap:Body></soap:Envelope>"#;
        let mut rates = BTreeMap::new();
        rates.insert(day(2026, 11, 4), 24.31);

        let prices = parse_prices(xml, &rates).expect("one priced item is enough to succeed");

        assert_eq!(prices.len(), 1, "the unpriced item is skipped, not zero-filled");
        assert_eq!(prices[0].eur_per_mwh, 215.85);
        assert!(
            prices.iter().all(|p| p.eur_per_mwh > 0.0),
            "a skipped item must not reappear as a free kWh"
        );
    }

    #[test]
    fn rates_are_keyed_by_delivery_date_and_cover_tomorrow() {
        let rates = parse_rates(INDEX).expect("the captured index must parse");

        assert_eq!(rates.len(), 3, "one row per delivery date");
        assert!((rates[&day(2026, 11, 4)] - 24.310).abs() < 1e-9);
        assert!((rates[&day(2026, 11, 5)] - 24.290).abs() < 1e-9);
        assert!(
            (rates[&day(2026, 11, 6)] - 24.290).abs() < 1e-9,
            "tomorrow's rate is published with tomorrow's prices"
        );
    }

    #[test]
    fn converts_with_the_delivery_days_own_rate() {
        let xml = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><GetDamPricePeriodEResponse xmlns="http://www.ote-cr.cz/schema/service/public"><Result><Item><Date>2026-11-04</Date><PeriodResolution>PT15M</PeriodResolution><PeriodIndex>1</PeriodIndex><PeriodInterval>00:00-00:15</PeriodInterval><Price>215.85</Price></Item><Item><Date>2026-11-05</Date><PeriodResolution>PT15M</PeriodResolution><PeriodIndex>1</PeriodIndex><PeriodInterval>00:00-00:15</PeriodInterval><Price>300.00</Price></Item></Result></GetDamPricePeriodEResponse></soap:Body></soap:Envelope>"#;
        let mut rates = BTreeMap::new();
        rates.insert(day(2026, 11, 4), 24.0);
        rates.insert(day(2026, 11, 5), 30.0);

        let prices = parse_prices(xml, &rates).expect("both days have a rate");

        assert_eq!(prices.len(), 2);
        assert!(
            (prices[0].czk_per_kwh().get() - 215.85 * 24.0 / 1000.0).abs() < 1e-9,
            "EUR/MWh × CZK/EUR ÷ 1000, with the 4th's rate"
        );
        assert!(
            (prices[1].czk_per_kwh().get() - 300.00 * 30.0 / 1000.0).abs() < 1e-9,
            "the 5th must not borrow the 4th's rate"
        );
    }

    #[test]
    fn the_request_carries_the_operation_and_both_civil_dates() {
        let body = soap_price_request(day(2026, 11, 4), day(2026, 11, 6));

        assert!(body.contains("<pub:GetDamPricePeriodE>"), "the operation element");
        assert!(body.contains("<pub:StartDate>2026-11-04</pub:StartDate>"));
        assert!(body.contains("<pub:EndDate>2026-11-06</pub:EndDate>"));
        assert!(body.contains("PT15M"), "the period resolution is the whole point");
        assert!(
            body.contains("xmlns:pub=\"http://www.ote-cr.cz/schema/service/public\""),
            "the service namespace, declared on the request prefix"
        );

        let index = soap_index_request(day(2026, 11, 4), day(2026, 11, 6));
        assert!(index.contains("<pub:GetDamIndexE>"));
        assert!(
            !index.contains("PeriodResolution"),
            "the index takes no resolution"
        );
    }

    #[test]
    fn network_and_http_errors_are_retryable_and_parse_errors_are_not() {
        assert!(PriceError::Network("connection reset".into()).is_retryable());
        assert!(PriceError::Http(503).is_retryable());
        assert!(
            !PriceError::Parse("unexpected end of stream".into()).is_retryable(),
            "a contract break does not heal by waiting"
        );
        assert!(
            !PriceError::MissingRate {
                date: day(2026, 11, 5)
            }
            .is_retryable()
        );
    }

    #[test]
    fn cache_covers_exactly_up_to_its_last_mtu() {
        let mut cache = PriceCache::default();
        assert!(
            !cache.covers(FIRST_MTU, FIRST_MTU + 900),
            "an empty cache covers nothing"
        );

        let points: Vec<PricePoint> = (0..4).map(|k| point(FIRST_MTU + k * 900)).collect();
        cache.replace(points, 1_000);

        assert!(cache.covers(FIRST_MTU, FIRST_MTU + 4 * 900), "the window itself");
        assert!(
            !cache.covers(FIRST_MTU, FIRST_MTU + 5 * 900),
            "one MTU past the last published MTU is not covered"
        );
        assert!(
            !cache.covers(FIRST_MTU - 900, FIRST_MTU + 900),
            "it cannot cover an MTU that starts before its first"
        );
    }

    #[test]
    fn a_priced_day_without_a_rate_reuses_the_most_recent_one() {
        let xml = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><GetDamPricePeriodEResponse xmlns="http://www.ote-cr.cz/schema/service/public"><Result><Item><Date>2026-11-04</Date><PeriodResolution>PT15M</PeriodResolution><PeriodIndex>1</PeriodIndex><PeriodInterval>00:00-00:15</PeriodInterval><Price>215.85</Price></Item><Item><Date>2026-11-05</Date><PeriodResolution>PT15M</PeriodResolution><PeriodIndex>1</PeriodIndex><PeriodInterval>00:00-00:15</PeriodInterval><Price>300.00</Price></Item></Result></GetDamPricePeriodEResponse></soap:Body></soap:Envelope>"#;
        let mut rates = BTreeMap::new();
        rates.insert(day(2026, 11, 4), 24.0);

        let prices = parse_prices(xml, &rates).expect("a missing row for one day is not fatal");

        assert_eq!(prices.len(), 2);
        assert!(
            (prices[1].eur_rate - 24.0).abs() < 1e-9,
            "the 5th inherits the newest rate behind it, got {}",
            prices[1].eur_rate
        );

        // Nothing behind it at all: still an error, because there is nothing to reuse.
        let err = parse_prices(xml, &BTreeMap::new()).expect_err("no rate anywhere");
        assert!(matches!(err, PriceError::MissingRate { .. }), "got {err:?}");
    }
}
