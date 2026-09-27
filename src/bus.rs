use anyhow::anyhow;
use kameo::{Actor, actor::ActorRef, message::Message};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use tokio_modbus::{
    Slave,
    client::{Context, Reader},
};
use tokio_serial::SerialPortBuilderExt;

use crate::{
    decode::decode,
    map::RegisterMap,
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

        let value = decode(sensor, &words)
            .map_err(|e| anyhow!("sensor {} @{}: {e}", sensor.name, sensor.address))?;

        snapshot_values.insert(sensor.name.clone(), value);
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
