//! Prices + forecast PV + the site's own load → the horizon `solve` consumes, and the
//! actor that keeps that window warm.
//!
//! # Contract
//!
//! * [`assemble`] is pure: per-MTU prices, per-MTU PV, `now` and [`HorizonParams`] in,
//!   `Vec<Slot>` out.
//! * The horizon starts at the MTU **containing** `now` and ends at the last published MTU.
//!   Nothing assumes 24 h, 96 MTUs or a fixed UTC offset: a DST day is 92 or 100 MTUs.
//! * Prices and PV are joined on [`Mtu::start`], never by position — the two sources are
//!   fetched independently and may be trimmed differently.
//! * `price_sell = spot − sell_fee`, `price_buy = buy_price`; `load_kw` is the plant's own
//!   draw (~0.09 kW), not the house.
//!
//! # Refresh policy (2026-11-06, supersedes 2026-11-05)
//!
//! Refresh is the actor's own business and never the caller's: [`HorizonActor::build`] never
//! fetches — it returns whatever window the cache holds, so the planner, which solves about
//! once a minute, reads the latest data without ever waiting on the network. Only an empty
//! cache is an error, and it is a distinct one (`NoPrices`), because "nothing fetched yet"
//! at boot is normal and must not read as a bug.
//!
//! Three properties the 2026-11-05 policy lacked, all of them measured on the Pi:
//!
//! * **Each feed has its own staleness.** Prices are stale when the window stops reaching
//!   `now + 24 h`; PV is stale after [`FORECAST_TTL_SECS`]. A shared check meant one stale
//!   feed dragged the other one to the network every minute.
//! * **A budget, not a tick.** forecast.solar's free tier answers 12 requests per hour per
//!   IP; at one attempt a minute the quota was spent in twelve minutes and the rest of the
//!   hour came back 429. [`REFRESH_FLOOR_SECS`] bounds the attempts, a failed attempt backs
//!   off exponentially, and a 429's own `retry-at` is honoured over all of it.
//! * **Commits are per source.** OTE and forecast.solar fail independently; a price fetch
//!   that succeeds lands even when the PV fetch is rate-limited. Otherwise one 429 hides the
//!   afternoon's newly published prices for the rest of the hour.

use std::collections::BTreeMap;

use jiff::Timestamp;
use thiserror::Error;

use crate::planner::Slot;
use crate::planner::TerminalValue;
use crate::planner::forecast::{ForecastError, PvPoint, PvSource};
use crate::planner::price::{PriceCache, PriceError, PricePoint, PriceSource};
use crate::units::{AcKw, CzkPerKwh, DcKw};

/// The day-ahead MTU length. Everything here is on this grid.
const MTU_SECS: i64 = 900;

/// How far back the price fetch reaches. One less than [`crate::planner::TERMINAL_DAYS`]
/// because the window includes today, and today is fetched anyway.
pub const PRICE_HISTORY_DAYS: i64 = crate::planner::TERMINAL_DAYS - 1;

/// Minimum seconds between two refresh attempts.
///
/// forecast.solar's free tier allows 12 requests per hour per IP. The planner ticks every
/// 60 s; five minutes is at most 12 attempts an hour, and PV is only fetched on its own TTL,
/// so the steady-state cost of the forecast is 2 requests an hour.
pub const REFRESH_FLOOR_SECS: i64 = 300;

/// How long a fetched PV forecast is reused. The source republishes hourly; asking more
/// often only spends quota.
pub const FORECAST_TTL_SECS: i64 = 1800;

/// Cap on the exponential backoff that follows a failed attempt (the floor is the base).
pub const RETRY_MAX_SECS: i64 = 1800;

/// What the horizon needs from boot config and directives.
#[derive(Debug, Clone, Copy)]
pub struct HorizonParams {
    /// Deducted from the spot price: `price_sell = spot − sell_fee`.
    pub sell_fee: CzkPerKwh,
    /// The all-in household tariff paid per kWh imported.
    pub buy_price: CzkPerKwh,
    /// The plant's own AC draw (inverter + Pi), constant across MTUs.
    pub load_kw: AcKw,
    /// Measured ÷ forecast, applied to every forecast kWh. A directive, because the
    /// ratio drifts with the seasons and the model does not.
    pub forecast_calibration: f64,
}

