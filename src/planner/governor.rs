//! The governor: a measured-state safety filter between the plan and the bus.
//!
//! # Why it exists
//!
//! The DP treats `soc_ceiling` as a hard bound, but the plant does not honour it: a hold
//! runs `zero_export_load`, where the TOU target gates grid charge and the discharge floor
//! **not PV charge**, so the pack charges to BMS-full behind a commanded hold (measured
//! 2026-11-15: 80 → 100 % in 67 minutes). The ceiling was a property of the *plan*, which
//! inherits the forecast's error, not of the *closed loop*. The governor makes it a
//! closed-loop invariant: it keys on the **measured** SOC, so it is symmetric to forecast
//! error — an under-forecast peak that arrives unannounced is spilled just as an
//! over-forecast one is.
//!
//! # The spill primitive (validated by history, not a probe)
//!
//! `selling_first` with every slot at the ceiling and a 0 W discharge cap holds the pack
//! and exports PV surplus. Measured 2026-11-07 with all six slots at 100 % and 0 W: the pack
//! sat flat at 21 % while PV produced 12.35 kW and the meter took 11.90 kW — the mode sells
//! the PV and never charges the pack. Writing **all six** targets equal removes the
//! next-slot lookahead that dumped the pack on 2026-11-02 (current target 100, next slot 20,
//! pack drained to 20 before the boundary). With no lower target to find, the pack holds.
//!
//! # The reserve (forecast-timed headroom)
//!
//! The export cap (the 12 kW interface) is the binding constraint: a full pack closes the
//! DC charge sink, so PV above the cap curtails. Headroom is the buffer that absorbs the
//! above-cap surplus, and it is a *stock* prepared before the peak — discharging during
//! saturation only steals export capacity from PV. The reserve keeps the pack structurally
//! lower through the solar window so a peak the forecast missed still finds room.
//!
//! The window comes from the **PV forecast's timing**, not a fixed clock band: the span of
//! today's MTUs where forecast PV is a meaningful fraction of the day's peak (see
//! [`reserve_window`]). The forecast's magnitude is unreliable but its sunrise/sunset timing
//! is sun geometry, which is accurate — so the window tracks the actual solar day across the
//! seasons without a knob. Only the depth (`reserve_headroom_kwh`) is policy; 0 disables the
//! reserve (the November default), summer deepens it. The spill above stays purely
//! measured-state and forecast-agnostic.

use jiff::Timestamp;

use crate::planner::PlanStep;
use crate::planner::price::prague;

/// What the governor does with the plan's current command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Govern {
    /// The plan stands.
    FollowPlan,
    /// Hold the pack at the ceiling and export PV surplus (`selling_first`, all slots at
    /// the ceiling, 0 W discharge). Replaces a hold or a buy that would charge past it.
    Spill,
    /// Pre-peak: discharge toward `ceiling − reserve` to open headroom for the coming peak.
    ReserveSell { kw: f64 },
}

/// The governor's slice of policy, from [`crate::directives::Directives`] plus device facts.
#[derive(Debug, Clone, Copy)]
pub struct GovernorParams {
    pub soc_floor_pct: f64,
    pub soc_ceiling_pct: f64,
    pub capacity_kwh: f64,
    pub battery_discharge_kw: f64,
    pub reserve_headroom_kwh: f64,
    /// The solar window `[start, end)` in unix seconds, derived from the forecast PV timing
    /// for `now`'s day ([`reserve_window`]). `None` when the day has no meaningful PV.
    pub reserve_window: Option<(i64, i64)>,
}

/// Engage spill half a percent below the ceiling. A 60 s tick at the pack's charge rate
/// moves ~0.4 % SOC, so this catches the pack before it crosses the ceiling rather than
/// after. Stateless hysteresis: spill holds the pack (0 W discharge), so it does not flap at
/// the boundary, and a planned sell still drains it.
const SPILL_MARGIN_PCT: f64 = 0.5;

/// Decide from the measured SOC, whether the plan is already selling, and the clock.
///
/// `plan_selling` is true when the plan's current MTU commands a positive export: a sell
/// drains the pack, which is always allowed (it moves SOC away from the ceiling, never past
/// it). The governor only overrides commands that would charge or hold at the ceiling.
pub fn decide(soc_pct: f64, plan_selling: bool, now: i64, p: &GovernorParams) -> Govern {
    let cap = p.soc_ceiling_pct;

    // At the ceiling: forbid charging past it. A sell drains (allow); a hold or buy would
    // charge to BMS-full, so spill instead.
    if soc_pct >= cap - SPILL_MARGIN_PCT {
        return if plan_selling {
            Govern::FollowPlan
        } else {
            Govern::Spill
        };
    }

    // Below the ceiling, in the reserve window: keep headroom for a peak the forecast may
    // miss. A plan already selling drains toward the floor faster than the reserve needs,
    // so let it; otherwise discharge toward the reserve target.
    if p.reserve_headroom_kwh > 0.0 && in_reserve_window(now, p) {
        let reserve_soc = p.reserve_headroom_kwh / p.capacity_kwh * 100.0;
        let target = (cap - reserve_soc).max(p.soc_floor_pct);
        if soc_pct > target {
            return if plan_selling {
                Govern::FollowPlan
            } else {
                Govern::ReserveSell {
                    kw: p.battery_discharge_kw,
                }
            };
        }
    }

    Govern::FollowPlan
}

