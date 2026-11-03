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
        directives: Directives,
    },
    /// Telemetry, one frame per poll. It deliberately does **not** carry the plan: a full
    /// horizon is ~60–80 KB and the poll is 5 s, which would be about 1.2 GB per day per
    /// subscriber for a value that changes once a minute. The plan rides `Hello` (the
    /// complete snapshot on connect) and `Plan` (one frame per change).
    State {
        ts: u64,
        link_ok: bool,
        values: BTreeMap<String, Value>,
        directives: Directives,
    },
    /// The plan, pushed when the planner publishes one and re-sent to a subscriber that
    /// catches up after lagging. `None` withdraws it (planner off, or no horizon yet).
    Plan { plan: Option<Plan> },
    CmdAck {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        directives: Directives,
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
            directives: directives.clone(),
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
    Ping,
}
