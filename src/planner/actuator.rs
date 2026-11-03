//! Plan → TOU slots.
//!
//! # Settled 2026-11-06 (mode A)
//!
//! The planner projects its 15-minute plan onto the inverter's six wall-clock TOU slots on
//! **every tick** and rewrites whatever disagrees. Rewriting mid-slot is safe on this unit —
//! verified by hand — so the slots are a projection of the plan, not a schedule.
//!
//! The slots are a **rolling window**, not a day plan: the first slot carries the segment
//! the clock is inside, the rest carry the segments that follow, and a slot that has run out
//! is reused for the next segment. When the plan has more segments than slots, the tail
//! simply waits — it is written when a slot frees up. Nothing is merged or coarsened to fit.
//!
//! # What each register means (verified live, `wiki entities/deye-inverter-energy-model`)
//!
//! * 148–153 are slot **start** times; slot N runs to `time_{N+1}`, slot 6 to 24:00. Slot 1
//!   is pinned to 00:00 so the day is covered.
//! * 166–171 is the per-slot target SOC: above the current SOC the battery is **held**,
//!   at or below it the battery may discharge down to it. This is also the on-grid
//!   discharge floor, which is why the planner's `soc_floor` is written here.
//! * 154–159 caps discharge power per slot — this is the "slow versus burst" lever.
//! * 172–177 picks the charge source per slot; Import blocks use `grid`.
//! * 143 caps export in watts; 145 enables PV export (on only while selling, so a hold
//!   does not depend on `max_sell_power = 0` meaning "no export"); 146 bit 0 is the TOU
//!   master.
//!
//! # Landmine this module exists to avoid
//!
//! `selling_first` with a target SOC *above* the pack exported 9.5–9.8 kW from the battery
//! at night, in three separate tests, stopping only at the next slot's target. So the mode
//! is armed **only while the current block is selling**, and every hold block pins its
//! target SOC to 100.

use std::collections::BTreeMap;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::decode::Value;
use crate::planner::price::prague;
use crate::planner::{Action, Plan, PlanStep};

/// The inverter has six TOU slots.
pub const SLOTS: usize = 6;
/// Register 143 accepts 0–16000 W; the interface rating is the real bound.
const MAX_SELL_W: f64 = 12_000.0;
/// Pack nominal voltage, for the amp conversion the charger wants.
const PACK_VOLTS: f64 = 51.2;
/// Register 128's ceiling (185 A), which is also below the 200 A BMS limit. Asking for more
/// is rejected by the map constraint, so clamping here is what keeps a large import from
/// aborting the whole batch.
const MAX_CHARGE_A: f64 = 185.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActuatorParams {
    pub soc_floor_pct: f64,
    pub soc_ceiling_pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charge {
    Off,
    Grid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// `HH:MM`, wall clock — the register's own format.
    pub start: String,
    pub soc: u8,
    pub power_w: u16,
    pub charge: Charge,
}

/// Everything one tick wants the inverter to be.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub blocks: Vec<Block>,
    pub selling: bool,
    /// Solar sell is enabled only while selling. In a hold it must be off: the mode alone
    /// does not stop PV export when solar sell is on (manual §5.7), and relying on
    /// `max_sell_power = 0` to mean "no export" is an assumption we do not need.
    pub solar_sell: bool,
    pub max_sell_w: u16,
    pub grid_charge: bool,
    pub charge_current_a: u16,
    /// The commanded import exceeded reg 128's 185 A ceiling and was clamped.
    pub charge_current_limited: bool,
    /// The import the plan asked for, before the clamp — logged when limited.
    pub charge_requested_kw: f64,
}

/// One register the actuator may write, with the value to write and the decoded value that
/// means "already correct".
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub name: String,
    pub write: serde_json::Value,
    pub expect: Value,
}

