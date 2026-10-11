use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, bail};
use clap::{Parser, Subcommand};
use kameo::actor::{ActorRef, Spawn};
use solar_controller::{
    bus::{BusActor, LinkConfig, POLL_INTERVAL, Tick}, config, directives::{DirectivesActor, directive_schema}, map::{EMBEDDED_MAP, RegisterMap, load_from_str}, probe, server::{self, AppState}, state::{GetState, StateActor},
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(short, long, global = true)]
    config: Option<String>,
    #[arg(short, long, default_value = "/dev/ttyUSB0", global = true)]
    port: String,
    #[arg(short, long, default_value = "1", global = true)]
    slave: u8,
    #[arg(short, long, default_value = "9600", global = true)]
    baud: u32,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Probe,
    Run,
    Dump,
}

fn link_and_interval(
    config_path: &Option<String>,
    port: String,
    baud: u32,
    slave: u8,
) -> anyhow::Result<(LinkConfig, Duration)> {
    match config_path {
        Some(path) => {
            let cfg = config::load(path)?;
            Ok((
                LinkConfig {
                    port: cfg.port,
                    baud: cfg.baud,
                    slave: cfg.slave,
                },
                Duration::from_secs(cfg.interval_secs),
            ))
        }
        None => Ok((LinkConfig { port, baud, slave }, POLL_INTERVAL)),
    }
}

fn spawn_actors(
    link: LinkConfig,
    map: RegisterMap,
    interval: Duration,
) -> (ActorRef<BusActor>, ActorRef<StateActor>) {
    let state_ref = StateActor::spawn(StateActor::default());
    let bus_ref = BusActor::spawn(BusActor::new(link, map, interval, state_ref.clone()));
    (bus_ref, state_ref)
}

async fn drive(bus: ActorRef<BusActor>, interval: Duration) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        bus.tell(Tick)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("bus actor died"))?;
    }
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    if cfg.token.is_empty() {
        bail!(
            "config: token must be non-empty — the server would bind {} unauthenticated",
            cfg.bind
        );
    }
    let map = load_from_str(EMBEDDED_MAP)?;
    let mut meta = serde_json::to_value(&map)?;
    meta["directives"] = directive_schema();
    let meta = Arc::new(meta);

    let state_ref = StateActor::spawn(StateActor::default());
    let directives_ref = DirectivesActor::spawn(DirectivesActor::new(
        PathBuf::from(&cfg.directives_path),
        state_ref.clone(),
    )?);
    let interval = Duration::from_secs(cfg.interval_secs);
    let bus_ref = BusActor::spawn(BusActor::new(
        LinkConfig {
            port: cfg.port.clone(),
            baud: cfg.baud,
            slave: cfg.slave,
        },
        map,
        interval,
        state_ref.clone(),
    ));

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    let app = AppState {
        meta,
        state: state_ref,
        directives: directives_ref,
        bus: bus_ref.clone(),
        token: cfg.token,
    };
    tokio::spawn(server::serve(listener, app));

    drive(bus_ref, interval).await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (link, interval) = link_and_interval(&cli.config, cli.port, cli.baud, cli.slave)?;

    match cli.command {
        Some(Commands::Probe) => probe::run(&link).await?,
        Some(Commands::Run) => {
            let Some(cfg_path) = cli.config.as_deref() else {
                bail!("run requires --config (bind/token/directives_path live there)");
            };
            run(config::load(cfg_path)?).await?;
        }
        Some(Commands::Dump) => {
            let map = load_from_str(EMBEDDED_MAP)?;
            let (bus_ref, state_ref) = spawn_actors(link, map, interval);

            bus_ref
                .ask(Tick)
                .await
                .map_err(|e| anyhow!("bus actor died: {e}"))?;

            let status = state_ref
                .ask(GetState)
                .await
                .map_err(|e| anyhow!("state actor died: {e}"))?;

            println!("{}", serde_json::to_string_pretty(&status)?);
            if !status.link_ok {
                bail!("link down");
            }
        }
        None => {}
    }

    Ok(())
}
