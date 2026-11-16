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
//! # The reserve (climatological headroom)
//!
//! The export cap (the 12 kW interface) is the binding constraint: a full pack closes the
//! DC charge sink, so PV above the cap curtails. Headroom is the buffer that absorbs the
//! above-cap surplus, and it is a *stock* prepared before the peak — discharging during
//! saturation only steals export capacity from PV. The reserve keeps the pack structurally
//! lower through the solar window so a peak the forecast missed still finds room. The window
//! is sun geometry (climatology), not weather: it is predictable even when clouds are not.
//! `reserve_headroom_kwh = 0` disables it (the November default); summer deepens it.

use jiff::Timestamp;

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
    pub reserve_start_hour: f64,
    pub reserve_end_hour: f64,
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

/// Whether `now` falls in the climatological reserve window `[start, end)` local hours.
/// A window with `start >= end` is disabled (no wrap-around: the solar window is daytime).
fn in_reserve_window(now: i64, p: &GovernorParams) -> bool {
    if p.reserve_start_hour >= p.reserve_end_hour {
        return false;
    }
    let zone = prague().expect("the Prague zone is compiled in");
    let hour = f64::from(Timestamp::from_second(now).expect("plan timestamps are in range").to_zoned(zone).hour());
    hour >= p.reserve_start_hour && hour < p.reserve_end_hour
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> GovernorParams {
        GovernorParams {
            soc_floor_pct: 20.0,
            soc_ceiling_pct: 80.0,
            capacity_kwh: 32.0,
            battery_discharge_kw: 8.25,
            reserve_headroom_kwh: 0.0,
            reserve_start_hour: 8.0,
            reserve_end_hour: 16.0,
        }
    }

    // 2026-11-16T12:00 Prague (CET).
    const NOON: i64 = 1_794_826_800;
    // 2026-11-16T20:00 Prague.
    const EVENING: i64 = NOON + 8 * 3600;

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
        // 20:00 is outside [8, 16): the reserve does not force a discharge into the evening.
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
}
