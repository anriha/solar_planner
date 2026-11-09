//! The per-MTU ledger: what was planned and what the meter did (board-aru).
//!
//! One row per closed fifteen-minute MTU, appended to the day file that the row's own start
//! belongs to. The row is the only place the two halves meet: the planner's intention (taken
//! from the plan in force when the MTU opened) and the meter's integral over the polls the
//! bus produced.
//!
//! The actor is fed by the same fan-out Home Assistant reads: it subscribes to
//! [`StateActor`](crate::state::StateActor) like any socket would, so the poll path and the
//! planner path are untouched, and a slow disk can never delay a Modbus cycle.
//!
//! # What a row means
//!
//! * `plan` — the plan **snapshot taken when the MTU opened**. A revision that lands mid-MTU
//!   only schedules the remainder and cannot be compared to the whole MTU's energy, so the
//!   opening plan is the honest comparison. `computed_at` is stored beside it to make that
//!   visible.
//! * `actual.ac_kwh` — signed AC through the meter, export positive, integrated by trapezoid
//!   over the samples inside the MTU. The meter is register 607 and the sign flip lives in
//!   [`MeteredSignals::ac_kw`]; see the measurement in its doc comment.
//! * `actual.soc_end_pct` — the last SOC sample inside the MTU, i.e. the pack at the boundary.
//!
//! A restart in the middle of an MTU drops that MTU's row: a half-integrated row would look
//! like a plant that missed its target. The next MTU starts a fresh accumulator.
use serde::{Deserialize, Serialize};

use kameo::{
    Actor,
    actor::{ActorRef, Spawn},
    message::Message,
};
use tokio::sync::mpsc;

use crate::{
    planner::{CAPACITY_KWH, Plan},
    protocol::ServerMsg,
    state::{GetState, StateActor, Subscribe},
    store::{AppendRow, ReadRange, StoreActor},
    telemetry::MeteredSignals,
};

/// One MTU, in seconds. The planner's `Mtu::hours` is 0.25; this is the same quantity.
pub const MTU_SECS: i64 = 15 * 60;

/// The plan's side of one MTU, projected so the row reads on its own long after the plan
/// that produced it has been replaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedMtu {
    /// `computed_at` of the plan this step came from.
    pub computed_at: i64,
    /// `idle` | `export` | `import`.
    pub kind: String,
    /// The **signed command**: positive exports, negative imports.
    pub kw: f64,
    /// Metered AC the DP expected, signed the same way.
    pub ac_kwh: f64,
    pub soc_end_pct: f64,
    pub net_czk: f64,
    pub reason: String,
}

/// The meter's side of one MTU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActualMtu {
    /// Signed AC through the meter: positive exported, negative imported.
    pub ac_kwh: f64,
    /// Pack at the MTU boundary; `None` when no sample landed in the MTU.
    pub soc_end_pct: Option<f64>,
    /// Polls integrated. A row with very few samples is a gap, not a quiet plant.
    pub samples: u32,
}

/// One ledger row, exactly as it is stored in the day file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerRow {
    /// Unix seconds of the MTU start — also the key the day file is chosen by.
    pub start: i64,
    pub hours: f64,
    pub plan: Option<PlannedMtu>,
    pub actual: ActualMtu,
}

/// An MTU that has just closed.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosedMtu {
    pub start: i64,
    pub energy_kwh: f64,
    pub soc_end: f64,
    pub samples: u32,
    pub planned: Option<PlannedMtu>,
}

/// The integral over one open MTU.
#[derive(Debug, Clone)]
pub struct Accumulator {
    start: i64,
    last_ts: i64,
    last_kw: f64,
    energy_kwh: f64,
    soc_end: f64,
    samples: u32,
    planned: Option<PlannedMtu>,
}

impl Accumulator {
    pub fn new(ts: i64, kw: f64, soc: f64, planned: Option<PlannedMtu>) -> Self {
        Self {
            start: mtu_start(ts),
            last_ts: ts,
            last_kw: kw,
            energy_kwh: 0.0,
            soc_end: soc,
            samples: 1,
            planned,
        }
    }

    pub fn start(&self) -> i64 {
        self.start
    }

    /// Fold one sample in. `Some(closed)` means this sample belongs to the next MTU; the
    /// interval between the last sample and the MTU boundary belongs to the closed one, and
    /// the remainder is the next MTU's first sample.
    pub fn push(&mut self, ts: i64, kw: f64, soc: f64) -> Option<ClosedMtu> {
        if ts - self.start < MTU_SECS {
            let seconds = ts - self.last_ts;
            if seconds > 0 {
                self.energy_kwh += (self.last_kw + kw) / 2.0 * seconds as f64 / 3600.0;
            }
            self.last_ts = ts;
            self.last_kw = kw;
            self.soc_end = soc;
            self.samples += 1;
            return None;
        }
        // Close at the boundary, not at the late sample: a five-second overshoot must not be
        // billed to the MTU it does not belong to, and a long gap must not invent energy.
        let cut = (self.start + MTU_SECS).min(ts);
        if cut > self.last_ts {
            self.energy_kwh += (self.last_kw + kw) / 2.0 * (cut - self.last_ts) as f64 / 3600.0;
        }
        Some(ClosedMtu {
            start: self.start,
            energy_kwh: self.energy_kwh,
            soc_end: self.soc_end,
            samples: self.samples,
            planned: self.planned.clone(),
        })
    }
}

