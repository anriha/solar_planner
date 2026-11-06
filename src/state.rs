use kameo::{Actor, message::Message};
use tokio::sync::mpsc;

use crate::{
    directives::Directives,
    planner::Plan,
    protocol::{ServerMsg, StatePayload},
    telemetry::Snapshot,
};

/// One open connection's outbound queue.
struct Conn {
    id: u64,
    tx: mpsc::Sender<ServerMsg>,
    /// A frame was skipped because this connection's socket fell behind.
    /// Cleared by the first frame that gets through again.
    lagging: bool,
}

#[derive(Actor, Default)]
pub struct StateActor {
    payload: StatePayload,
    directives: Directives,
    subscribers: Vec<Conn>,
    next_id: u64,
}

pub enum Update {
    Good(Snapshot),
    Down,
    /// Boxed: the curve fields make `Directives` ~400 bytes, and every other variant here
    /// is a pointer or a small option — the plan/telemetry messages must not carry it.
    Directives(Box<Directives>),
    /// The planner's latest output. `None` withdraws it (planner off, no horizon).
    Plan(Option<Plan>),
}

pub struct Subscribe {
    pub tx: mpsc::Sender<ServerMsg>,
}

pub struct Unsubscribe(pub u64);

pub struct GetState;

impl StateActor {
    /// Fan the current snapshot out to every open connection.
    ///
    /// Every *telemetry* frame is a complete snapshot, so skipping one for a connection
    /// whose socket is momentarily behind costs nothing — the next frame carries the
    /// whole state again. Reaping that subscriber instead (the behaviour until
    /// 2026-11-03) severs the feed permanently: the writer task ends, the TCP socket
    /// stays ESTABLISHED, axum keeps answering protocol pings, and the client's read
    /// loop never receives anything again — no error, no `unavailable`, no reconnect.
    /// That is how HA lost the inverter for 19 h 30 m while the bridge still looked
    /// healthy. Only a closed queue means the connection is gone.
    ///
    /// The plan is *not* in the telemetry stream (see [`ServerMsg::State`]): it changes
    /// about once a minute, so it is its own frame. That makes it the one message a
    /// skipped frame could lose, so a connection that catches up is handed the current
    /// plan again — once, at the transition, not on every later frame.
    fn push(&mut self, msg: ServerMsg) {
        let is_plan = matches!(msg, ServerMsg::Plan { .. });
        let mut reaped = 0usize;
        let mut caught_up: Vec<u64> = Vec::new();
        for conn in &mut self.subscribers {
            match conn.tx.try_send(msg.clone()) {
                Ok(()) => {
                    if conn.lagging {
                        conn.lagging = false;
                        eprintln!("[state] conn {} caught up", conn.id);
                        caught_up.push(conn.id);
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if !conn.lagging {
                        conn.lagging = true;
                        eprintln!(
                            "[state] conn {} is behind; skipping frames until it catches up",
                            conn.id
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => reaped += 1,
            }
        }
        if reaped > 0 {
            eprintln!("[state] reaping {reaped} closed connection(s)");
            self.subscribers.retain(|conn| !conn.tx.is_closed());
        }
        if caught_up.is_empty() || is_plan {
            return;
        }
        let plan = ServerMsg::Plan {
            plan: self.payload.plan.clone(),
        };
        for conn in &mut self.subscribers {
            if !caught_up.contains(&conn.id) {
                continue;
            }
            match conn.tx.try_send(plan.clone()) {
                Ok(()) => {}
                // No room even for this: the connection is behind again, so stay in that
                // state and let the next frame that gets through retry the replay.
                Err(mpsc::error::TrySendError::Full(_)) => conn.lagging = true,
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    fn broadcast(&mut self) {
        let msg = ServerMsg::state(&self.payload, &self.directives);
        self.push(msg);
    }
}

impl Message<Update> for StateActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Update,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match msg {
            Update::Good(snap) => {
                // The plan is a separate stream from telemetry: a poll must not wipe it.
                let plan = self.payload.plan.clone();
                self.payload = StatePayload::from_snapshot(snap, plan);
                self.broadcast();
            }
            Update::Down => {
                if self.payload.link_ok {
                    self.payload.link_ok = false;
                    self.broadcast();
                }
            }
            Update::Directives(d) => {
                if *d != self.directives {
                    self.directives = *d;
                    self.broadcast();
                }
            }
            Update::Plan(plan) => {
                // One frame per *change*, not per poll: the plan is 60–80 KB and is
                // republished once a minute, so it has its own message type rather than
                // riding the 5 s telemetry frames (1.2 GB/day per subscriber). No
                // equality check — every plan carries its own `computed_at`.
                self.payload.plan = plan.clone();
                self.push(ServerMsg::Plan { plan });
            }
        }
    }
}

impl Message<Subscribe> for StateActor {
    type Reply = u64;

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let id = self.next_id;
        self.next_id += 1;
        self.subscribers.push(Conn {
            id,
            tx: msg.tx,
            lagging: false,
        });
        eprintln!(
            "[state] conn {id} subscribed ({} open)",
            self.subscribers.len()
        );
        id
    }
}

impl Message<Unsubscribe> for StateActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let before = self.subscribers.len();
        self.subscribers.retain(|conn| conn.id != msg.0);
        if self.subscribers.len() != before {
            eprintln!(
                "[state] conn {} unsubscribed ({} open)",
                msg.0,
                self.subscribers.len()
            );
        }
    }
}

impl Message<GetState> for StateActor {
    type Reply = StatePayload;

    async fn handle(
        &mut self,
        _msg: GetState,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.payload.clone()
    }
}
