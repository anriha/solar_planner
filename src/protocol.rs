use std::collections::BTreeMap;

use kameo::Reply;
use serde::{Deserialize, Serialize};

use crate::{decode::Value, directives::Directives, planner::Plan, telemetry::Snapshot};

#[derive(Debug, Clone, Default, Reply, Serialize)]
pub struct StatePayload {
    pub ts: u64,
    pub link_ok: bool,
    pub values: BTreeMap<String, Value>,
    /// The latest dispatch plan, absent while the planner is off or has no horizon.
    /// Carried by `Hello` and `/state` — the *complete* snapshot — while live subscribers
    /// get it as a separate `ServerMsg::Plan` frame, so the 5 s poll stays small.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
}

impl StatePayload {
    pub fn from_snapshot(snap: Snapshot, plan: Option<Plan>) -> Self {
        Self {
            ts: snap.ts,
            link_ok: true,
            values: snap.values,
            plan,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        meta: serde_json::Value,
        state: StatePayload,
        /// The flat directive map (see [`Directives::wire_json`]): one key per entity.
        directives: serde_json::Value,
    },
    /// Telemetry, one frame per poll. It deliberately does **not** carry the plan: a full
    /// horizon is ~60–80 KB and the poll is 5 s, which would be about 1.2 GB per day per
    /// subscriber for a value that changes once a minute. The plan rides `Hello` (the
    /// complete snapshot on connect) and `Plan` (one frame per change).
    State {
        ts: u64,
        link_ok: bool,
        values: BTreeMap<String, Value>,
        directives: serde_json::Value,
    },
    /// The plan, pushed when the planner publishes one and re-sent to a subscriber that
    /// catches up after lagging. `None` withdraws it (planner off, or no horizon yet).
    ///
    /// The browser never reads this frame — it is told [`ServerMsg::PlanChanged`] and
    /// re-requests a [`ServerMsg::View`] (board-zpe).
    Plan { plan: Option<Plan> },
    /// The plan changed — an invitation to re-request a view, not a second copy of the plan.
    ///
    /// Sent only to connections that have asked for a view at least once. It deliberately
    /// carries no payload: the client re-requests `get_view` with **its own** window, so a
    /// viewer showing a week is never overwritten by a horizon-shaped frame. Until
    /// 2026-11-09 this frame *was* the view, projected once with the default window and
    /// cloned to every watcher, which silently reset a wider chart on every replan.
    PlanChanged {
        /// `computed_at` of the new plan; `null` when the plan was withdrawn, so a viewer
        /// can tell "clear the chart" from "re-draw the same plan".
        computed_at: Option<i64>,
    },
    /// The browser's flat projection of the plan (see [`crate::view::View`]). Always the
    /// reply to a [`ClientMsg::GetView`], so `id` is the request that asked for it.
    View {
        id: String,
        view: crate::view::View,
    },
    CmdAck {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        directives: serde_json::Value,
    },
    WriteAck {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Pong,
    Error {
        message: String,
    },
}

impl ServerMsg {
    pub fn state(payload: &StatePayload, directives: &Directives) -> Self {
        ServerMsg::State {
            ts: payload.ts,
            link_ok: payload.link_ok,
            values: payload.values.clone(),
            directives: directives.wire_json(),
        }
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("server messages always serialize")
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Cmd {
        id: String,
        name: String,
        value: serde_json::Value,
    },
    // --- Control writes (2026-10-11 D1–D5) ----------------------------------
    // HA -> bridge register writes. Wire contract:
    //   client->server {"type":"write","id":"w1","name":"max_sell_power","value":200}
    //   client->server {"type":"write","id":"w2","name":"grid_charge_enable","value":true}
    //   server->client {"type":"write_ack","id":"w1","ok":true}
    //   server->client {"type":"write_ack","id":"w1","ok":false,"error":"..."}
    // value semantics (settled 2026-10-11, decision 1b): number constraints ->
    //   JSON number (out-of-range REJECTED, never clamped, D4; non-representable
    //   under scale/offset also rejected, 3a); switch constraints -> true/false
    //   (bridge maps via constraint on/off); select constraints -> the option's
    //   u16 VALUE from meta.controls (labels are display-only). Validation and
    //   encoding live in BusActor (2b); errors carry name + address.
    // Every write is FC06 -> FC03 readback -> raw equality before ok=true (D5).
    Write {
        id: String,
        name: String,
        value: serde_json::Value,
    },
    /// Ask for the browser's flat plan projection (board-zpe).
    ///
    ///   client->server {"type":"get_view","id":"v1","from":1793746800,"to":1793833200}
    ///   server->client {"type":"view","id":"v1","view":{...}}
    ///
    /// `from`/`to` are unix seconds and optional: omitted, both default to the plan's own
    /// horizon. The window is clamped to [`crate::view::MAX_WINDOW_SECS`] — 60 days — and the
    /// reply echoes the window actually served. Answering also subscribes the connection to
    /// [`ServerMsg::PlanChanged`] notices, so the window it asked for is the window it keeps
    /// getting; a client that never asks (Home Assistant) never receives them.
    GetView {
        id: String,
        #[serde(default)]
        from: Option<i64>,
        #[serde(default)]
        to: Option<i64>,
    },
    Ping,
}
