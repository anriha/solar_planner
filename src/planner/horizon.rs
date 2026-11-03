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
//! # Refresh policy (2026-11-05)
//!
//! Refresh is the actor's own business and never the caller's: it runs at startup, at the
//! 13:10 Prague publication edge, and on a coverage check against `now .. now + 24 h`.
//! [`HorizonActor::build`] never fetches — it returns whatever window the cache holds, so
//! the planner, which solves about once a minute, reads the latest data without ever
//! waiting on the network. A stale window is served with its age; only an empty cache is
//! an error, and it is a distinct one (`NoPrices`), because "nothing fetched yet" at boot
//! is normal and must not read as a bug.

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
/// A *priced* MTU without PV is a clear sky and becomes zero; a *forecast* MTU without a
/// price means the two sources disagree about the horizon, and that is an error. The PV
/// check only looks at MTUs at or after the horizon start — a forecast that still covers
/// the hours already past is not a disagreement.
pub fn assemble(
    prices: &[PricePoint],
    pv: &[PvPoint],
    now: i64,
    params: &HorizonParams,
) -> Result<Vec<Slot>, HorizonError> {
    let start = current_mtu_start(now);
    let priced: BTreeMap<i64, &PricePoint> = prices.iter().map(|p| (p.mtu.start, p)).collect();
    let forecast: BTreeMap<i64, &PvPoint> = pv.iter().map(|p| (p.mtu.start, p)).collect();

    for mtu_start in forecast.keys().filter(|s| **s >= start) {
        if !priced.contains_key(mtu_start) {
            return Err(HorizonError::PvWithoutPrice { start: *mtu_start });
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
// timer (startup, the 13:10 publication edge, the coverage check) and the planner consumes
// whatever window is current. Wire the timer in `on_start` when this becomes a kameo actor.

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

    /// Fetches a fresh window and replaces the cache. An error leaves the cache untouched,
    /// so a failed refresh never costs the planner data it already had.
    ///
    /// The window starts [`PRICE_HISTORY_DAYS`] before today, not at today: the terminal
    /// value is a statistic over the last [`crate::planner::TERMINAL_DAYS`] days, and this
    /// fetch is the only one there is. `build` ignores the past MTUs it brings back.
    pub async fn refresh(&mut self, now: i64) -> Result<(), HorizonError> {
        let zone = crate::planner::price::prague()?;
        let today = Timestamp::from_second(now)
            .map_err(|e| HorizonError::Clock(format!("{now}: {e}")))?
            .to_zoned(zone)
            .date();
        // `EndDate` is exclusive, so `today..today+2` publishes today and tomorrow.
        let to = today
            .tomorrow()
            .and_then(|day| day.tomorrow())
            .map_err(|e| HorizonError::Clock(format!("{today} + 2 days: {e}")))?;
        let from = today
            .checked_sub(jiff::Span::new().days(PRICE_HISTORY_DAYS))
            .map_err(|e| HorizonError::Clock(format!("{today} - {PRICE_HISTORY_DAYS} days: {e}")))?;

        let prices = self.prices.fetch(from, to).await?;
        let forecast = self.pv.fetch(today, to).await?;

        self.terminal = TerminalValue::measured(&prices, now, self.params.sell_fee);
        self.cache.replace(prices, now);
        self.forecast = forecast;
        Ok(())
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
    fn pv_for_an_unpriced_mtu_is_an_error() {
        let prices = window(2, 250.0);
        let pv = vec![pv(T0, 1.0), pv(T0 + 900, 1.0), pv(T0 + 3600, 2.0)];

        let err = assemble(&prices, &pv, T0, &params()).expect_err("PV with no price");

        assert!(
            matches!(err, HorizonError::PvWithoutPrice { start } if start == T0 + 3600),
            "a forecast the prices do not cover is a contract break, got {err:?}"
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

        actor.refresh(T0).await.expect("refresh");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

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
    async fn a_failed_refresh_keeps_serving_the_previous_window() {
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
        let err = actor.refresh(T0).await.expect_err("the second fetch fails");
        assert!(
            matches!(err, HorizonError::Price(PriceError::Network(_))),
            "got {err:?}"
        );

        let horizon = actor
            .build(T0)
            .expect("a stale window is still a horizon");
        assert_eq!(horizon.len(), 4, "the old window survives a failed refresh");
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

        actor.refresh(T0).await.expect("refresh");

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
