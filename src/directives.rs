use std::path::{Path, PathBuf};

use kameo::{Actor, Reply, actor::ActorRef, message::Message};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::{StateActor, Update};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct Percent(f64);

impl TryFrom<f64> for Percent {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if (0.0..=100.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(format!("expected 0–100, got {v}"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64")]
pub struct Price(f64);

impl TryFrom<f64> for Price {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if v.abs() <= 10_000.0 {
            Ok(Self(v))
        } else {
            Err(format!("expected at most ±10000 CZK/kWh, got {v}"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForceMode {
    Auto,
    Charge,
    Discharge,
    Hold,
}

#[derive(Debug, Clone, PartialEq, Reply, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directives {
    pub version: u64,
    pub planner_enabled: bool,
    pub force_mode: ForceMode,
    pub soc_target_hold: Option<Percent>,
    pub price_override: Option<Price>,
}

impl Default for Directives {
    fn default() -> Self {
        Self {
            version: 0,
            planner_enabled: true,
            force_mode: ForceMode::Auto,
            soc_target_hold: None,
            price_override: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("unknown directive '{0}'")]
    Unknown(String),
    #[error("{name}: {reason}")]
    Invalid { name: String, reason: String },
    #[error("persisting directives failed: {0}")]
    Persist(String),
}

fn invalid(name: &str, reason: &str) -> ApplyError {
    ApplyError::Invalid {
        name: name.to_string(),
        reason: reason.to_string(),
    }
}

fn set<T: PartialEq>(slot: &mut T, new: T) -> bool {
    if *slot == new {
        false
    } else {
        *slot = new;
        true
    }
}

impl Directives {
    pub fn apply(&mut self, name: &str, value: serde_json::Value) -> Result<bool, ApplyError> {
        const MODES: &str = "expected \"auto\"|\"charge\"|\"discharge\"|\"hold\"";
        let changed = match name {
            "planner_enabled" => {
                let new = value
                    .as_bool()
                    .ok_or_else(|| invalid(name, "expected true/false"))?;
                set(&mut self.planner_enabled, new)
            }
            "force_mode" => {
                let new = match value.as_str() {
                    Some("auto") => ForceMode::Auto,
                    Some("charge") => ForceMode::Charge,
                    Some("discharge") => ForceMode::Discharge,
                    Some("hold") => ForceMode::Hold,
                    _ => return Err(invalid(name, MODES)),
                };
                set(&mut self.force_mode, new)
            }
            "soc_target_hold" => {
                let new = match &value {
                    serde_json::Value::Null => None,
                    v => Some(
                        Percent::try_from(
                            v.as_f64()
                                .ok_or_else(|| invalid(name, "expected a number or null"))?,
                        )
                        .map_err(|e| invalid(name, &e))?,
                    ),
                };
                set(&mut self.soc_target_hold, new)
            }
            "price_override" => {
                let new = match &value {
                    serde_json::Value::Null => None,
                    v => Some(
                        Price::try_from(
                            v.as_f64()
                                .ok_or_else(|| invalid(name, "expected a number or null"))?,
                        )
                        .map_err(|e| invalid(name, &e))?,
                    ),
                };
                set(&mut self.price_override, new)
            }
            _ => return Err(ApplyError::Unknown(name.to_string())),
        };
        if changed {
            self.version += 1;
        }
        Ok(changed)
    }
}

pub fn load(path: &Path) -> anyhow::Result<Option<Directives>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            Ok(Some(serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("directives file {path:?}: {e}")
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {path:?}: {e}")),
    }
}

/// Atomic write: tmp file + rename within the same directory/filesystem.
pub fn save(path: &Path, d: &Directives) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Actor)]
pub struct DirectivesActor {
    path: PathBuf,
    current: Directives,
    state: ActorRef<StateActor>,
}

impl DirectivesActor {
    /// Loads persisted state; errors on a corrupt file (fail startup loudly).
    pub fn new(path: PathBuf, state: ActorRef<StateActor>) -> anyhow::Result<Self> {
        let current = load(&path)?.unwrap_or_default();
        Ok(Self {
            path,
            current,
            state,
        })
    }
}

pub struct ApplyDirective {
    pub name: String,
    pub value: serde_json::Value,
}

pub struct GetCurrent;

impl Message<ApplyDirective> for DirectivesActor {
    type Reply = Result<Directives, ApplyError>;

    async fn handle(
        &mut self,
        msg: ApplyDirective,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut next = self.current.clone();
        match next.apply(&msg.name, msg.value) {
            Ok(false) => Ok(self.current.clone()),
            Ok(true) => {
                save(&self.path, &next).map_err(|e| ApplyError::Persist(e.to_string()))?;
                self.current = next.clone();
                let _ = self.state.tell(Update::Directives(next)).send().await;
                Ok(self.current.clone())
            }
            Err(e) => Err(e),
        }
    }
}

impl Message<GetCurrent> for DirectivesActor {
    type Reply = Directives;

    async fn handle(
        &mut self,
        _msg: GetCurrent,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.current.clone()
    }
}
