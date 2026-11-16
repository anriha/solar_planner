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
//! * 143 caps export in watts; 145 enables PV export, 146 bit 0 is the TOU master.
//!
//! # A hold sells the surplus but never sells from the pack (2026-11-08, supersedes
//! 2026-11-06)
//!
//! A hold used to write `solar_sell = 0` and `max_sell_power = 0`, which curtailed every kWh
//! of PV the pack could not absorb. The 2026-11-06 fix routed the hold through
//! `selling_first` instead, which traded that defect for a worse one: **in `selling_first`
//! the plant sells the PV and never charges the pack.** Measured 2026-11-07 with all six
//! slots carrying target SOC 100 % and a 0 W discharge cap: the pack sat flat at 21 % while
//! PV produced 12.35 kW and the meter took 11.90 kW. Across 11-07 00:00 → 11-08 17:00 every
//! charge above 1.5 kW (3 153 samples) happened in `zero_export_load` and every discharge
//! above 1.5 kW (639 samples) in `selling_first` — the mode is the pack's direction valve.
//!
//! So a hold runs in `zero_export_load` with `solar_sell = 1` and `max_sell_power` at the
//! interface rating: PV charges the pack first and only the surplus leaves, which is what the
//! DP models for an `Idle` MTU. `selling_first` is reserved for a sell — the only mode in
//! which the pack may reach the meter. The slot target is the **ceiling**, not 100 %:
//! `solve` clamps the measured energy into `[floor, ceiling]`, so a pack charged past the
//! ceiling would be invisible to the planner.
//!
//! # The slot grid (2026-11-06, second fix)
//!
//! The inverter stores a TOU start time only on a **5-minute grid**. Writing `19:31` is
//! refused and the register keeps `19:30`, so the padding below emits grid-aligned starts
//! only, and a refused start leaves that one slot as it was instead of disarming the plant.
//! Before this, every plan whose remaining civil day had fewer than six segments padded with
//! `last + 1 minute`, the write was refused, the batch stopped, the fail-closed path
//! disarmed (`max_sell_power = 0`, `solar_sell = 0`, `zero_export_load`) and the next tick
//! re-armed: `max_sell_power` visibly flipped 10800 ↔ 0 about every other minute and roughly
//! half of the evening peak was never sold.
//!
//! # Landmine this module exists to avoid
//!
//! `selling_first` with a target SOC *above* the pack exported 9.5–9.8 kW from the battery
//! at night, in three separate tests, stopping only at the next slot's target. Every hold
//! block therefore pins its target SOC to the ceiling **and** its discharge cap to 0 W, and
//! the mode is disarmed before the slots are rewritten and armed again only afterwards, so
//! the pack is never inside a slot rewrite with export live.

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
/// The inverter stores a TOU start only on a 5-minute grid: a value like `19:31` is refused
/// and the register keeps its previous value (measured 2026-11-06 — every `tou_time_*`
/// refusal in a 134-minute log was `written = read back + 1 minute`).
const SLOT_GRID_MINUTES: usize = 5;
/// The last start the grid offers before midnight; used when `bump` runs past 23:59.
const LAST_GRID_START: &str = "23:55";
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
    /// `Directives::battery_charge_kw`: the highest DC power the pack may charge at. The
    /// plan is already bounded by it; this clamps the current register to the same rate so
    /// the charger is never asked for more than the operator allows.
    pub battery_charge_kw: f64,
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
    /// The pack may reach the meter (`work_mode = selling_first`): true only for a sell.
    /// Measured 2026-11-07/08, the mode is the pack's direction valve — every discharge
    /// above 1.5 kW happened in `selling_first` and no charge ever did.
    pub sell_from_pack: bool,
    /// PV export (`solar_sell`): true for a sell **and** for a hold, because a hold sells the
    /// surplus the pack cannot take. Off for a grid-charge block, which curtails its surplus.
    pub sell_surplus: bool,
    pub max_sell_w: u16,
    pub grid_charge: bool,
    pub charge_current_a: u16,
    /// The commanded import exceeded the pack's charge rate or reg 128's 185 A ceiling and
    /// was clamped.
    pub charge_current_limited: bool,
    /// The binding ceiling in amps: the lower of the configured pack rate and 185 A.
    pub charge_limit_a: u16,
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
    // A hold is a decision about the **pack** ("do not discharge, do not buy"), never an
    // instruction to give the sun away — but it must not run `selling_first`, where the plant
    // sells the PV and leaves the pack flat (2026-11-07). Only a sell arms the pack's export.
    let sell_from_pack = matches!(current, Kind::Sell { .. });
    // A hold still sells the surplus the pack cannot take; only a grid-charge block curtails.
    let sell_surplus = matches!(current, Kind::Sell { .. } | Kind::Hold);

    // Slot 1 is the segment the clock is inside; the rest are the ones after it. A segment
    // that has not been written yet keeps waiting — the tail of a long plan is not dropped,
    // it is simply not scheduled until a slot frees up.
    let mut blocks: Vec<Block> = segments
        .iter()
        .take(SLOTS)
        .map(|segment| block(hhmm(segment.start, &zone), segment.kind, params))
        .collect();

    if blocks.is_empty() {
        blocks.push(block(on_grid(&hhmm(now, &zone)), Kind::Hold, params));
    }
    // Unused slots repeat the last setting one grid step apart, so the inverter never falls
    // back to whatever schedule preceded the planner. The step must be a multiple of the
    // inverter's 5-minute grid: a start it cannot store is refused, the batch stops there and
    // the fail-closed path disarms the whole plant (2026-11-06). Near midnight the grid runs
    // out and the last slots pile up on 23:55 — the settings are identical, so a duplicate
    // start cannot change what the inverter does.
    while blocks.len() < SLOTS {
        let last = blocks.last().expect("non-empty").clone();
        let start = bump(&last.start, SLOT_GRID_MINUTES)
            .map(|start| on_grid(&start))
            .unwrap_or_else(|| LAST_GRID_START.to_string());
        blocks.push(Block { start, ..last });
    }

    let (max_sell_w, charge_current_a, charge_current_limited, charge_limit_a, charge_requested_kw) =
        match current {
            Kind::Sell { kw } => ((kw * 1000.0).round() as u16, 0, false, 0, 0.0),
            Kind::Buy { kw } => {
                // The plan's AC import as pack amps, at the nominal voltage. Two ceilings bind:
                // the operator's DC charge rate (`battery_charge_kw`) and reg 128's own 185 A.
                // The DP already prices against the rate, so this clamp only catches the AC/DC
                // conversion gap — but without it a 6.1 kW charge rate would still write 126 A.
                let wanted = (kw * 1000.0) / PACK_VOLTS;
                let limit = (params.battery_charge_kw * 1000.0 / PACK_VOLTS).min(MAX_CHARGE_A);
                (
                    0,
                    wanted.min(limit).round() as u16,
                    wanted > limit,
                    limit.round() as u16,
                    kw,
                )
            }
            // A hold exports whatever the pack cannot take, so the export cap is the interface
            // rating, not zero. Nothing leaves the pack: the slot's own power cap is 0 W, so this
            // number only lets the PV surplus through.
            Kind::Hold => (MAX_SELL_W as u16, 0, false, 0, 0.0),
        };

    Desired {
        blocks,
        sell_from_pack,
        sell_surplus,
        max_sell_w: max_sell_w.min(MAX_SELL_W as u16),
        grid_charge: matches!(current, Kind::Buy { .. }),
        charge_current_a,
        charge_current_limited,
        charge_limit_a,
        charge_requested_kw,
    }
}

