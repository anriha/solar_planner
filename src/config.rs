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
    /// forecast.solar site geometry; defaults match the installed array.
    #[serde(default)]
    pub site: SiteConfig,
}

fn default_directives_path() -> String {
    "directives.json".to_string()
}

/// The array, as forecast.solar needs it. Boot config: the panels do not move.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    pub latitude: f64,
    pub longitude: f64,
    /// Panel tilt from horizontal, degrees.
    pub declination: f64,
    /// Compass bearing, degrees (0 north, 180 south, 200 = SSW).
    pub azimuth: f64,
    /// Installed DC capacity, kWp.
    pub kwp: f64,
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self {
            latitude: 50.08,
            longitude: 14.45,
            declination: 37.0,
            azimuth: 200.0,
            kwp: 16.5,
        }
    }
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
            site: SiteConfig::default(),
        }
    }
}

pub fn load(path: &str) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    Ok(toml::from_str(&raw)?)
}