/// The MTU an instant belongs to.
pub fn mtu_start(ts: i64) -> i64 {
    ts - ts.rem_euclid(MTU_SECS)
}

/// Fold a plan step into the row's plan half. `None` when the plan does not cover the MTU.
pub fn planned_for(plan: Option<&Plan>, start: i64) -> Option<PlannedMtu> {
    let plan = plan?;
    let step = plan.steps.iter().find(|step| step.mtu.start == start)?;
    let (kind, kw) = match step.action {
        crate::planner::Action::Idle => ("idle", 0.0),
        crate::planner::Action::Export { kw } => ("export", kw.get()),
        crate::planner::Action::Import { kw } => ("import", -kw.get()),
    };
    Some(PlannedMtu {
        computed_at: plan.computed_at,
        kind: kind.to_string(),
        kw: round3(kw),
        ac_kwh: round3(step.ac_kwh.get()),
        soc_end_pct: round1(step.energy_after_kwh.get() / CAPACITY_KWH * 100.0),
        net_czk: round3(step.net_czk.get()),
        reason: step.reason.describe(),
    })
}

/// A state frame or a plan, as the ledger consumes them.
pub struct Observe(pub ServerMsg);

/// Rows for `[from, to)`, oldest first.
pub struct QueryRows {
    pub from: i64,
    pub to: i64,
}

#[derive(Actor)]
pub struct LedgerActor {
    store: ActorRef<StoreActor>,
    plan: Option<Plan>,
    acc: Option<Accumulator>,
    last_meter_error: Option<String>,
}

impl LedgerActor {
    pub fn new(store: ActorRef<StoreActor>) -> Self {
        Self {
            store,
            plan: None,
            acc: None,
            last_meter_error: None,
        }
    }

    /// Enter one sample. Opening a new MTU snapshots the plan that is in force for it.
    async fn sample(&mut self, ts: i64, signals: MeteredSignals) {
        let closed = self
            .acc
            .as_mut()
            .and_then(|acc| acc.push(ts, signals.ac_kw(), signals.battery_soc));
        if let Some(closed) = closed {
            self.write(closed).await;
        }
        let start = mtu_start(ts);
        if self.acc.as_ref().is_none_or(|acc| acc.start() != start) {
            self.acc = Some(Accumulator::new(
                ts,
                signals.ac_kw(),
                signals.battery_soc,
                planned_for(self.plan.as_ref(), start),
            ));
        }
    }

    async fn write(&mut self, closed: ClosedMtu) {
        let row = LedgerRow {
            start: closed.start,
            hours: MTU_SECS as f64 / 3600.0,
            plan: closed.planned,
            actual: ActualMtu {
                ac_kwh: round3(closed.energy_kwh),
                soc_end_pct: Some(round1(closed.soc_end)),
                samples: closed.samples,
            },
        };
        match serde_json::to_string(&row) {
            Ok(line) => {
                // `tell(..).send()` is lazy: the future has to be awaited or the row is
                // never enqueued. The store owns the file and reports its own failures.
                let _ = self
                    .store
                    .tell(AppendRow {
                        at: row.start,
                        line,
                    })
                    .send()
                    .await;
            }
            Err(error) => eprintln!("[ledger] could not serialise a row: {error}"),
        }
    }
}

impl Message<Observe> for LedgerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Observe,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match msg.0 {
            ServerMsg::State {
                ts,
                link_ok,
                values,
                ..
            } => {
                if !link_ok {
                    return; // the values are stale; integrating them would invent energy
                }
                match MeteredSignals::extract_values(&values) {
                    Ok(signals) => {
                        if self.last_meter_error.take().is_some() {
                            eprintln!("[ledger] meter signals are back");
                        }
                        self.sample(ts as i64, signals).await;
                    }
                    Err(error) => {
                        // Log once per distinct failure: a renamed sensor must not fill the
                        // journal once per poll.
                        let message = error.to_string();
                        if self.last_meter_error.as_deref() != Some(message.as_str()) {
                            eprintln!("[ledger] no ledger row from this frame: {message}");
                            self.last_meter_error = Some(message);
                        }
                    }
                }
            }
            ServerMsg::Plan { plan } => self.plan = plan,
            _ => {}
        }
    }
}

impl Message<QueryRows> for LedgerActor {
    type Reply = Vec<LedgerRow>;

