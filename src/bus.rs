use anyhow::anyhow;
use kameo::{Actor, actor::ActorRef, message::Message};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use tokio_modbus::{
    Slave,
    client::{Context, Reader, Writer},
};
use tokio_serial::SerialPortBuilderExt;

use crate::{
    decode::{Field, decode_field},
    map::{Constraint, ControlDef, Encoding, RegisterMap},
    spans::{MERGE_GAP, ReadSpan, plan_spans},
    state::{StateActor, Update},
    telemetry::{Snapshot, unix_now},
};

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
pub struct Tick;

pub async fn poll_once(
    ctx: &mut Context,
    map: &RegisterMap,
    spans: &[ReadSpan],
) -> anyhow::Result<Snapshot> {
    let mut reads: Vec<(ReadSpan, Vec<u16>)> = Vec::with_capacity(spans.len());
    for span in spans {
        let words = read_span(ctx, span.start, span.len)
            .await
            .map_err(|e| anyhow!("span {}+{}: {e}", span.start, span.len))?;
        reads.push((*span, words));
    }
    decode_all(map, &reads)
}

pub fn decode_all(map: &RegisterMap, reads: &[(ReadSpan, Vec<u16>)]) -> anyhow::Result<Snapshot> {
    let address_values: BTreeMap<u16, u16> = reads
        .iter()
        .flat_map(|(span, words)| {
            words
                .iter()
                .enumerate()
                .map(|(i, read_value)| (span.start + (i as u16), *read_value))
        })
        .collect();

    let mut snapshot_values = BTreeMap::new();
    for sensor in &map.sensors {
        let count = sensor.encoding.word_count() as u16;
        let words: Vec<u16> = (sensor.address..sensor.address + count)
            .map(|addr| {
                address_values.get(&addr).copied().ok_or_else(|| {
                    anyhow!(
                        "sensor {} @{}: register {addr} not covered by any span",
                        sensor.name,
                        sensor.address
                    )
                })
            })
            .collect::<anyhow::Result<_>>()?;

        let value = decode_field(&Field::from(sensor), &words)
            .map_err(|e| anyhow!("sensor {} @{}: {e}", sensor.name, sensor.address))?;

        snapshot_values.insert(sensor.name.clone(), value);
    }

    // Controls (settings readbacks) join `values` under their own names
    // (decision 2026-10-11 #1b), via the shared Field decoder (decision 4a).
    // Same loud-failure policy as sensors (decision 2026-09-28 #1): any
    // missing register or decode error fails the whole cycle.
    for control in &map.controls {
        let field = Field::from(control);
        let count = field.encoding.word_count() as u16;
        let words: Vec<u16> = (control.address..control.address + count)
            .map(|addr| {
                address_values.get(&addr).copied().ok_or_else(|| {
                    anyhow!(
                        "control {} @{}: register {addr} not covered by any span",
                        control.name,
                        control.address
                    )
                })
            })
            .collect::<anyhow::Result<_>>()?;

        let value = decode_field(&field, &words)
            .map_err(|e| anyhow!("control {} @{}: {e}", control.name, control.address))?;

        snapshot_values.insert(control.name.clone(), value);
    }

    Ok(Snapshot {
        link_ok: true,
        ts: unix_now(),
        values: snapshot_values,
    })
}

pub fn backoff_delay(interval: Duration, fails: u32) -> Duration {
    const CAP: Duration = Duration::from_secs(60);
    if fails <= 1 {
        return Duration::ZERO;
    }
    let exp = (fails - 2).min(6);
    interval.saturating_mul(2u32.pow(exp)).min(CAP)
}

#[derive(Actor)]
pub struct BusActor {
    cfg: LinkConfig,
    map: RegisterMap,
    spans: Vec<ReadSpan>,
    interval: Duration,
    ctx: Option<Context>,
    state: ActorRef<StateActor>,
    fails: u32,
    next_attempt: Instant,
}

impl BusActor {
    pub fn new(
        cfg: LinkConfig,
        map: RegisterMap,
        interval: Duration,
        state: ActorRef<StateActor>,
    ) -> Self {
        let spans = plan_spans(&map, MERGE_GAP);
        Self {
            cfg,
            map,
            spans,
            interval,
            ctx: None,
            state,
            fails: 0,
            next_attempt: Instant::now(),
        }
    }

    async fn record_failure(&mut self, e: anyhow::Error) {
        eprintln!("[bus] {e}");
        self.ctx = None;
        self.fails += 1;
        self.next_attempt = Instant::now() + backoff_delay(self.interval, self.fails);
        let _ = self.state.tell(Update::Down).send().await;
    }
}

impl Message<Tick> for BusActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Tick,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if Instant::now() < self.next_attempt {
            return;
        }

        if self.ctx.is_none() {
            match connect(&self.cfg) {
                Ok(ctx) => self.ctx = Some(ctx),
                Err(e) => {
                    self.record_failure(anyhow!("connect: {e}")).await;
                    return;
                }
            }
        }

        let ctx = self.ctx.as_mut().expect("ctx was just set");

        match poll_once(ctx, &self.map, &self.spans).await {
            Ok(snap) => {
                self.fails = 0;
                self.next_attempt = Instant::now();
                let _ = self.state.tell(Update::Good(snap)).send().await;
            }
            Err(e) => self.record_failure(e).await,
        }
    }
}

pub struct LinkConfig {
    pub port: String,
    pub baud: u32,
    pub slave: u8,
}

pub fn connect(cfg: &LinkConfig) -> anyhow::Result<Context> {
    let stream = tokio_serial::new(cfg.port.clone(), cfg.baud).open_native_async()?;
    Ok(tokio_modbus::client::rtu::attach_slave(
        stream,
        Slave(cfg.slave),
    ))
}