/// The governor's spill: hold the pack at the ceiling and export PV surplus.
///
/// `selling_first` with **every** slot at the ceiling and a 0 W discharge cap. Measured
/// 2026-11-07 (all six slots at 100 %, 0 W): the pack sat flat while PV exported — the mode
/// sells the PV and never charges the pack. All six targets equal removes the next-slot
/// lookahead that dumped the pack on 2026-11-02, so the pack holds at the ceiling instead of
/// charging to BMS-full as a `zero_export_load` hold does. The slot times are the current grid
/// step onward; the targets are identical, so a duplicate start near midnight is inert.
pub fn spill_desired(now: i64, params: &ActuatorParams) -> Desired {
    let zone = prague().expect("the Prague zone is compiled in");
    let ceiling = params.soc_ceiling_pct.round().clamp(0.0, 100.0) as u8;
    let mut blocks = Vec::with_capacity(SLOTS);
    let mut start = on_grid(&hhmm(now, &zone));
    for _ in 0..SLOTS {
        blocks.push(Block {
            start: start.clone(),
            soc: ceiling,
            power_w: 0,
            charge: Charge::Off,
        });
        start = bump(&start, SLOT_GRID_MINUTES)
            .map(|start| on_grid(&start))
            .unwrap_or_else(|| LAST_GRID_START.to_string());
    }
    Desired {
        blocks,
        // selling_first: the pack may reach the meter, but the 0 W slot caps block discharge
        // and the ceiling target holds it. PV surplus exports through solar_sell/max_sell.
        sell_from_pack: true,
        sell_surplus: true,
        max_sell_w: MAX_SELL_W as u16,
        grid_charge: false,
        charge_current_a: 0,
        charge_current_limited: false,
        charge_limit_a: 0,
        charge_requested_kw: 0.0,
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

/// `HH:MM` floored to the inverter's slot grid.
///
/// A start off the grid can never be stored, so no such value may leave this module. The
/// fallback only fires on a malformed string, which `hhmm` and `bump` do not produce.
fn on_grid(start: &str) -> String {
    let Some((hour, minute)) = start.split_once(':') else {
        return "00:00".to_string();
    };
    let (Ok(hour), Ok(minute)) = (hour.parse::<usize>(), minute.parse::<usize>()) else {
        return "00:00".to_string();
    };
    let total = (hour * 60 + minute) / SLOT_GRID_MINUTES * SLOT_GRID_MINUTES;
    format!("{:02}:{:02}", total / 60, total % 60)
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
        // Hold: charge to the ceiling and stop there. The plant fills to the slot target, and
        // the DP's energy grid ends at the ceiling (`solve` clamps the measured SOC into it),
        // so a target of 100 would store energy the planner cannot see. The 0 W discharge cap
        // is what keeps the pack out of the trade — including a pack already above the
        // ceiling, which cannot bleed down through a 0 W slot.
        Kind::Hold => Block {
            start,
            soc: ceiling,
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
///
/// Two ordering rules that keep the documented landmine shut (with `selling_first` armed the
/// pack discharged 9.5–9.8 kW toward whatever target the *previous* slot carried):
///
/// * the discharge cap of every slot is written **before** its target SOC, so a hold's
///   0 W cap lands while the old SOC target is still in force and a sell's new SOC target
///   lands after its cap — the pack can never be mid-rewrite with discharge live and a
///   target that permits it;
/// * `work_mode` is written first when it is being disarmed and last when it is being
///   armed, so the slots are rewritten while nothing is armed or nothing can leave.
pub fn targets(desired: &Desired) -> Vec<Target> {
    let mut out = Vec::new();
    let switch = |name: &str, on: bool| Target {
        name: name.to_string(),
        write: serde_json::json!(on),
        expect: Value::Number(if on { 1.0 } else { 0.0 }),
    };
    let work_mode = |label: &str, value: u64| Target {
        name: "work_mode".to_string(),
        write: serde_json::json!(value),
        expect: Value::Text(label.to_string()),
    };

    // The slots: the mode must never be armed against the previous schedule.
    out.push(switch("tou_enabled", true));
    for (index, block) in desired.blocks.iter().enumerate() {
        let n = index + 1;
        out.push(Target {
            name: format!("tou_time_{n}"),
            write: serde_json::json!(block.start),
            expect: Value::Text(block.start.clone()),
        });
        // The cap before the target: see the ordering rules on this function.
        out.push(Target {
            name: format!("tou_power_{n}"),
            write: serde_json::json!(block.power_w),
            expect: Value::Number(f64::from(block.power_w)),
        });
        out.push(Target {
            name: format!("tou_soc_{n}"),
            write: serde_json::json!(block.soc),
            expect: Value::Number(f64::from(block.soc)),
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
    out.push(switch("solar_sell", desired.sell_surplus));
    let mode = if desired.sell_from_pack {
        work_mode("selling_first", WORK_MODE_SELLING_FIRST)
    } else {
        work_mode("zero_export_load", WORK_MODE_ZERO_EXPORT_LOAD)
    };
    if desired.sell_from_pack {
        // Armed last: every slot and every cap is in place before the pack may export.
        out.push(mode);
    } else {
        // Disarmed first: nothing can leave the pack while the slots are rewritten.
        out.insert(0, mode);
    }
    out
}

/// Whether a process shutdown may write the safe state at all.
///
/// A deploy with the actuator off must leave the registers exactly as it found them: the
/// planner never wrote, so there is nothing to undo. On 2026-11-06 a restart with
/// `actuator_enabled = false` wrote `work_mode = zero_export_load`, `max_sell_power = 0` and
/// `solar_sell = 0` on the way out, which is a write nobody asked for. An **unreadable**
/// directive state still disarms: unknown is not the same as off, and leaving a plant that
/// was being driven mid-schedule is the worse failure.
pub fn shutdown_disarms(actuator_enabled: Option<bool>) -> bool {
    actuator_enabled != Some(false)
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

/// Whether a refused write of this register may be skipped instead of stopping the batch.
///
/// Only a slot **start** time qualifies: the inverter floors those to its 5-minute grid, so a
/// value it cannot store leaves the previous one — the same setting, one slot earlier — in
/// force. Every other register here is money or mode (export cap, solar sell, work mode,
/// discharge cap, charge enable), and a refusal there still stops the batch and disarms.
pub fn is_slot_time(name: &str) -> bool {
    name.starts_with("tou_time_")
}

/// The TOU slot a register belongs to (`tou_soc_3` → 3), so a slot whose start the inverter
/// refused can be left exactly as it was while the rest of the plan still lands.
pub fn slot_of(name: &str) -> Option<usize> {
    const PREFIXES: [&str; 4] = ["tou_time_", "tou_power_", "tou_soc_", "tou_charge_source_"];
    PREFIXES
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix)?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{Mtu, PlanStep, Reason};
    use crate::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};

    /// 2026-11-04T00:00 Prague.
    const T0: i64 = 1_793_746_800;

    fn step(index: i64, action: Action) -> PlanStep {
        PlanStep {
            mtu: Mtu {
                start: T0 + index * 900,
                hours: Hours::new(0.25),
            },
            price_sell: CzkPerKwh::new(5.0),
            price_buy: CzkPerKwh::new(3.0),
            pv_kw: DcKw::ZERO,
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
            battery_charge_kw: 10.24,
        }
    }

    fn params_charging_at(kw: f64) -> ActuatorParams {
        ActuatorParams {
            battery_charge_kw: kw,
            ..params()
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
        assert_eq!(
            desired.blocks[0].soc, 20,
            "discharge is allowed only to the floor"
        );
        assert_eq!(desired.blocks[0].power_w, 6000);
        assert!(desired.sell_from_pack, "a sell is the pack's export valve");
        assert!(desired.sell_surplus, "selling needs solar sell on");
        assert_eq!(desired.max_sell_w, 6000);
    }

    #[test]
    fn a_hold_charges_to_the_ceiling_and_still_sells_the_surplus() {
        let desired = desired(&plan(vec![Action::Idle]), T0, &params());

        // The plant charges the pack to the slot target and sells only the surplus. The
        // target is the DP's ceiling: `solve` clamps the measured SOC into [floor, ceiling],
        // so a pack filled past 80 % would be invisible to the planner (2026-11-08).
        assert_eq!(desired.blocks[0].soc, 80, "the hold target is the ceiling");
        assert_eq!(desired.blocks[0].power_w, 0, "nothing may leave the pack");
        // A hold must NOT run `selling_first`: there the plant sells the PV and never charges
        // the pack (2026-11-07 — flat 21 % behind a 100 % target, 12 kW to the meter).
        assert!(
            !desired.sell_from_pack,
            "a hold never exports from the pack"
        );
        assert!(desired.sell_surplus, "the surplus still leaves");
        assert_eq!(desired.max_sell_w, 12000, "the interface rating, not zero");
    }

    #[test]
    fn a_spill_holds_every_slot_at_the_ceiling_and_arms_selling_first_last() {
        // The governor's spill: selling_first with all six targets equal removes the
        // next-slot lookahead that dumped the pack on 2026-11-02, and the 0 W cap holds the
        // pack while PV surplus exports (measured 2026-11-07: flat pack, 12 kW to the meter).
        let desired = spill_desired(T0, &params());
        assert_eq!(desired.blocks.len(), SLOTS);
        for block in &desired.blocks {
            assert_eq!(block.soc, 80, "every slot at the ceiling, no lower lookahead");
            assert_eq!(block.power_w, 0, "no discharge: the pack holds");
            assert_eq!(block.charge, Charge::Off);
        }
        assert!(desired.sell_from_pack, "spill runs selling_first to export PV");
        assert!(desired.sell_surplus);
        assert_eq!(desired.max_sell_w, 12000);
        assert!(!desired.grid_charge);

        let targets = targets(&desired);
        let index = |name: &str| {
            targets
                .iter()
                .position(|target| target.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        assert_eq!(
            targets.last().unwrap().name, "work_mode",
            "selling_first is armed only after every slot is in place"
        );
        assert_eq!(targets.last().unwrap().expect, Value::Text("selling_first".into()));
        assert!(index("tou_power_1") < index("tou_soc_1"));
        for n in 1..=SLOTS {
            let target = targets
                .iter()
                .find(|target| target.name == format!("tou_soc_{n}"))
                .unwrap();
            assert_eq!(target.write, serde_json::json!(80));
        }
    }

    #[test]
    fn a_buy_block_still_curtails_its_surplus() {
        // A grid-charge block runs in a no-export mode: the DP does not credit the surplus
        // there, and the actuator must not export it either.
        let desired = desired(
            &plan(vec![Action::Import { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        );

        assert!(!desired.sell_from_pack);
        assert!(!desired.sell_surplus);
        assert_eq!(desired.max_sell_w, 0);
    }

    #[test]
    fn a_hold_writes_the_discharge_cap_before_its_target_soc() {
        // With `selling_first` armed the pack once discharged 9.5–9.8 kW toward the previous
        // slot's target, so the 0 W cap that stops it must land before the target SOC.
        let targets = targets(&desired(&plan(vec![Action::Idle]), T0, &params()));
        let index = |name: &str| {
            targets
                .iter()
                .position(|target| target.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };

        assert!(index("tou_power_1") < index("tou_soc_1"));
        assert_eq!(
            targets[0].name, "work_mode",
            "the slots are rewritten with the pack's export disarmed"
        );
        assert_eq!(targets[0].expect, Value::Text("zero_export_load".into()));
    }

    #[test]
    fn a_sell_arms_the_mode_last() {
        // The other half of the ordering rule: `selling_first` lands only after every slot
        // carries the sell's own target and cap.
        let targets = targets(&desired(
            &plan(vec![Action::Export { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        ));

        let last = targets.last().expect("non-empty");
        assert_eq!(last.name, "work_mode");
        assert_eq!(last.expect, Value::Text("selling_first".into()));
    }

    #[test]
    fn a_curtailing_block_disarms_the_mode_first() {
        let targets = targets(&desired(
            &plan(vec![Action::Import { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        ));

        assert_eq!(
            targets[0].name, "work_mode",
            "nothing leaves while slots change"
        );
        assert_eq!(targets[0].expect, Value::Text("zero_export_load".into()));
    }

    #[test]
    fn a_shutdown_only_writes_when_the_actuator_was_on() {
        // 2026-11-06: a restart with the actuator off put the plant into zero-export.
        assert!(!shutdown_disarms(Some(false)), "off means untouched");
        assert!(shutdown_disarms(Some(true)), "on means leave it safe");
        assert!(
            shutdown_disarms(None),
            "an unreadable directive state is unknown, not off"
        );
    }

    #[test]
    fn a_buy_block_charges_from_the_grid_within_the_pack_limit() {
        let desired = desired(
            &plan(vec![Action::Import { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        );

        assert_eq!(desired.blocks[0].charge, Charge::Grid);
        assert_eq!(
            desired.blocks[0].soc, 80,
            "charge to the ceiling, no further"
        );
        assert!(desired.grid_charge);
        assert_eq!(desired.charge_current_a, 117, "6 kW at 51.2 V");
        assert!(!desired.charge_current_limited);
    }

    #[test]
    fn an_import_above_the_charger_limit_is_clamped_to_185_a() {
        let desired = desired(
            &plan(vec![Action::Import {
                kw: AcKw::new(12.0),
            }]),
            T0,
            &params(),
        );

        assert_eq!(desired.charge_current_a, 185);
        assert!(desired.charge_current_limited);
        assert_eq!(
            desired.charge_limit_a, 185,
            "the register's own ceiling binds"
        );
        assert_eq!(desired.charge_requested_kw, 12.0);
    }

    #[test]
    fn a_lowered_charge_rate_clamps_the_current_register() {
        // 6.144 kW at 51.2 V is 120 A — the number the inverter's battery setting was at
        // when this knob was asked for. The plan's 12 kW import asks for 234 A; the
        // configured rate, not the register's 185 A, must be what binds.
        let desired = desired(
            &plan(vec![Action::Import {
                kw: AcKw::new(12.0),
            }]),
            T0,
            &params_charging_at(6.144),
        );

        assert_eq!(desired.charge_current_a, 120);
        assert!(desired.charge_current_limited);
        assert_eq!(desired.charge_limit_a, 120);
    }

    #[test]
    fn a_charge_rate_above_the_registers_ceiling_still_writes_185_a() {
        // The knob may only lower: 10.24 kW is 200 A, but reg 128 stops at 185 A.
        let desired = desired(
            &plan(vec![Action::Import {
                kw: AcKw::new(12.0),
            }]),
            T0,
            &params_charging_at(10.24),
        );

        assert_eq!(desired.charge_current_a, 185);
        assert_eq!(desired.charge_limit_a, 185);
        assert!(desired.charge_current_limited);
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

    /// Four one-MTU segments: sell, hold, sell, hold. The plan runs out before the sixth slot.
    fn four_segments() -> Plan {
        plan(vec![
            Action::Export { kw: AcKw::new(1.0) },
            Action::Idle,
            Action::Export { kw: AcKw::new(2.0) },
            Action::Idle,
        ])
    }

    #[test]
    fn padded_slots_land_on_the_inverters_five_minute_grid() {
        // 2026-11-06: a padded start one minute after the last segment (00:46) is refused by
        // the inverter, which keeps 00:45 — the batch stopped there and the plant disarmed.
        let desired = desired(&four_segments(), T0, &params());

        assert_eq!(desired.blocks[4].start, "00:50");
        assert_eq!(desired.blocks[5].start, "00:55");
        for block in &desired.blocks {
            let (hour, minute) = block.start.split_once(':').expect("HH:MM");
            let total: usize =
                hour.parse::<usize>().unwrap() * 60 + minute.parse::<usize>().unwrap();
            assert_eq!(total % 5, 0, "{} is off the inverter's grid", block.start);
        }
    }

    #[test]
    fn the_midnight_pile_up_stays_on_the_grid() {
        // The grid runs out at 23:55, so the tail piles up there instead of on the off-grid
        // 23:59 the old padding used.
        let start = T0 + 23 * 3600 + 30 * 60;
        let plan = plan_at(
            start,
            vec![
                Action::Export { kw: AcKw::new(1.0) },
                Action::Idle,
                Action::Idle,
                Action::Idle,
            ],
        );
        let desired = desired(&plan, start, &params());

        assert_eq!(desired.blocks[0].start, "23:30");
        assert_eq!(desired.blocks[1].start, "23:45");
        assert_eq!(desired.blocks[2].start, "23:50");
        assert_eq!(desired.blocks[3].start, "23:55");
        assert_eq!(
            desired.blocks[5].start, "23:55",
            "the tail piles up on the grid"
        );
        assert!(
            desired.blocks.iter().all(|block| block.start != "23:59"),
            "an off-grid start leaked out: {:?}",
            desired.blocks.iter().map(|b| &b.start).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_plan_still_starts_on_the_grid() {
        // No future segment: the anchor is `now`, floored to the grid.
        let plan = plan_at(T0, vec![Action::Export { kw: AcKw::new(1.0) }]);
        let desired = desired(&plan, T0 + 3600 + 7 * 60, &params());

        assert_eq!(desired.blocks[0].start, "01:05");
    }

    #[test]
    fn only_a_slot_start_time_may_be_skipped() {
        assert!(is_slot_time("tou_time_3"));
        for name in [
            "tou_power_3",
            "tou_soc_3",
            "tou_charge_source_3",
            "tou_enabled",
            "max_sell_power",
            "solar_sell",
            "work_mode",
            "grid_charge_enable",
            "grid_charge_current",
        ] {
            assert!(!is_slot_time(name), "{name} must still stop the batch");
        }
        assert_eq!(slot_of("tou_time_4"), Some(4));
        assert_eq!(slot_of("tou_charge_source_6"), Some(6));
        assert_eq!(slot_of("max_sell_power"), None);
    }

    #[test]
    fn only_registers_that_disagree_are_written() {
        let desired = desired(
            &plan(vec![Action::Export { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        );
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
        let desired = desired(
            &plan(vec![Action::Export { kw: AcKw::new(6.0) }]),
            T0,
            &params(),
        );
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
        actions.push(Action::Import {
            kw: AcKw::new(12.0),
        });
        let desired = desired(&plan_at(start, actions), start + 5 * 60, &params());

        assert_eq!(desired.blocks[0].start, "23:45");
        assert_eq!(desired.blocks[0].soc, 80, "a hold targets the ceiling");
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
                .all(|block| block.soc == 80 && block.charge == Charge::Off),
            "tomorrow's buy leaked into today's slots: {:?}",
            desired.blocks
        );
        assert!(!desired.grid_charge);
        assert!(
            !desired.sell_from_pack && desired.sell_surplus,
            "the current block is a hold, which sells only its surplus"
        );
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

        assert_eq!(
            by_name("work_mode").expect,
            Value::Text("zero_export_load".into())
        );
        assert_eq!(by_name("max_sell_power").expect, Value::Number(0.0));
        assert_eq!(by_name("solar_sell").expect, Value::Number(0.0));
        assert_eq!(by_name("grid_charge_enable").expect, Value::Number(0.0));
        assert_eq!(by_name("tou_enabled").expect, Value::Number(1.0));
        for n in 1..=SLOTS {
            assert_eq!(
                by_name(&format!("tou_soc_{n}")).expect,
                Value::Number(100.0)
            );
            assert_eq!(
                by_name(&format!("tou_power_{n}")).expect,
                Value::Number(0.0)
            );
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
        assert_eq!(desired.blocks[0].soc, 80, "the current block is the hold");
        assert!(
            !desired.sell_from_pack && desired.sell_surplus,
            "a hold sells its surplus, never the pack"
        );
    }

    fn steps_to_actions(steps: &[PlanStep]) -> Vec<Action> {
        steps.iter().map(|step| step.action).collect()
    }
}
