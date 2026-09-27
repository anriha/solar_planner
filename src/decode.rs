use crate::map::{BaseWord, Encoding, SensorDef};

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

pub fn decode(def: &SensorDef, words: &[u16]) -> Result<Value, DecodeError> {
    if words.len() != def.encoding.word_count() {
        return Err(DecodeError::WordCount {
            expected: def.encoding.word_count(),
            got: words.len(),
        });
    }
    if let Some(options) = &def.options {
        let raw = words[0];
        return options
            .iter()
            .find(|o| o.value == raw)
            .map(|o| Value::Text(o.label.clone()))
            .ok_or(DecodeError::UnknownOption(raw));
    }

    let raw: f64 = match def.encoding {
        Encoding::U16 => words[0]  as f64,
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
    Ok(Value::Number((raw - def.offset) * def.scale))
}

fn assemble(words: &[u16], base: BaseWord) -> u32 {
    let (high, low) = match base {
        BaseWord::High => (words[0], words[1]),
        BaseWord::Low => (words[1], words[0]),
    };
    (high as u32) << 16 | low as u32
}
