use std::collections::BTreeMap;

use serde::Serialize;

use crate::decode::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub link_ok: bool,
    pub ts: u64,
    pub values: BTreeMap<String, Value>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_secs()
}