/// What the plan wants, for the MTU sequence starting at `now`.
pub fn desired(plan: &Plan, now: i64, params: &ActuatorParams) -> Desired {
    let zone = prague().expect("the Prague zone is compiled in");
    // The six slots are one civil day. Segments after midnight belong to tomorrow's
    // schedule and must not be written as today's HH:MM (2026-11-06).
    let segments = segments(&plan.steps, now, next_midnight(now, &zone));

    let current = segments.first().map(|s| s.kind).unwrap_or(Kind::Hold);
    let selling = matches!(current, Kind::Sell { .. });

    // Slot 1 is the segment the clock is inside; the rest are the ones after it. A segment
    // that has not been written yet keeps waiting — the tail of a long plan is not dropped,
    // it is simply not scheduled until a slot frees up.
    let mut blocks: Vec<Block> = segments
        .iter()
        .take(SLOTS)
        .map(|segment| block(hhmm(segment.start, &zone), segment.kind, params))
        .collect();

    if blocks.is_empty() {
        blocks.push(block(hhmm(now, &zone), Kind::Hold, params));
    }
    // Unused slots repeat the last setting one minute apart, so the inverter never falls back
    // to whatever schedule preceded the planner. Near midnight they pile up on 23:59 — the
    // settings are identical, so a duplicate start cannot change what the inverter does.
    while blocks.len() < SLOTS {
        let last = blocks.last().expect("non-empty").clone();
        let start = bump(&last.start, 1).unwrap_or_else(|| "23:59".to_string());
        blocks.push(Block { start, ..last });
    }

    let (max_sell_w, charge_current_a, charge_current_limited, charge_requested_kw) = match current {
        Kind::Sell { kw } => ((kw * 1000.0).round() as u16, 0, false, 0.0),
        Kind::Buy { kw } => {
            let wanted = (kw * 1000.0) / PACK_VOLTS;
            (
                0,
                wanted.round().min(MAX_CHARGE_A) as u16,
                wanted > MAX_CHARGE_A,
                kw,
            )
        }
        Kind::Hold => (0, 0, false, 0.0),
    };

    Desired {
        blocks,
        selling,
        solar_sell: selling,
        max_sell_w: max_sell_w.min(MAX_SELL_W as u16),
        grid_charge: matches!(current, Kind::Buy { .. }),
        charge_current_a,
        charge_current_limited,
        charge_requested_kw,
    }
}

/// Unix seconds of the next local midnight: the end of the civil day the slots describe.
fn next_midnight(now: i64, zone: &TimeZone) -> i64 {
    let today = Timestamp::from_second(now)
        .expect("planner timestamps are in range")
        .to_zoned(zone.clone())
        .date();
    today
        .tomorrow()
        .expect("the calendar does not end this year")
        .at(0, 0, 0, 0)
        .to_zoned(zone.clone())
        .expect("local midnight is never ambiguous")
        .timestamp()
        .as_second()
}

/// `HH:MM` advanced by `by` minutes, or `None` if that would pass 23:59.
fn bump(start: &str, by: usize) -> Option<String> {
    let (hour, minute) = start.split_once(':')?;
    let total = hour.parse::<usize>().ok()? * 60 + minute.parse::<usize>().ok()? + by;
    if total > 23 * 60 + 59 {
        return None;
    }
    Some(format!("{:02}:{:02}", total / 60, total % 60))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Sell { kw: f64 },
    Buy { kw: f64 },
    Hold,
}

fn kind_of(step: &PlanStep) -> Kind {
    match step.action {
        Action::Export { kw } if kw.get() > 0.0 => Kind::Sell { kw: kw.get() },
        Action::Import { kw } if kw.get() > 0.0 => Kind::Buy { kw: kw.get() },
        _ => Kind::Hold,
    }
}