#[derive(Debug, Error)]
pub enum HorizonError {
    #[error("no price window has been fetched yet")]
    NoPrices,
    #[error("no published MTU at or after {now}")]
    NoFutureMtu { now: i64 },
    #[error("PV is forecast for {start}, an MTU that has no published price")]
    PvWithoutPrice { start: i64 },
    #[error("cannot place the clock: {0}")]
    Clock(String),
    #[error(transparent)]
    Price(#[from] PriceError),
    #[error(transparent)]
    Forecast(#[from] ForecastError),
}

/// The MTU containing `now`, on the quarter-hour grid of unix seconds.
pub fn current_mtu_start(now: i64) -> i64 {
    now - now.rem_euclid(MTU_SECS)
}

/// Cuts the horizon and assembles the slots. Pure.
///
/// A *priced* MTU without PV is a clear sky and becomes zero. A *forecast* MTU beyond the
/// last published price is **outside the horizon** and is ignored: the two sources refresh
/// on their own schedules, forecast.solar always answers a horizon of its own (today and
/// tomorrow), and OTE publishes tomorrow only around 13:10–14:00 — so a strict superset
/// check would withhold the whole plan every morning for the unpriced tail alone. A forecast
/// MTU with no price **inside** the priced window is still an error: that is a partial OTE
/// answer, and planning across the gap would silently read a hole as a clear sky.
pub fn assemble(
    prices: &[PricePoint],
    pv: &[PvPoint],
    now: i64,
    params: &HorizonParams,
) -> Result<Vec<Slot>, HorizonError> {
    let start = current_mtu_start(now);
    let priced: BTreeMap<i64, &PricePoint> = prices.iter().map(|p| (p.mtu.start, p)).collect();
    let forecast: BTreeMap<i64, &PvPoint> = pv.iter().map(|p| (p.mtu.start, p)).collect();

    if let Some(last_priced) = priced.keys().next_back().copied() {
        for mtu_start in forecast.keys().filter(|s| **s >= start && **s <= last_priced) {
            if !priced.contains_key(mtu_start) {
                return Err(HorizonError::PvWithoutPrice { start: *mtu_start });
            }
        }
    }

    let slots: Vec<Slot> = priced
        .range(start..)
        .map(|(mtu_start, price)| Slot {
            mtu: price.mtu,
            price_sell: CzkPerKwh::new(price.czk_per_kwh().get() - params.sell_fee.get()),
            price_buy: params.buy_price,
            pv_kw: forecast
                .get(mtu_start)
                .map(|point| point.dc_kwh / price.mtu.hours * params.forecast_calibration)
                .unwrap_or(DcKw::ZERO),
            load_kw: params.load_kw,
        })
        .collect();

    if slots.is_empty() {
        return Err(HorizonError::NoFutureMtu { now });
    }

    Ok(slots)
}

// Settled 2026-11-05: **the refresh is independent of the solve.** This actor owns its own
// timer (startup, the publication edge, the coverage check) and the planner consumes
// whatever window is current. Amended 2026-11-06: the timer is a budget, not a tick.

/// What one attempt managed, per source.
///
/// Not a `Result`, because a refresh is deliberately **not** atomic: OTE and forecast.solar
/// fail independently, and a price fetch that succeeded must be committed even when the PV
/// fetch did not. Sharing one outcome is what let a single 429 hide the afternoon's newly
/// published prices until the quota reset.
#[derive(Debug)]
pub struct RefreshReport {
    pub prices: Result<(), HorizonError>,
    pub forecast: Result<(), HorizonError>,
}

impl Default for RefreshReport {
    fn default() -> Self {
        Self {
            prices: Ok(()),
            forecast: Ok(()),
        }
    }
}

impl RefreshReport {
    /// True when both feeds were fetched, or were already fresh enough not to be.
    pub fn is_complete(&self) -> bool {
        self.prices.is_ok() && self.forecast.is_ok()
    }
}

/// Owns the two sources and the cache; hands out a horizon without ever fetching it.
pub struct HorizonActor<P: PriceSource, F: PvSource> {
    prices: P,
    pv: F,
    cache: PriceCache,
    forecast: Vec<PvPoint>,
    params: HorizonParams,
    /// Computed on each successful refresh, from the same fetched window — the terminal
    /// value is a price statistic, so it costs no extra request and no extra clock.
    terminal: Option<TerminalValue>,
    /// Earliest instant at which the next attempt may start. The rate budget lives here,
    /// not in the caller, so no call site can talk the actor into spending the quota.
    next_attempt_at: i64,
    /// Failed attempts since the last fully successful one; drives the backoff.
    failed_attempts: u32,
    /// When `forecast` was last fetched; 0 before the first success.
    forecast_fetched_at: i64,
}

impl<P: PriceSource, F: PvSource> HorizonActor<P, F> {
    /// Replaces the policy half of the horizon (prices, tariff, load, calibration).
    pub fn set_params(&mut self, params: HorizonParams) {
        self.params = params;
    }

    pub fn new(prices: P, pv: F, params: HorizonParams) -> Self {
        Self {
            prices,
            pv,
            cache: PriceCache::default(),
            forecast: Vec::new(),
            params,
            terminal: None,
            next_attempt_at: 0,
            failed_attempts: 0,
            forecast_fetched_at: 0,
        }
    }

    /// What a stored kWh is assumed salable at at the end of the horizon, as of the last
    /// refresh. `None` before any refresh has produced enough history.
    pub fn terminal(&self) -> Option<TerminalValue> {
        self.terminal
    }

    /// The horizon for `now` from whatever the cache currently holds. **Never fetches**:
    /// the caller must not wait on the network, and a stale window is legal.
    pub fn build(&self, now: i64) -> Result<Vec<Slot>, HorizonError> {
        if self.cache.points().is_empty() {
            return Err(HorizonError::NoPrices);
        }
        assemble(self.cache.points(), &self.forecast, now, &self.params)
    }

    /// How old the served window is, in seconds; 0 when nothing has been fetched.
    pub fn cache_age_secs(&self, now: i64) -> i64 {
        self.cache.age_secs(now)
    }

    /// True when the held window reaches far enough to be worth not refetching.
    pub fn is_fresh(&self, now: i64) -> bool {
        self.cache.covers(now, now + 24 * 3600)
    }

    /// True when the held PV forecast is younger than [`FORECAST_TTL_SECS`].
    pub fn forecast_is_fresh(&self, now: i64) -> bool {
        !self.forecast.is_empty() && now - self.forecast_fetched_at < FORECAST_TTL_SECS
    }

    /// True when at least one feed is older than its own TTL, so an attempt would fetch
    /// something. Says nothing about the budget — [`Self::refresh`] owns that.
    pub fn needs_refresh(&self, now: i64) -> bool {
        !self.is_fresh(now) || !self.forecast_is_fresh(now)
    }

    /// When the next attempt becomes due. Read by the driver's log and by the tests.
    pub fn next_attempt_at(&self) -> i64 {
        self.next_attempt_at
    }

    /// Fetches whatever is stale, committing each source on its own.
    ///
    /// `None` means the budget said "not yet": the caller is asking more often than
    /// [`REFRESH_FLOOR_SECS`] allows, or a 429 handed back a later `retry-at`. Nothing was
    /// fetched and nothing was committed.
    ///
    /// The window starts [`PRICE_HISTORY_DAYS`] before today, not at today: the terminal
    /// value is a statistic over the last [`crate::planner::TERMINAL_DAYS`] days, and this
    /// fetch is the only one there is. `build` ignores the past MTUs it brings back.
    pub async fn refresh(&mut self, now: i64) -> Option<RefreshReport> {
        if now < self.next_attempt_at {
            return None;
        }

        let mut report = RefreshReport::default();
        let mut retry_at: Option<i64> = None;

        let (from, to) = match self.fetch_window(now) {
            Ok(window) => window,
            Err(message) => {
                // The clock cannot be placed, so neither feed can be dated. Nothing to
                // commit; the planner keeps the window it holds.
                report.prices = Err(HorizonError::Clock(message.clone()));
                report.forecast = Err(HorizonError::Clock(message));
                self.schedule(now, None, true);
                return Some(report);
            }
        };

        if !self.is_fresh(now) {
            match self.prices.fetch(from, to).await {
                Ok(points) => {
                    self.terminal =
                        TerminalValue::measured(&points, now, self.params.sell_fee);
                    self.cache.replace(points, now);
                }
                Err(error) => report.prices = Err(error.into()),
            }
        }

        if !self.forecast_is_fresh(now) {
            match self.pv.fetch(from, to).await {
                Ok(points) => {
                    self.forecast = points;
                    self.forecast_fetched_at = now;
                }
                Err(error) => {
                    if let ForecastError::RateLimited { retry_at: at } = &error {
                        retry_at = *at;
                    }
                    report.forecast = Err(error.into());
                }
            }
        }

        self.schedule(now, retry_at, !report.is_complete());
        Some(report)
    }

    /// The civil dates one fetch needs: history through the end of tomorrow, and the PV
    /// window's own start (the source ignores it; it answers with its own horizon).
    fn fetch_window(&self, now: i64) -> Result<(jiff::civil::Date, jiff::civil::Date), String> {
        let zone = crate::planner::price::prague().map_err(|e| e.to_string())?;
        let today = Timestamp::from_second(now)
            .map_err(|e| format!("{now}: {e}"))?
            .to_zoned(zone)
            .date();
        // `EndDate` is exclusive, so `today..today+2` publishes today and tomorrow.
        let to = today
            .tomorrow()
            .and_then(|day| day.tomorrow())
            .map_err(|e| format!("{today} + 2 days: {e}"))?;
        let from = today
            .checked_sub(jiff::Span::new().days(PRICE_HISTORY_DAYS))
            .map_err(|e| format!("{today} - {PRICE_HISTORY_DAYS} days: {e}"))?;
        Ok((from, to))
    }

    /// Sets the next due instant: the floor, stretched by the consecutive-failure backoff,
    /// and never earlier than a 429's own `retry-at`.
    fn schedule(&mut self, now: i64, retry_at: Option<i64>, failed: bool) {
        self.failed_attempts = if failed {
            self.failed_attempts.saturating_add(1)
        } else {
            0
        };
        // 1x, 2x, 4x, 8x the floor, capped: 5 min → 10 → 20 → 30 min.
        let shift = self.failed_attempts.saturating_sub(1).min(3);
        let wait = (REFRESH_FLOOR_SECS << shift).min(RETRY_MAX_SECS);
        self.next_attempt_at = retry_at.map_or(now + wait, |at| (now + wait).max(at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::planner::Mtu;
    use crate::planner::forecast::ForecastError;
    use crate::units::{DcKwh, Hours};

    /// 2026-11-04T00:00 Prague (CET, UTC+1), quarter-hour aligned.
    const T0: i64 = 1_793_746_800;
    /// 2026-10-25T00:00 Prague (CEST, UTC+2) — a 25-hour civil day, 100 MTUs.
    const AUTUMN_DST: i64 = 1_792_879_200;
    /// 2026-03-29T00:00 Prague (CET, UTC+1) — a 23-hour civil day, 92 MTUs.
    const SPRING_DST: i64 = 1_774_738_800;

    fn mtu(start: i64) -> Mtu {
        Mtu {
            start,
            hours: Hours::new(0.25),
        }
    }

    fn price(start: i64, eur_per_mwh: f64) -> PricePoint {
        PricePoint {
            mtu: mtu(start),
            eur_per_mwh,
            eur_rate: 25.0,
        }
    }

    fn pv(start: i64, dc_kwh: f64) -> PvPoint {
        PvPoint {
            mtu: mtu(start),
            dc_kwh: DcKwh::new(dc_kwh),
        }
    }

    fn params() -> HorizonParams {
        HorizonParams {
            sell_fee: CzkPerKwh::new(0.5),
            buy_price: CzkPerKwh::new(5.0),
            load_kw: AcKw::new(0.09),
            forecast_calibration: 1.0,
        }
    }

    fn window(count: usize, eur_per_mwh: f64) -> Vec<PricePoint> {
        (0..count)
            .map(|k| price(T0 + k as i64 * 900, eur_per_mwh))
            .collect()
    }

    #[test]
    fn horizon_starts_at_the_mtu_containing_now() {
        let prices = window(4, 250.0);

        let aligned = assemble(&prices, &[], T0, &params()).expect("aligned now");
        assert_eq!(aligned.len(), 4);
        assert_eq!(aligned[0].mtu.start, T0);

        // 31 minutes in: the first two MTUs are past, and `now` sits inside the third.
        let mid = assemble(&prices, &[], T0 + 1900, &params()).expect("mid-window now");
        assert_eq!(mid.len(), 2);
        assert_eq!(
            mid[0].mtu.start,
            T0 + 1800,
            "the MTU containing now, not the next one"
        );
    }

    #[test]
    fn horizon_ends_at_the_last_published_mtu() {
        let prices = window(6, 250.0);

        let horizon = assemble(&prices, &[], T0 + 900, &params()).expect("assemble");

        assert_eq!(horizon.len(), 5);
        assert_eq!(
            horizon.last().expect("non-empty").mtu.start,
            T0 + 5 * 900,
            "the horizon stops where the published prices stop"
        );
    }

    #[test]
    fn prices_and_pv_are_joined_on_start_not_by_position() {
        let prices = window(3, 250.0);
        // Deliberately out of order: a positional zip would put the evening PV first.
        let pv = vec![pv(T0 + 1800, 0.5), pv(T0, 1.5), pv(T0 + 900, 1.0)];

        let horizon = assemble(&prices, &pv, T0, &params()).expect("assemble");

        // `PvPoint` carries energy, `Slot` carries power: 1.5 kWh in a quarter-hour is 6 kW.
        for (slot, kwh) in horizon.iter().zip([1.5, 1.0, 0.5]) {
            let want = kwh / 0.25;
            assert_eq!(slot.pv_kw.get(), want, "PV at MTU {}", slot.mtu.start);
        }
    }

    #[test]
    fn pv_beyond_the_last_published_price_is_ignored() {
        // The production failure of 2026-11-06: forecast.solar answers today *and* tomorrow,
        // OTE publishes tomorrow around 13:10–14:00. Before that the forecast is a strict
        // superset of the priced window, and the plan used to be withheld for the whole day.
        let prices = window(2, 250.0);
        let pv = vec![pv(T0, 1.0), pv(T0 + 900, 1.0), pv(T0 + 3600, 2.0)];

        let horizon = assemble(&prices, &pv, T0, &params()).expect("the tail is not fatal");

        assert_eq!(horizon.len(), 2, "the horizon ends where the prices do");
        assert_eq!(
            horizon.last().expect("non-empty").mtu.start,
            T0 + 900,
            "tomorrow's PV never becomes a slot"
        );
        assert!(
            (horizon[1].pv_kw.get() - 4.0).abs() < 1e-9,
            "the PV that *is* inside the window still lands: 1 kWh over a quarter is 4 kW"
        );
    }

    #[test]
    fn a_hole_inside_the_published_window_is_still_an_error() {
        // A partial OTE answer: prices for the first, second and fourth MTU, none for the
        // third, with PV forecast across the gap. Planning over it would read the hole as a
        // clear sky, so it stays a contract break — the 2026-11-06 fix only moved the edge.
        let prices = vec![
            price(T0, 250.0),
            price(T0 + 900, 250.0),
            price(T0 + 2700, 250.0),
        ];
        let pv = vec![pv(T0, 1.0), pv(T0 + 1800, 1.0)];

        let err = assemble(&prices, &pv, T0, &params()).expect_err("PV inside a hole");

        assert!(
            matches!(err, HorizonError::PvWithoutPrice { start } if start == T0 + 1800),
            "got {err:?}"
        );
    }

    #[test]
    fn a_dst_day_still_produces_its_own_count_of_mtus() {
        let autumn: Vec<PricePoint> = (0..100)
            .map(|k| price(AUTUMN_DST + k * 900, 250.0))
            .collect();
        let horizon = assemble(&autumn, &[], AUTUMN_DST, &params()).expect("25-hour day");
        assert_eq!(horizon.len(), 100, "a 25-hour day is 100 MTUs, never 96");
        assert_eq!(horizon.last().expect("non-empty").mtu.start, AUTUMN_DST + 99 * 900);

        let spring: Vec<PricePoint> = (0..92)
            .map(|k| price(SPRING_DST + k * 900, 250.0))
            .collect();
        let horizon = assemble(&spring, &[], SPRING_DST, &params()).expect("23-hour day");
        assert_eq!(horizon.len(), 92, "a 23-hour day is 92 MTUs");
    }

    #[test]
    fn sell_price_is_spot_minus_the_fee() {
        // 250 EUR/MWh at 25.0 CZK/EUR is 6.25 CZK/kWh; the 0.50 fee leaves 5.75.
        let prices = vec![price(T0, 250.0)];

        let horizon = assemble(&prices, &[], T0, &params()).expect("assemble");
        let slot = &horizon[0];

        assert!(
            (slot.price_sell.get() - 5.75).abs() < 1e-9,
            "spot minus fee, got {}",
            slot.price_sell
        );
        assert!((slot.price_buy.get() - 5.0).abs() < 1e-9, "the tariff is not discounted");
        assert!((slot.load_kw.get() - 0.09).abs() < 1e-9);
    }

    // --- the actor: refresh is its own business, build never fetches -------------------

    struct CountingPrices {
        points: Vec<PricePoint>,
        calls: Arc<AtomicUsize>,
    }

    impl PriceSource for CountingPrices {
        fn fetch(
            &self,
            _from: jiff::civil::Date,
            _to: jiff::civil::Date,
        ) -> impl Future<Output = Result<Vec<PricePoint>, PriceError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let points = self.points.clone();
            async move { Ok(points) }
        }
    }

    struct FlakyPrices {
        points: Vec<PricePoint>,
        fail: Arc<AtomicBool>,
    }

    impl PriceSource for FlakyPrices {
        fn fetch(
            &self,
            _from: jiff::civil::Date,
            _to: jiff::civil::Date,
        ) -> impl Future<Output = Result<Vec<PricePoint>, PriceError>> + Send {
            let failing = self.fail.load(Ordering::SeqCst);
            let points = self.points.clone();
            async move {
                if failing {
                    Err(PriceError::Network("down".into()))
                } else {
                    Ok(points)
                }
            }
        }
    }

    struct NoPv;

    impl PvSource for NoPv {
        fn fetch(
            &self,
            _from: jiff::civil::Date,
            _to: jiff::civil::Date,
        ) -> impl Future<Output = Result<Vec<PvPoint>, ForecastError>> + Send {
            async { Ok(Vec::new()) }
        }
    }

    struct CountingPv {
        points: Vec<PvPoint>,
        calls: Arc<AtomicUsize>,
    }

    impl PvSource for CountingPv {
        fn fetch(
            &self,
            _from: jiff::civil::Date,
            _to: jiff::civil::Date,
        ) -> impl Future<Output = Result<Vec<PvPoint>, ForecastError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let points = self.points.clone();
            async move { Ok(points) }
        }
    }

    /// A PV source that answers 429 with the server's own retry time while switched on.
    struct RateLimitedPv {
        points: Vec<PvPoint>,
        failing: Arc<AtomicBool>,
        retry_at: Option<i64>,
        calls: Arc<AtomicUsize>,
    }

    impl PvSource for RateLimitedPv {
        fn fetch(
            &self,
            _from: jiff::civil::Date,
            _to: jiff::civil::Date,
        ) -> impl Future<Output = Result<Vec<PvPoint>, ForecastError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let failing = self.failing.load(Ordering::SeqCst);
            let points = self.points.clone();
            let retry_at = self.retry_at;
            async move {
                if failing {
                    Err(ForecastError::RateLimited { retry_at })
                } else {
                    Ok(points)
                }
            }
        }
    }

    #[tokio::test]
    async fn build_serves_the_cache_and_never_fetches() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut actor = HorizonActor::new(
            CountingPrices {
                points: window(4, 250.0),
                calls: Arc::clone(&calls),
            },
            NoPv,
            params(),
        );

        assert!(
            matches!(actor.build(T0), Err(HorizonError::NoPrices)),
            "an unfetched window is its own state, not a missing MTU"
        );

        let report = actor.refresh(T0).await.expect("the first attempt is due");
        assert!(report.is_complete(), "both feeds answered");

        let horizon = actor.build(T0).expect("build serves the cache");
        assert_eq!(horizon.len(), 4);
        let later = actor.build(T0 + 900).expect("build serves the cache again");
        assert_eq!(later.len(), 3);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "build must never go to the network"
        );
    }