    async fn handle(
        &mut self,
        msg: QueryRows,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match self
            .store
            .ask(ReadRange {
                from: msg.from,
                to: msg.to,
            })
            .await
        {
            Ok(lines) => rows_from(lines, msg.from, msg.to),
            Err(kameo::error::SendError::HandlerError(error)) => {
                eprintln!("[ledger] reading the day files failed: {error}");
                Vec::new()
            }
            Err(_) => Vec::new(),
        }
    }
}

/// Parse stored lines and keep the rows inside `[from, to)`, oldest first. A line that does
/// not parse is skipped loudly — one corrupt row must not cost the whole window.
pub fn rows_from(lines: Vec<String>, from: i64, to: i64) -> Vec<LedgerRow> {
    let mut rows: Vec<LedgerRow> = lines
        .into_iter()
        .filter_map(|line| match serde_json::from_str::<LedgerRow>(&line) {
            Ok(row) => Some(row),
            Err(error) => {
                eprintln!("[ledger] skipping an unreadable row: {error}");
                None
            }
        })
        .filter(|row| row.start >= from && row.start < to)
        .collect();
    rows.sort_by_key(|row| row.start);
    rows
}

/// Spawn the actor and hang it off the state fan-out.
///
/// The subscription is the same `Subscribe` a websocket client uses, so there is no new
/// coupling into the bus or the planner: the ledger is just another reader.
pub async fn spawn(
    state: ActorRef<StateActor>,
    store: ActorRef<StoreActor>,
) -> anyhow::Result<ActorRef<LedgerActor>> {
    let actor = LedgerActor::spawn(LedgerActor::new(store));
    // Seed the plan: a restart that lands between two planner ticks would otherwise file the
    // first MTU with no plan at all.
    if let Ok(payload) = state.ask(GetState).await
        && payload.plan.is_some()
    {
        let _ = actor
            .tell(Observe(ServerMsg::Plan {
                plan: payload.plan,
            }))
            .send()
            .await;
    }
    let (tx, mut rx) = mpsc::channel(64);
    state.ask(Subscribe { tx }).await?;
    let forward = actor.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if forward.tell(Observe(msg)).send().await.is_err() {
                break; // actor gone = process shutting down
            }
        }
    });
    Ok(actor)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-09 12:00 Prague — an MTU boundary.
    const NOON: i64 = 1_794_222_000;

    #[test]
    fn a_steady_ten_kilowatts_fills_the_mtu_with_two_point_five_kwh() {
        let mut acc = Accumulator::new(NOON, 10.0, 41.0, None);
        for minute in 1..15 {
            assert!(
                acc.push(NOON + minute * 60, 10.0, 41.0).is_none(),
                "still inside the MTU"
            );
        }
        let closed = acc.push(NOON + MTU_SECS, 10.0, 42.0).expect("the boundary closes it");
        assert_eq!(closed.start, NOON);
        assert!((closed.energy_kwh - 2.5).abs() < 1e-9, "{}", closed.energy_kwh);
        assert_eq!(closed.soc_end, 41.0);
        assert_eq!(closed.samples, 15);
    }

    /// Only the part of the straddling interval that is inside the MTU is billed to it.
    #[test]
    fn a_sample_past_the_boundary_closes_at_the_boundary() {
        let mut acc = Accumulator::new(NOON + 60, 8.0, 50.0, None);
        // 13:59:55 → 14:00:05: five seconds belong to the MTU, five to the next one.
        acc.push(NOON + MTU_SECS - 5, 8.0, 50.0);
        let closed = acc
            .push(NOON + MTU_SECS + 5, 8.0, 50.0)
            .expect("the boundary closes it");
        let expected = 8.0 * 14.0 * 60.0 / 3600.0; // 14 minutes at 8 kW
        assert!((closed.energy_kwh - expected).abs() < 1e-9, "{}", closed.energy_kwh);
    }

    #[test]
    fn an_import_is_negative_energy() {
        let mut acc = Accumulator::new(NOON, -3.0, 20.0, None);
        let closed = acc
            .push(NOON + MTU_SECS, -3.0, 21.0)
            .expect("the boundary closes it");
        assert!(closed.energy_kwh < 0.0);
        assert!((closed.energy_kwh + 0.75).abs() < 1e-9);
    }

    #[test]
    fn a_row_round_trips_through_the_day_file_format() {
        let row = LedgerRow {
            start: NOON,
            hours: 0.25,
            plan: Some(PlannedMtu {
                computed_at: NOON - 60,
                kind: "export".to_string(),
                kw: 8.4,
                ac_kwh: 2.1,
                soc_end_pct: 21.4,
                net_czk: 13.47,
                reason: "sell 7.90 > stored 5.32 CZK/kWh".to_string(),
            }),
            actual: ActualMtu {
                ac_kwh: 2.07,
                soc_end_pct: Some(21.1),
                samples: 180,
            },
        };
        let line = serde_json::to_string(&row).unwrap();
        let back: LedgerRow = serde_json::from_str(&line).unwrap();
        assert_eq!(back, row);
    }
}