pub async fn read_span(ctx: &mut Context, addr: u16, cnt: u16) -> anyhow::Result<Vec<u16>> {
    tokio::time::timeout(
        Duration::from_secs(2),
        ctx.read_holding_registers(addr, cnt),
    )
    .await
    .map_err(|_| anyhow!("no response within 2s"))??
    .map_err(|e| anyhow!("slave exception {e:?}"))
}

// --- Register write path (D1–D5, 2026-10-11; decisions 2b/3a/1b) -----------
// BusActor owns validation/encoding (2b): the ws reader stays thin, and the
// mailbox serializes writes with polls (single-master discipline). encode_write
// is the pure, tested core; the handler adds IO + readback verify (D5).

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("unknown control '{0}'")]
    Unknown(String),
    #[error("{name} @ {address}: not writable")]
    NotWritable { name: String, address: u16 },
    #[error("{name} @ {address}: {reason}")]
    Invalid { name: String, address: u16, reason: String },
    #[error("{name} @ {address}: wrote {written:#06x}, read back {read:#06x}")]
    Readback { name: String, address: u16, written: u16, read: u16 },
    #[error("{name} @ {address}: {source}")]
    Io {
        name: String,
        address: u16,
        source: anyhow::Error,
    },
}

/// Validate a write (D4: reject, never clamp; 3a: reject non-representable;
/// 1b: select value = option's u16 value) and encode it to the raw register
/// word. Inverse of decode: value = (raw - offset) * scale.
pub fn encode_write(control: &ControlDef, value: &serde_json::Value) -> Result<u16, WriteError> {
    let invalid = |reason: String| WriteError::Invalid {
        name: control.name.clone(),
        address: control.address,
        reason,
    };
    if !control.writable {
        return Err(WriteError::NotWritable {
            name: control.name.clone(),
            address: control.address,
        });
    }
    if !matches!(control.encoding, Encoding::U16 | Encoding::S16) {
        return Err(invalid("only u16/s16 controls are writable".into()));
    }
    let raw: i64 = match &control.constraint {
        Constraint::Switch { on, off } => match value {
            serde_json::Value::Bool(true) => i64::from(*on),
            serde_json::Value::Bool(false) => i64::from(*off),
            _ => return Err(invalid("expected true/false".into())),
        },
        Constraint::Select { options } => {
            let Some(v) = value.as_u64() else {
                return Err(invalid("expected an option value (u16 index)".into()));
            };
            if !options.iter().any(|o| u64::from(o.value) == v) {
                return Err(invalid(format!("value {v} is not one of the options")));
            }
            v as i64
        }
        Constraint::Number { min, max, .. } => {
            let Some(x) = value.as_f64() else {
                return Err(invalid("expected a number".into()));
            };
            if !(*min..=*max).contains(&x) {
                return Err(invalid(format!(
                    "{x} outside {min}..={max} (rejected, never clamped)"
                )));
            }
            let scaled = x / control.scale + control.offset;
            // 1e-6 tolerance: float artifacts of the division round to the
            // correct register; genuinely non-representable values reject (3a).
            if (scaled - scaled.round()).abs() > 1e-6 {
                return Err(invalid(format!(
                    "{x} is not representable (scale {}, offset {})",
                    control.scale, control.offset
                )));
            }
            let r = scaled.round() as i64;
            let (lo, hi) = match control.encoding {
                Encoding::S16 => (-0x8000_i64, 0x7FFF_i64),
                _ => (0_i64, 0xFFFF_i64),
            };
            if !(lo..=hi).contains(&r) {
                return Err(invalid(format!("encoded raw {r} outside {lo}..={hi}")));
            }
            r
        }
    };
    Ok(raw as u16) // s16 negatives wrap to two's complement (range-checked above)
}

pub struct WriteRegister {
    pub name: String,
    pub value: serde_json::Value,
}

async fn write_register(ctx: &mut Context, addr: u16, raw: u16) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), ctx.write_single_register(addr, raw))
        .await
        .map_err(|_| anyhow!("no response within 2s"))??
        .map_err(|e| anyhow!("slave exception {e:?}"))
}

impl Message<WriteRegister> for BusActor {
    type Reply = Result<(), WriteError>;

    async fn handle(
        &mut self,
        msg: WriteRegister,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let control = self
            .map
            .controls
            .iter()
            .find(|c| c.name == msg.name)
            .ok_or_else(|| WriteError::Unknown(msg.name.clone()))?
            .clone();
        let raw = encode_write(&control, &msg.value)?;

        if self.ctx.is_none() {
            match connect(&self.cfg) {
                Ok(ctx) => self.ctx = Some(ctx),
                Err(e) => {
                    self.record_failure(anyhow!("connect: {e}")).await;
                    return Err(WriteError::Io {
                        name: control.name,
                        address: control.address,
                        source: anyhow!("connect failed (see journal)"),
                    });
                }
            }
        }
        let ctx = self.ctx.as_mut().expect("ctx was just set");

        let outcome = async {
            write_register(ctx, control.address, raw).await?;
            let read = read_span(ctx, control.address, 1).await?;
            Ok::<u16, anyhow::Error>(read[0])
        }
        .await;

        match outcome {
            Ok(read) if read == raw => {
                self.fails = 0;
                self.next_attempt = Instant::now();
                Ok(())
            }
            Ok(read) => Err(WriteError::Readback {
                name: control.name,
                address: control.address,
                written: raw,
                read,
            }),
            Err(e) => {
                let display = e.to_string();
                self.record_failure(e).await;
                Err(WriteError::Io {
                    name: control.name,
                    address: control.address,
                    source: anyhow!("{display}"),
                })
            }
        }
    }
}
