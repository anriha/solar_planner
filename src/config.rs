use serde::Deserialize;

use crate::bus::POLL_INTERVAL;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub port: String,
    pub baud: u32,
    pub slave: u8,
    pub interval_secs: u64,
    pub bind: String,
    pub token: String,
    #[serde(default = "default_directives_path")]
    pub directives_path: String,
}

fn default_directives_path() -> String {
    "directives.json".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: "/dev/ttyRS485".to_string(),
            baud: 9600,
            slave: 1,
            interval_secs: POLL_INTERVAL.as_secs(),
            bind: "0.0.0.0:8300".to_string(),
            token: String::new(),
            directives_path: default_directives_path(),
        }
    }
}

pub fn load(path: &str) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    Ok(toml::from_str(&raw)?)
}