    #[tokio::test]
    async fn a_failed_price_fetch_keeps_serving_the_previous_window() {
        let fail = Arc::new(AtomicBool::new(false));
        let mut actor = HorizonActor::new(
            FlakyPrices {
                points: window(4, 250.0),
                fail: Arc::clone(&fail),
            },
            NoPv,
            params(),
        );

        actor.refresh(T0).await.expect("first fetch");
        fail.store(true, Ordering::SeqCst);
        let report = actor
            .refresh(T0 + REFRESH_FLOOR_SECS)
            .await
            .expect("the second attempt is due after the floor");

        assert!(
            matches!(
                report.prices,
                Err(HorizonError::Price(PriceError::Network(_)))
            ),
            "got {:?}",
            report.prices
        );

        let horizon = actor
            .build(T0)
            .expect("a stale window is still a horizon");
        assert_eq!(horizon.len(), 4, "the old window survives a failed refresh");
    }

    #[tokio::test]
    async fn a_rate_limited_forecast_still_commits_fresh_prices() {
        // The exact production shape of 2026-11-06: OTE answers (tomorrow published), the
        // PV call is 429ed. The prices must land anyway, or the planner keeps failing on a
        // horizon it could already build.
        let pv_calls = Arc::new(AtomicUsize::new(0));
        let retry_at = T0 + 1200;
        let mut actor = HorizonActor::new(
            CountingPrices {
                points: window(96, 250.0),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            RateLimitedPv {
                points: Vec::new(),
                failing: Arc::new(AtomicBool::new(true)),
                retry_at: Some(retry_at),
                calls: Arc::clone(&pv_calls),
            },
            params(),
        );

        let report = actor.refresh(T0).await.expect("the first attempt is due");

        assert!(report.prices.is_ok(), "prices were fetched and committed");
        assert!(
            matches!(report.forecast, Err(HorizonError::Forecast(
                ForecastError::RateLimited { retry_at: Some(at) }
            )) if at == retry_at),
            "got {:?}",
            report.forecast
        );
        assert!(
            actor.is_fresh(T0),
            "the price cache took the successful fetch"
        );
        assert_eq!(
            actor.build(T0).expect("a plan-sized horizon").len(),
            96,
            "the horizon is available the moment the prices are"
        );
        assert!(
            actor.next_attempt_at() >= retry_at,
            "the server's retry time is honoured over the floor, got {}",
            actor.next_attempt_at()
        );
        assert!(
            actor.refresh(T0 + 600).await.is_none(),
            "no attempt before the retry time"
        );
    }

    #[tokio::test]
    async fn refresh_is_not_attempted_more_often_than_the_floor() {
        let pv_calls = Arc::new(AtomicUsize::new(0));
        let mut actor = HorizonActor::new(
            CountingPrices {
                points: window(96, 250.0),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            CountingPv {
                points: vec![pv(T0, 1.0)],
                calls: Arc::clone(&pv_calls),
            },
            params(),
        );

        actor.refresh(T0).await.expect("the first attempt is due");
        assert!(
            actor.refresh(T0 + 60).await.is_none(),
            "a 60 s tick must not spend a request"
        );
        assert_eq!(pv_calls.load(Ordering::SeqCst), 1, "one PV request, not two");
        assert!(
            actor.refresh(T0 + REFRESH_FLOOR_SECS).await.is_some(),
            "the floor has passed"
        );
    }

    #[tokio::test]
    async fn the_forecast_has_its_own_ttl() {
        let price_calls = Arc::new(AtomicUsize::new(0));
        let pv_calls = Arc::new(AtomicUsize::new(0));
        // Two full turns of the clock, so only the forecast can go stale in this test.
        let mut actor = HorizonActor::new(
            CountingPrices {
                points: window(200, 250.0),
                calls: Arc::clone(&price_calls),
            },
            CountingPv {
                points: vec![pv(T0, 1.0)],
                calls: Arc::clone(&pv_calls),
            },
            params(),
        );

        actor.refresh(T0).await.expect("the first attempt is due");

        assert!(!actor.needs_refresh(T0 + 60), "both feeds are young");
        assert!(
            actor.needs_refresh(T0 + FORECAST_TTL_SECS),
            "the PV forecast is stale on its own clock"
        );

        actor
            .refresh(T0 + FORECAST_TTL_SECS)
            .await
            .expect("an attempt is due");

        assert_eq!(
            price_calls.load(Ordering::SeqCst),
            1,
            "a day of prices is not refetched for an hourly forecast"
        );
        assert_eq!(pv_calls.load(Ordering::SeqCst), 2);
    }

    /// A live check, not part of the default run:
    /// `cargo test --lib -- --ignored live_horizon`.
    ///
    /// The 2026-11-06 production failure against the real feeds: forecast.solar answers
    /// today *and* tomorrow, while OTE has today only until the early afternoon. Before the
    /// fix this returned `PvWithoutPrice` for tomorrow's first MTU and the planner published
    /// nothing all morning; now the horizon simply ends at the last published price.
    #[tokio::test]
    #[ignore = "hits OTE and forecast.solar"]
    async fn live_horizon_ends_at_the_last_published_price() {
        use crate::planner::forecast::{ForecastSolar, Site};
        use crate::planner::price::OteClient;

        let now = crate::telemetry::unix_now() as i64;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .expect("client");
        let site = Site {
            latitude: 50.08,
            longitude: 14.45,
            declination: 37.0,
            azimuth: 200.0,
            kwp: 16.5,
        };
        let mut actor = HorizonActor::new(
            OteClient::new(http.clone()),
            ForecastSolar::new(http, site),
            params(),
        );

        let report = actor.refresh(now).await.expect("the first attempt is due");
        assert!(report.prices.is_ok(), "prices: {:?}", report.prices);
        assert!(report.forecast.is_ok(), "forecast: {:?}", report.forecast);

        let slots = actor.build(now).expect("a plan-sized horizon");
        let last = slots.last().expect("a non-empty horizon");
        eprintln!(
            "live horizon: {} MTUs, {} .. {} (now {now})",
            slots.len(),
            slots[0].mtu.start,
            last.mtu.start
        );
        assert!(
            last.mtu.start <= now + 48 * 3600,
            "the horizon ends at the last published price, never past it"
        );
    }

    #[tokio::test]
    async fn repeated_failures_back_off_until_a_success_resets_them() {
        let fail = Arc::new(AtomicBool::new(true));
        let mut actor = HorizonActor::new(
            FlakyPrices {
                points: window(4, 250.0),
                fail: Arc::clone(&fail),
            },
            NoPv,
            params(),
        );

        actor.refresh(T0).await.expect("the first attempt is due");
        assert_eq!(
            actor.next_attempt_at(),
            T0 + REFRESH_FLOOR_SECS,
            "one failure waits the floor"
        );

        let second = T0 + REFRESH_FLOOR_SECS;
        actor.refresh(second).await.expect("due");
        assert_eq!(
            actor.next_attempt_at(),
            second + 2 * REFRESH_FLOOR_SECS,
            "two consecutive failures double the wait"
        );

        fail.store(false, Ordering::SeqCst);
        let third = second + 2 * REFRESH_FLOOR_SECS;
        actor.refresh(third).await.expect("due");
        assert_eq!(
            actor.next_attempt_at(),
            third + REFRESH_FLOOR_SECS,
            "a success drops the backoff back to the floor"
        );
    }

    #[tokio::test]
    async fn the_refresh_measures_the_terminal_value_and_keeps_history_out_of_the_horizon() {
        // A week of history (the widened fetch) plus the four MTUs of the horizon, all at
        // the same price so the median is that price.
        let mut points = Vec::new();
        for day in -6..=0 {
            for k in 0..4 {
                points.push(price(T0 + day * 86_400 + k * 900, 250.0));
            }
        }
        points.extend(window(4, 250.0));

        let mut actor = HorizonActor::new(
            CountingPrices {
                points,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            NoPv,
            params(),
        );

        assert!(
            actor.terminal().is_none(),
            "nothing is measured before the first fetch — the planner keeps its bootstrap"
        );

        actor.refresh(T0).await.expect("the first attempt is due");

        let terminal = actor.terminal().expect("a full week of prices measures one");
        assert!(
            (terminal.czk_per_kwh.get() - 5.75).abs() < 1e-9,
            "250 EUR/MWh at 25 CZK/EUR is 6.25 CZK/kWh, minus the 0.5 fee; got {}",
            terminal.czk_per_kwh.get()
        );

        let horizon = actor.build(T0).expect("build");
        assert_eq!(
            horizon.len(),
            4,
            "the week of history the fetch brought back is not part of the horizon"
        );
        assert_eq!(
            horizon[0].mtu.start, T0,
            "the horizon still starts at now, not six days ago"
        );
    }
}
