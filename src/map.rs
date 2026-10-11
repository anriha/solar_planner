use serde::{Deserialize, Serialize};

pub const EMBEDDED_MAP: &str = include_str!("../maps/sun_3ph_hybrid.toml");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterMap {
    pub sensors: Vec<SensorDef>,
    #[serde(default)]
    pub controls: Vec<ControlDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Encoding {
    U16,
    S16,
    U32 { base_word: BaseWord },
    S32 { base_word: BaseWord },
    Str { words: u16 },
}

// serde does not enforce deny_unknown_fields inside internally-tagged enum
// variants (verified live: `base_word` on a u16 entry was silently dropped),
// so map loading goes through deny-strict raw mirrors and converts with
// full combination checks. The enums above stay the public types; the
// mirrors exist only at the deserialization boundary.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EncodingKind {
    U16,
    S16,
    U32,
    S32,
    Str,
}

impl EncodingKind {
    fn as_str(&self) -> &'static str {
        match self {
            EncodingKind::U16 => "u16",
            EncodingKind::S16 => "s16",
            EncodingKind::U32 => "u32",
            EncodingKind::S32 => "s32",
            EncodingKind::Str => "str",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEncoding {
    #[serde(rename = "type")]
    kind: EncodingKind,
    #[serde(default)]
    base_word: Option<BaseWord>,
    #[serde(default)]
    words: Option<u16>,
}

fn encoding_from_raw(raw: RawEncoding) -> Result<Encoding, String> {
    match raw.kind {
        EncodingKind::U16 | EncodingKind::S16 => {
            if raw.base_word.is_some() {
                return Err(format!("{} encoding takes no base_word", raw.kind.as_str()));
            }
            if raw.words.is_some() {
                return Err(format!("{} encoding takes no words", raw.kind.as_str()));
            }
            Ok(if matches!(raw.kind, EncodingKind::U16) {
                Encoding::U16
            } else {
                Encoding::S16
            })
        }
        kind @ (EncodingKind::U32 | EncodingKind::S32) => {
            let Some(base_word) = raw.base_word else {
                return Err(format!("{} encoding requires base_word", kind.as_str()));
            };
            if raw.words.is_some() {
                return Err(format!("{} encoding takes no words", kind.as_str()));
            }
            Ok(match kind {
                EncodingKind::U32 => Encoding::U32 { base_word },
                _ => Encoding::S32 { base_word },
            })
        }
        EncodingKind::Str => {
            let Some(words) = raw.words else {
                return Err("str encoding requires words".to_string());
            };
            if words == 0 {
                return Err("str encoding requires words >= 1".to_string());
            }
            if raw.base_word.is_some() {
                return Err("str encoding takes no base_word".to_string());
            }
            Ok(Encoding::Str { words })
        }
    }
}

fn parse_encoding<'de, D>(d: D) -> Result<Encoding, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = RawEncoding::deserialize(d)?;
    encoding_from_raw(raw).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseWord {
    Low,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateClass {
    Measurement,
    Total,
    TotalIncreasing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumOptions {
    pub value: u16,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorDef {
    pub name: String,
    pub address: u16,
    #[serde(deserialize_with = "parse_encoding")]
    pub encoding: Encoding,
    #[serde(default = "unit")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub state_class: Option<StateClass>,
    #[serde(default)]
    pub options: Option<Vec<EnumOptions>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Constraint {
    Switch {
        on: u16,
        off: u16,
    },
    Select {
        options: Vec<EnumOptions>,
    },
    Number {
        min: f64,
        max: f64,
        step: Option<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConstraintKind {
    Switch,
    Select,
    Number,
}

impl ConstraintKind {
    fn as_str(&self) -> &'static str {
        match self {
            ConstraintKind::Switch => "switch",
            ConstraintKind::Select => "select",
            ConstraintKind::Number => "number",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConstraint {
    #[serde(rename = "type")]
    kind: ConstraintKind,
    #[serde(default)]
    on: Option<u16>,
    #[serde(default)]
    off: Option<u16>,
    #[serde(default)]
    options: Option<Vec<EnumOptions>>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    step: Option<f64>,
}

fn constraint_from_raw(raw: RawConstraint) -> Result<Constraint, String> {
    let unexpected = |what: &str, kind: &str| format!("{kind} constraint takes no {what}");
    match raw.kind {
        ConstraintKind::Switch => {
            let (Some(on), Some(off)) = (raw.on, raw.off) else {
                return Err("switch constraint requires on and off".to_string());
            };
            if raw.options.is_some() {
                return Err(unexpected("options", "switch"));
            }
            if raw.min.is_some() {
                return Err(unexpected("min", "switch"));
            }
            if raw.max.is_some() {
                return Err(unexpected("max", "switch"));
            }
            if raw.step.is_some() {
                return Err(unexpected("step", "switch"));
            }
            Ok(Constraint::Switch { on, off })
        }
        ConstraintKind::Select => {
            let Some(options) = raw.options else {
                return Err("select constraint requires options".to_string());
            };
            if options.is_empty() {
                return Err("select constraint requires a non-empty options list".to_string());
            }
            let values = options
                .iter()
                .map(|o| o.value)
                .collect::<std::collections::BTreeSet<_>>();
            if values.len() != options.len() {
                return Err("select constraint has duplicate option values".to_string());
            }
            if raw.on.is_some() {
                return Err(unexpected("on", "select"));
            }
            if raw.off.is_some() {
                return Err(unexpected("off", "select"));
            }
            if raw.min.is_some() {
                return Err(unexpected("min", "select"));
            }
            if raw.max.is_some() {
                return Err(unexpected("max", "select"));
            }
            if raw.step.is_some() {
                return Err(unexpected("step", "select"));
            }
            Ok(Constraint::Select { options })
        }
        ConstraintKind::Number => {
            let (Some(min), Some(max)) = (raw.min, raw.max) else {
                return Err("number constraint requires min and max".to_string());
            };
            if raw.on.is_some() {
                return Err(unexpected("on", "number"));
            }
            if raw.off.is_some() {
                return Err(unexpected("off", "number"));
            }
            if raw.options.is_some() {
                return Err(unexpected("options", "number"));
            }
            Ok(Constraint::Number {
                min,
                max,
                step: raw.step,
            })
        }
    }
}

fn parse_constraint<'de, D>(d: D) -> Result<Constraint, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = RawConstraint::deserialize(d)?;
    constraint_from_raw(raw).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlDef {
    pub name: String,
    pub address: u16,
    #[serde(deserialize_with = "parse_encoding")]
    pub encoding: Encoding,
    #[serde(default = "unit")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub writable: bool,
    #[serde(deserialize_with = "parse_constraint")]
    pub constraint: Constraint,
}

#[derive(Debug, thiserror::Error)]
pub enum MapError {
    #[error("read map file: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse map: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid map: {0}")]
    Invalid(String),
}

fn unit() -> f64 {
    1.0
}

impl Encoding {
    pub fn word_count(&self) -> usize {
        match self {
            Encoding::U16 | Encoding::S16 => 1,
            Encoding::U32 { .. } | Encoding::S32 { .. } => 2,
            Encoding::Str { words } => *words as usize,
        }
    }
}

impl RegisterMap {
    pub fn validate(&self) -> Result<(), MapError> {
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.sensors {
            if !seen.insert(s.name.clone()) {
                return Err(MapError::Invalid(format!(
                    "duplicate sensor name: {}",
                    s.name
                )));
            }
            if s.options.is_some() && s.encoding != Encoding::U16 {
                return Err(MapError::Invalid(format!(
                    "sensor {}: options requires u16 encoding (enum index is one register)",
                    s.name
                )));
            }
            if matches!(s.encoding, Encoding::Str { .. }) && s.options.is_some() {
                return Err(MapError::Invalid(format!(
                    "sensor {}: str decoding has no options",
                    s.name
                )));
            }
            if s.scale == 0.0 {
                return Err(MapError::Invalid(format!(
                    "sensor {}: scale must be nonzero",
                    s.name
                )));
            }
        }
        for c in &self.controls {
            if !seen.insert(c.name.clone()) {
                return Err(MapError::Invalid(format!(
                    "duplicate name (sensors/controls share the values namespace): {}",
                    c.name
                )));
            }
            if c.scale == 0.0 {
                return Err(MapError::Invalid(format!(
                    "control {}: scale must be nonzero",
                    c.name
                )));
            }
            if let Constraint::Number { min, max, .. } = &c.constraint
                && min >= max
            {
                return Err(MapError::Invalid(format!(
                    "control {}: min must be < max",
                    c.name
                )));
            }
            if let Constraint::Switch { on, off } = &c.constraint
                && on == off
            {
                return Err(MapError::Invalid(format!("control {}: on == off", c.name)));
            }
        }
        Ok(())
    }
}

pub fn load_from_str(s: &str) -> Result<RegisterMap, MapError> {
    let map: RegisterMap = toml::from_str(s)?;
    map.validate()?;
    Ok(map)
}

pub fn load(path: &str) -> Result<RegisterMap, MapError> {
    let s = std::fs::read_to_string(path)?;
    load_from_str(&s)
}
