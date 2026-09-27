use std::collections::BTreeMap;

use kameo::Reply;
use serde::{Deserialize, Serialize};

use crate::{decode::Value, directives::Directives, telemetry::Snapshot};

#[derive(Debug, Clone, Default, Reply, Serialize)]
pub struct StatePayload {
    pub ts: u64,
    pub link_ok: bool,
    pub values: BTreeMap<String, Value>,
}

impl StatePayload {
    pub fn from_snapshot(snap: Snapshot) -> Self {
        Self {
            ts: snap.ts,
            link_ok: true,
            values: snap.values,
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
    State {
        ts: u64,
        link_ok: bool,
        values: BTreeMap<String, Value>,
        directives: Directives,
    },
    CmdAck {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        directives: Directives,
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
    Ping,
}