/// Whether `now` falls in the forecast-derived reserve window `[start, end)`.
fn in_reserve_window(now: i64, p: &GovernorParams) -> bool {
    p.reserve_window
        .is_some_and(|(start, end)| now >= start && now < end)
}

/// The solar window of `now`'s Prague day, read from the forecast PV **timing**: from the
/// first to the last MTU whose forecast PV is at least a small fraction of the day's peak.
///
/// The fraction (5 %) is relative to the day's own forecast peak, so it adapts across the
/// seasons and ignores the sunrise noise and the sunset sliver — both a tiny fraction of the
/// peak — without an absolute threshold to retune. The forecast's magnitude may be wrong, but
/// the hours it puts the sun above the horizon are sun geometry and are right, which is all
/// the window needs. The horizon starts at the MTU containing `now`, so the window naturally
/// begins no earlier than `now`: the reserve prepares headroom forward from the present, never
/// retroactively. `None` when the day carries no meaningful PV (night, deep winter, or an
/// empty forecast), which leaves the reserve dormant.
pub fn reserve_window(steps: &[PlanStep], now: i64) -> Option<(i64, i64)> {
    let zone = prague().ok()?;
    let today = Timestamp::from_second(now).ok()?.to_zoned(zone.clone()).date();
    let on_today = |step: &PlanStep| {
        Timestamp::from_second(step.mtu.start)
            .ok()
            .is_some_and(|t| t.to_zoned(zone.clone()).date() == today)
    };

    let peak = steps
        .iter()
        .filter(|step| on_today(step))
        .map(|step| step.pv_kw.get())
        .fold(0.0_f64, f64::max);
    if peak <= 0.0 {
        return None;
    }
    let threshold = 0.05 * peak;

    let first = steps
        .iter()
        .find(|step| on_today(step) && step.pv_kw.get() >= threshold)?;
    let last = steps
        .iter()
        .rev()
        .find(|step| on_today(step) && step.pv_kw.get() >= threshold)?;
    let start = first.mtu.start;
    let end = last.mtu.start + (last.mtu.hours.get() * 3600.0).round() as i64;
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-16T12:00 Prague (CET).
    const NOON: i64 = 1_794_826_800;
    // 2026-11-16T20:00 Prague.
    const EVENING: i64 = NOON + 8 * 3600;
    const HOUR: i64 = 3600;

    fn params() -> GovernorParams {
        GovernorParams {
            soc_floor_pct: 20.0,
            soc_ceiling_pct: 80.0,
            capacity_kwh: 32.0,
            battery_discharge_kw: 8.25,
            reserve_headroom_kwh: 0.0,
            // A solar window [08:00, 16:00) around NOON; EVENING falls outside it.
            reserve_window: Some((NOON - 4 * HOUR, NOON + 4 * HOUR)),
        }
    }

    #[test]
    fn below_the_ceiling_with_no_reserve_follows_the_plan() {
        assert_eq!(decide(50.0, false, NOON, &params()), Govern::FollowPlan);
    }

    #[test]
    fn at_the_ceiling_a_hold_spills() {
        assert_eq!(decide(80.0, false, NOON, &params()), Govern::Spill);
        // The margin catches the pack just below the ceiling before a tick overshoots.
        assert_eq!(decide(79.6, false, NOON, &params()), Govern::Spill);
    }

    #[test]
    fn at_the_ceiling_a_planned_sell_is_allowed() {
        // Evening peak: the pack is at the ceiling and the plan wants to drain it. The
        // governor must not block the discharge.
        assert_eq!(decide(80.0, true, EVENING, &params()), Govern::FollowPlan);
    }

    #[test]
    fn above_the_ceiling_a_hold_spills_to_stop_further_charge() {
        assert_eq!(decide(99.0, false, NOON, &params()), Govern::Spill);
    }

    #[test]
    fn reserve_discharges_toward_the_target_in_the_window() {
        let mut p = params();
        p.reserve_headroom_kwh = 10.0; // 31.25 % of 32 kWh -> target 48.75 %
        // 60 % at noon is above the 48.75 % target: open headroom.
        assert_eq!(
            decide(60.0, false, NOON, &p),
            Govern::ReserveSell { kw: 8.25 }
        );
        // Already at the target: no override.
        assert_eq!(decide(48.0, false, NOON, &p), Govern::FollowPlan);
        // A plan already selling drains faster than the reserve needs: let it.
        assert_eq!(decide(60.0, true, NOON, &p), Govern::FollowPlan);
    }

    #[test]
    fn reserve_is_inactive_outside_the_window() {
        let mut p = params();
        p.reserve_headroom_kwh = 10.0;
        // EVENING is past the window's end: the reserve does not force a discharge into the
        // evening peak the plan wants to sell into.
        assert_eq!(decide(60.0, false, EVENING, &p), Govern::FollowPlan);
    }

    #[test]
    fn zero_reserve_is_dormant() {
        // The November default: no reserve, the curtailment price handles predicted peaks.
        assert_eq!(decide(60.0, false, NOON, &params()), Govern::FollowPlan);
    }

    #[test]
    fn reserve_target_never_drops_below_the_floor() {
        let mut p = params();
        p.reserve_headroom_kwh = 30.0; // 93.75 % -> target would be -13.75 %, clamped to floor
        // SOC above the floor still discharges toward the floor, not below it.
        assert_eq!(
            decide(50.0, false, NOON, &p),
            Govern::ReserveSell { kw: 8.25 }
        );
        // At the floor the reserve cannot push further down.
        assert_eq!(decide(20.0, false, NOON, &p), Govern::FollowPlan);
    }

    // The forecast-derived window.
    use crate::planner::{Action, Mtu, Reason};
    use crate::units::{AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};

    fn step(start: i64, pv_kw: f64) -> PlanStep {
        PlanStep {
            mtu: Mtu {
                start,
                hours: Hours::new(1.0),
            },
            price_sell: CzkPerKwh::new(5.0),
            price_buy: CzkPerKwh::new(5.0),
            pv_kw: DcKw::new(pv_kw),
            action: Action::Idle,
            battery_out_kwh: DcKwh::ZERO,
            ac_kwh: AcKwh::ZERO,
            energy_after_kwh: DcKwh::new(16.0),
            net_czk: Czk::ZERO,
            marginal_czk_per_kwh: CzkPerKwh::ZERO,
            reason: Reason::Idle,
        }
    }

    // A solar day: sunrise noise at 07:00 (0.2 kW), production 08:00–16:00, a 10 kW noon
    // peak, and a sunset sliver at 17:00 (0.3 kW). 5 % of the peak is 0.5 kW, so both slivers
    // fall below the threshold and neither stretches the window.
    fn solar_day() -> Vec<PlanStep> {
        vec![
            step(NOON - 6 * HOUR, 0.0),
            step(NOON - 5 * HOUR, 0.2),
            step(NOON - 4 * HOUR, 2.0),
            step(NOON, 10.0),
            step(NOON + 4 * HOUR, 2.0),
            step(NOON + 5 * HOUR, 0.3),
            step(NOON + 6 * HOUR, 0.0),
        ]
    }

    #[test]
    fn window_spans_the_significant_pv_of_today() {
        // First significant MTU 08:00, last 16:00 (its end 17:00): [08:00, 17:00).
        assert_eq!(
            reserve_window(&solar_day(), NOON),
            Some((NOON - 4 * HOUR, NOON + 5 * HOUR))
        );
    }

    #[test]
    fn window_is_the_same_before_sunrise_on_the_same_day() {
        // At 06:00 the window is still today's; it lies ahead, so `decide` will not fire yet.
        let dawn = NOON - 6 * HOUR;
        assert_eq!(
            reserve_window(&solar_day(), dawn),
            Some((NOON - 4 * HOUR, NOON + 5 * HOUR))
        );
        let mut p = params();
        p.reserve_headroom_kwh = 10.0;
        p.reserve_window = reserve_window(&solar_day(), dawn);
        assert_eq!(decide(60.0, false, dawn, &p), Govern::FollowPlan);
    }

    #[test]
    fn window_ignores_tomorrows_pv() {
        let mut steps = solar_day();
        steps.push(step(NOON + 24 * HOUR, 16.0)); // a bigger peak tomorrow
        assert_eq!(
            reserve_window(&steps, NOON),
            Some((NOON - 4 * HOUR, NOON + 5 * HOUR))
        );
    }

    #[test]
    fn window_is_none_without_meaningful_pv_today() {
        let night = vec![step(NOON - 6 * HOUR, 0.0), step(NOON, 0.0)];
        assert_eq!(reserve_window(&night, NOON), None);
        assert_eq!(reserve_window(&[], NOON), None);
    }
}