fn block(start: String, kind: Kind, params: &ActuatorParams) -> Block {
    let floor = params.soc_floor_pct.round().clamp(0.0, 100.0) as u8;
    let ceiling = params.soc_ceiling_pct.round().clamp(0.0, 100.0) as u8;
    match kind {
        // Discharge is allowed only down to the floor — the same number the DP uses.
        Kind::Sell { kw } => Block {
            start,
            soc: floor,
            power_w: (kw * 1000.0).round().clamp(0.0, MAX_SELL_W) as u16,
            charge: Charge::Off,
        },
        // Hold: a target above the pack blocks discharge (the grid carries standby in
        // zero_export_load) and the charge source is Off, so the 100 target charges nothing
        // from the grid — it only stops the pack from discharging.
        Kind::Hold => Block {
            start,
            soc: 100,
            power_w: 0,
            charge: Charge::Off,
        },
        // Charge up to the ceiling, from the grid, within the pack's power limit.
        Kind::Buy { .. } => Block {
            start,
            soc: ceiling,
            power_w: 0,
            charge: Charge::Grid,
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    start: i64,
    kind: Kind,
}

/// Consecutive MTUs with the *same* action — same direction and same power — form one
/// segment. A change of setpoint starts a new segment; nothing is merged across one.
fn segments(steps: &[PlanStep], now: i64, day_end: i64) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for step in steps {
        // The plan's own MTU length, not a hardcoded fifteen minutes.
        let end = step.mtu.start + (step.mtu.hours.get() * 3600.0).round() as i64;
        if end <= now {
            continue;
        }
        // Tomorrow is a different schedule: the six registers are wall-clock slots inside
        // one civil day.
        if step.mtu.start >= day_end {
            break;
        }
        let kind = kind_of(step);
        match out.last_mut() {
            Some(last) if same_actions(last.kind, kind) => {}
            _ => out.push(Segment {
                start: step.mtu.start,
                kind,
            }),
        }
    }
    out
}

fn same_actions(a: Kind, b: Kind) -> bool {
    match (a, b) {
        (Kind::Sell { kw }, Kind::Sell { kw: other }) => (kw - other).abs() < 1e-9,
        (Kind::Buy { kw }, Kind::Buy { kw: other }) => (kw - other).abs() < 1e-9,
        (Kind::Hold, Kind::Hold) => true,
        _ => false,
    }
}

/// The wider of two same-direction runs: a run that ever sold at 6 kW stays a 6 kW run.
/// How much a boundary is worth keeping. Same-direction boundaries are about magnitude;
/// a change of direction is worth more than any price gap.
fn hhmm(start: i64, zone: &TimeZone) -> String {
    let zoned = Timestamp::from_second(start)
        .expect("planner timestamps are in range")
        .to_zoned(zone.clone());
    format!("{:02}:{:02}", zoned.hour(), zoned.minute())
}

/// `work_mode` option values from the map (`selling_first`, `zero_export_load`, …).
const WORK_MODE_SELLING_FIRST: u64 = 0;
const WORK_MODE_ZERO_EXPORT_LOAD: u64 = 1;

/// Every register the desired state touches, in write order.
pub fn targets(desired: &Desired) -> Vec<Target> {
    let mut out = Vec::new();
    let switch = |name: &str, on: bool| Target {
        name: name.to_string(),
        write: serde_json::json!(on),
        expect: Value::Number(if on { 1.0 } else { 0.0 }),
    };

    // The slots first: the mode must never be armed against the previous schedule.
    out.push(switch("tou_enabled", true));
    for (index, block) in desired.blocks.iter().enumerate() {
        let n = index + 1;
        out.push(Target {
            name: format!("tou_time_{n}"),
            write: serde_json::json!(block.start),
            expect: Value::Text(block.start.clone()),
        });
        out.push(Target {
            name: format!("tou_soc_{n}"),
            write: serde_json::json!(block.soc),
            expect: Value::Number(f64::from(block.soc)),
        });
        out.push(Target {
            name: format!("tou_power_{n}"),
            write: serde_json::json!(block.power_w),
            expect: Value::Number(f64::from(block.power_w)),
        });
        let (value, label) = match block.charge {
            Charge::Off => (0, "off"),
            Charge::Grid => (1, "grid"),
        };
        out.push(Target {
            name: format!("tou_charge_source_{n}"),
            write: serde_json::json!(value),
            expect: Value::Text(label.to_string()),
        });
    }

    // The current before its enable: a rejected current must not leave charging armed.
    if desired.grid_charge {
        out.push(Target {
            name: "grid_charge_current".to_string(),
            write: serde_json::json!(desired.charge_current_a),
            expect: Value::Number(f64::from(desired.charge_current_a)),
        });
    }
    out.push(switch("grid_charge_enable", desired.grid_charge));
    out.push(Target {
        name: "max_sell_power".to_string(),
        write: serde_json::json!(desired.max_sell_w),
        expect: Value::Number(f64::from(desired.max_sell_w)),
    });
    out.push(switch("solar_sell", desired.solar_sell));
    out.push(Target {
        name: "work_mode".to_string(),
        write: serde_json::json!(if desired.selling {
            WORK_MODE_SELLING_FIRST
        } else {
            WORK_MODE_ZERO_EXPORT_LOAD
        }),
        expect: Value::Text(
            if desired.selling {
                "selling_first"
            } else {
                "zero_export_load"
            }
            .to_string(),
        ),
    });

    // Arming selling is the last write; disarming it is the first. Rewriting a hold slot
    // while `selling_first` is still armed is the exact combination the module documents as
    // a landmine, so the order flips with the direction of the transition.
    if !desired.selling {
        let mode = out.pop().expect("work_mode is last");
        out.insert(0, mode);
    }
    out
}

/// The safe state's global registers: nothing sold, nothing bought, the pack held.
///
/// Used by the disarm path and by the shutdown handler, where the slot registers are either
/// rewritten separately or left inert by the mode.
pub fn disarm_globals() -> Vec<Target> {
    let switch = |name: &str, on: bool| Target {
        name: name.to_string(),
        write: serde_json::json!(on),
        expect: Value::Number(if on { 1.0 } else { 0.0 }),
    };
    vec![
        Target {
            name: "work_mode".to_string(),
            write: serde_json::json!(WORK_MODE_ZERO_EXPORT_LOAD),
            expect: Value::Text("zero_export_load".to_string()),
        },
        Target {
            name: "max_sell_power".to_string(),
            write: serde_json::json!(0),
            expect: Value::Number(0.0),
        },
        switch("solar_sell", false),
        switch("grid_charge_enable", false),
        Target {
            name: "grid_charge_current".to_string(),
            write: serde_json::json!(0),
            expect: Value::Number(0.0),
        },
    ]
}

/// The complete safe state: [`disarm_globals`] plus every slot held at 100 % with no
/// discharge and no charge source. The slot times are left alone — with all six targets at
/// 100 % and zero power, any schedule is inert.
pub fn disarm_targets() -> Vec<Target> {
    let mut out = disarm_globals();
    out.push(Target {
        name: "tou_enabled".to_string(),
        write: serde_json::json!(true),
        expect: Value::Number(1.0),
    });
    for n in 1..=SLOTS {
        out.push(Target {
            name: format!("tou_soc_{n}"),
            write: serde_json::json!(100),
            expect: Value::Number(100.0),
        });
        out.push(Target {
            name: format!("tou_power_{n}"),
            write: serde_json::json!(0),
            expect: Value::Number(0.0),
        });
        out.push(Target {
            name: format!("tou_charge_source_{n}"),
            write: serde_json::json!(0),
            expect: Value::Text("off".to_string()),
        });
    }
    out
}

/// What a process shutdown writes: the globals only. There is no time budget for twenty
/// more slot registers, and the mode plus a zero export cap already stops the money.
pub fn shutdown_targets() -> Vec<Target> {
    disarm_globals()
}

/// The subset of `targets` the inverter does not already show. Reads are the authority —
/// there is no shadow state, so an HA or LCD change is corrected rather than remembered.
///
/// A target the snapshot does not contain is an **error**, not a write: it means the map and
/// the actuator disagree about a register name, and writing blind would repeat every tick.
pub fn writes_for(
    targets: &[Target],
    observed: &BTreeMap<String, Value>,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    let mut out = Vec::new();
    for target in targets {
        let Some(seen) = observed.get(&target.name) else {
            return Err(format!(
                "register '{}' is not in the observed snapshot",
                target.name
            ));
        };
        if seen != &target.expect {
            out.push((target.name.clone(), target.write.clone()));
        }
    }
    Ok(out)
}

/// [`writes_for`] over a [`Desired`].
pub fn writes(
    desired: &Desired,
    observed: &BTreeMap<String, Value>,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    writes_for(&targets(desired), observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{Mtu, PlanStep, Reason};
    use crate::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKwh, Hours};

    /// 2026-11-04T00:00 Prague.
    const T0: i64 = 1_793_746_800;

    fn step(index: i64, action: Action) -> PlanStep {
        PlanStep {
            mtu: Mtu {
                start: T0 + index * 900,
                hours: Hours::new(0.25),
            },
            action,
            battery_out_kwh: DcKwh::ZERO,
            ac_kwh: AcKwh::ZERO,
            energy_after_kwh: DcKwh::new(20.0),
            net_czk: Czk::ZERO,
            marginal_czk_per_kwh: CzkPerKwh::new(5.0),
            reason: Reason::Idle,
        }
    }

    fn plan(actions: Vec<Action>) -> Plan {
        plan_at(T0, actions)
    }

    /// A plan whose first MTU starts at `start` (the `plan` helper always starts at T0).
    fn plan_at(start: i64, actions: Vec<Action>) -> Plan {
        Plan {
            computed_at: start,
            steps: actions
                .into_iter()
                .enumerate()
                .map(|(index, action)| PlanStep {
                    mtu: Mtu {
                        start: start + index as i64 * 900,
                        hours: Hours::new(0.25),
                    },
                    ..step(0, action)
                })
                .collect(),
            expected_net_czk: Czk::ZERO,
            horizon_end: start + 86_400,
        }
    }

    fn params() -> ActuatorParams {
        ActuatorParams {
            soc_floor_pct: 20.0,
            soc_ceiling_pct: 80.0,
        }
    }

    #[test]
    fn the_current_block_is_slot_one_at_midnight() {
        let desired = desired(
            &plan(vec![Action::Export { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        );

        assert_eq!(desired.blocks.len(), SLOTS);
        assert_eq!(desired.blocks[0].start, "00:00");
        assert_eq!(desired.blocks[0].soc, 20, "discharge is allowed only to the floor");
        assert_eq!(desired.blocks[0].power_w, 6000);
        assert!(desired.selling);
        assert!(desired.solar_sell, "selling needs solar sell on");
        assert_eq!(desired.max_sell_w, 6000);
    }

    #[test]
    fn a_hold_block_pins_the_battery_at_full() {
        let desired = desired(&plan(vec![Action::Idle]), T0, &params());

        assert_eq!(desired.blocks[0].soc, 100, "a target above the pack holds it");
        assert_eq!(desired.blocks[0].power_w, 0);
        assert!(!desired.selling, "never leave selling_first armed while holding");
        assert!(
            !desired.solar_sell,
            "a hold must not rely on max_sell = 0 to stop PV export"
        );
        assert_eq!(desired.max_sell_w, 0);
    }

    #[test]
    fn a_buy_block_charges_from_the_grid_within_the_pack_limit() {
        let desired = desired(&plan(vec![Action::Import { kw: AcKw::new(6.0) }]), T0, &params());

        assert_eq!(desired.blocks[0].charge, Charge::Grid);
        assert_eq!(desired.blocks[0].soc, 80, "charge to the ceiling, no further");
        assert!(desired.grid_charge);
        assert_eq!(desired.charge_current_a, 117, "6 kW at 51.2 V");
        assert!(!desired.charge_current_limited);
    }

    #[test]
    fn an_import_above_the_charger_limit_is_clamped_to_185_a() {
        let desired = desired(&plan(vec![Action::Import { kw: AcKw::new(12.0) }]), T0, &params());

        assert_eq!(desired.charge_current_a, 185);
        assert!(desired.charge_current_limited);
        assert_eq!(desired.charge_requested_kw, 12.0);
    }

    /// Ten distinct one-hour segments: the plan is longer than the six slots can hold.
    fn long_plan() -> Plan {
        let mut actions = Vec::new();
        for index in 0..10 {
            for _ in 0..4 {
                actions.push(Action::Export {
                    kw: AcKw::new(1.0 + index as f64),
                });
            }
        }
        plan(actions)
    }

    #[test]
    fn the_slots_carry_the_next_six_segments_and_the_tail_waits() {
        let desired = desired(&long_plan(), T0, &params());

        assert_eq!(desired.blocks.len(), SLOTS);
        assert_eq!(desired.blocks[0].start, "00:00");
        assert_eq!(desired.blocks[0].power_w, 1000);
        assert_eq!(desired.blocks[1].start, "01:00");
        assert_eq!(desired.blocks[1].power_w, 2000);
        assert_eq!(
            desired.blocks[5].power_w, 6000,
            "six slots hold the first six segments; nothing is merged to fit"
        );
    }

    #[test]
    fn a_spent_slot_comes_back_for_the_next_segment() {
        // One hour in: segment 1 is over, so slot 1 carries segment 2 and the window slides.
        let desired = desired(&long_plan(), T0 + 3600, &params());

        assert_eq!(desired.blocks[0].start, "01:00");
        assert_eq!(desired.blocks[0].power_w, 2000);
        assert_eq!(
            desired.blocks[5].power_w, 7000,
            "the seventh segment takes the slot the first one freed"
        );
    }

    #[test]
    fn only_registers_that_disagree_are_written() {
        let desired = desired(&plan(vec![Action::Export { kw: AcKw::new(6.0) }]), T0, &params());
        let all = targets(&desired);

        // Everything already correct except the one register we care about.
        let mut observed: BTreeMap<String, Value> = all
            .iter()
            .map(|target| (target.name.clone(), target.expect.clone()))
            .collect();
        assert!(
            writes(&desired, &observed).unwrap().is_empty(),
            "nothing to do"
        );

        observed.insert("max_sell_power".to_string(), Value::Number(12000.0));
        let changed = writes(&desired, &observed).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, "max_sell_power");
        assert_eq!(changed[0].1, serde_json::json!(6000));
    }

    #[test]
    fn a_register_missing_from_the_snapshot_is_an_error_not_a_write() {
        let desired = desired(&plan(vec![Action::Export { kw: AcKw::new(6.0) }]), T0, &params());
        let observed: BTreeMap<String, Value> = BTreeMap::new();

        let error = writes(&desired, &observed).expect_err("a missing register must be loud");
        assert!(
            error.contains("not in the observed snapshot"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_plan_past_midnight_does_not_write_tomorrows_times_as_todays() {
        // 23:50, one segment still today and a hold that starts tomorrow. The six registers
        // are one civil day, so tomorrow's segment must wait for the next tick instead of
        // being written as a 00:00 slot before tonight's 23:45 one.
        let start = T0 + 23 * 3600 + 45 * 60;
        // A hold through midnight, then a buy at 01:00. Without the civil-day cut the buy
        // would be written as a 00:00 slot — before tonight's 23:45 one — and grid charge
        // would be armed for the rest of the day.
        let mut actions = vec![Action::Idle; 5];
        actions.push(Action::Import { kw: AcKw::new(12.0) });
        let desired = desired(&plan_at(start, actions), start + 5 * 60, &params());

        assert_eq!(desired.blocks[0].start, "23:45");
        assert_eq!(desired.blocks[0].soc, 100);
        assert!(
            desired
                .blocks
                .iter()
                .all(|block| block.start.as_str() >= "23:45"),
            "a tomorrow block leaked into today: {:?}",
            desired.blocks.iter().map(|b| &b.start).collect::<Vec<_>>()
        );
        assert!(
            desired
                .blocks
                .iter()
                .all(|block| block.soc == 100 && block.charge == Charge::Off),
            "tomorrow's buy leaked into today's slots: {:?}",
            desired.blocks
        );
        assert!(!desired.grid_charge);
        assert!(!desired.selling);
    }

    #[test]
    fn the_disarm_state_sells_nothing_and_holds_every_slot() {
        let targets = disarm_targets();
        let by_name = |name: &str| {
            targets
                .iter()
                .find(|target| target.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };

        assert_eq!(by_name("work_mode").expect, Value::Text("zero_export_load".into()));
        assert_eq!(by_name("max_sell_power").expect, Value::Number(0.0));
        assert_eq!(by_name("solar_sell").expect, Value::Number(0.0));
        assert_eq!(by_name("grid_charge_enable").expect, Value::Number(0.0));
        assert_eq!(by_name("tou_enabled").expect, Value::Number(1.0));
        for n in 1..=SLOTS {
            assert_eq!(by_name(&format!("tou_soc_{n}")).expect, Value::Number(100.0));
            assert_eq!(by_name(&format!("tou_power_{n}")).expect, Value::Number(0.0));
            assert_eq!(
                by_name(&format!("tou_charge_source_{n}")).expect,
                Value::Text("off".into())
            );
        }
        // The safe state is written before anything else in the list.
        assert_eq!(targets[0].name, "work_mode");
    }

    #[test]
    fn a_mid_day_start_skips_the_past_but_keeps_the_anchor() {
        let steps = vec![
            step(0, Action::Export { kw: AcKw::new(6.0) }),
            step(1, Action::Idle),
            step(2, Action::Idle),
        ];
        // Two MTUs in: the export block is already past, the hold is current.
        let desired = desired(&plan(steps_to_actions(&steps)), T0 + 1800, &params());

        assert_eq!(
            desired.blocks[0].start, "00:30",
            "slot 1 is the segment the clock is inside, not midnight"
        );
        assert_eq!(desired.blocks[0].soc, 100, "the current block is the hold");
        assert!(!desired.selling);
    }

    fn steps_to_actions(steps: &[PlanStep]) -> Vec<Action> {
        steps.iter().map(|step| step.action).collect()
    }
}
