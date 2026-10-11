use crate::map::{BaseWord, ControlDef, Constraint, Encoding, EnumOptions, SensorDef};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum Value {
    Number(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("expected {expected} words, got {got}")]
    WordCount { expected: usize, got: usize },
    #[error("no option mapping for raw value {0}")]
    UnknownOption(u16),
    #[error("register string is not valid ASCII")]
    InvalidAscii,
}

/// Shared decode input: the fields SensorDef and ControlDef have in common
/// (decision 4a, 2026-10-11). Options come from `SensorDef.options` or a
/// control's Select constraint, so enum readbacks decode to labels either way.
pub struct Field<'a> {
    pub name: &'a str,
    pub address: u16,
    pub encoding: Encoding,
    pub scale: f64,
    pub offset: f64,
    pub options: Option<&'a [EnumOptions]>,
}

impl<'a> From<&'a SensorDef> for Field<'a> {
    fn from(s: &'a SensorDef) -> Self {
        Self {
            name: &s.name,
            address: s.address,
            encoding: s.encoding,
            scale: s.scale,
            offset: s.offset,
            options: s.options.as_deref(),
        }
    }
}

impl<'a> From<&'a ControlDef> for Field<'a> {
    fn from(c: &'a ControlDef) -> Self {
        Self {
            name: &c.name,
            address: c.address,
            encoding: c.encoding,
            scale: c.scale,
            offset: c.offset,
            options: match &c.constraint {
                Constraint::Select { options } => Some(options),
                _ => None,
            },
        }
    }
}

pub fn decode_field(field: &Field, words: &[u16]) -> Result<Value, DecodeError> {
    if words.len() != field.encoding.word_count() {
        return Err(DecodeError::WordCount {
            expected: field.encoding.word_count(),
            got: words.len(),
        });
    }
    if let Some(options) = field.options {
        let raw = words[0];
        return options
            .iter()
            .find(|o| o.value == raw)
            .map(|o| Value::Text(o.label.clone()))
            .ok_or(DecodeError::UnknownOption(raw));
    }

    let raw: f64 = match field.encoding {
        Encoding::U16 => words[0] as f64,
        Encoding::S16 => words[0] as i16 as f64,
        Encoding::U32 { base_word } => assemble(words, base_word) as f64,
        Encoding::S32 { base_word } => assemble(words, base_word) as i32 as f64,
        Encoding::Str { .. } => {
            let bytes: Vec<u8> = words
                .iter()
                .flat_map(|w| [(w >> 8) as u8, *w as u8])
                .collect();
            let s = String::from_utf8_lossy(&bytes); // never fails for u8 ASCII check below
            let trimmed = s.trim_end_matches('\0');
            if !trimmed.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                return Err(DecodeError::InvalidAscii);
            }
            return Ok(Value::Text(trimmed.to_string()));
        }
    };
    Ok(Value::Number((raw - field.offset) * field.scale))
}

/// Decode a sensor def (frozen contract wrapper — tests/decode.rs vectors).
pub fn decode(def: &SensorDef, words: &[u16]) -> Result<Value, DecodeError> {
    decode_field(&Field::from(def), words)
}

fn assemble(words: &[u16], base: BaseWord) -> u32 {
    let (high, low) = match base {
        BaseWord::High => (words[0], words[1]),
        BaseWord::Low => (words[1], words[0]),
    };
    (high as u32) << 16 | low as u32
}

// Write encoding lives bus-side (decision 2b, 2026-10-11): see bus::encode_write.