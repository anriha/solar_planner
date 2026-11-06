use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, bail};
use clap::{Parser, Subcommand};
use kameo::actor::{ActorRef, Spawn};
use solar_controller::{
    bus::{BusActor, LinkConfig, POLL_INTERVAL, Tick, WriteRegister},
    config,
    directives::{Directives, DirectivesActor, GetCurrent, directive_schema},
    map::{EMBEDDED_MAP, RegisterMap, load_from_str},
    planner::{
        actor::{PLANNER_INTERVAL, PlannerActor, drive as planner_drive, horizon_params},
        actuator,
        forecast::{ForecastSolar, Site},
        horizon::HorizonActor,
        price::OteClient,
    },
    probe,
    server::{self, AppState},
    state::{GetState, StateActor},
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

/// Ctrl-C or SIGTERM: the point of the signal is to leave the inverter in the safe state
/// before the process disappears. Registers survive a restart; a `selling_first` armed at
/// shutdown would not.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Best-effort safe state on the way out: the globals only, each with the bus actor's own
/// timeout, so a dead bus cannot hang the shutdown.
///
/// **Only when the actuator was enabled.** A report-only deploy writes nothing, so there is
/// nothing to undo — and disarming anyway put a plant the planner never touched into
/// zero-export: on 2026-11-06 a restart with `actuator_enabled = false` wrote
/// `work_mode = zero_export_load`, `max_sell_power = 0` and `solar_sell = 0` at 13:33:33.
/// That is a write nobody asked for, from the one component whose promise is "off means the
/// inverter is untouched". An unreadable directive state still disarms: unknown is not off.
async fn disarm_on_shutdown(bus: &ActorRef<BusActor>, directives: &ActorRef<DirectivesActor>) {
    let enabled = match directives.ask(GetCurrent).await {
        Ok(current) => Some(current.actuator_enabled),
        Err(error) => {
            eprintln!("[main] directives unavailable ({error}); disarming anyway");
            None
        }
    };
    if !actuator::shutdown_disarms(enabled) {
        eprintln!("[main] actuator disabled; leaving the inverter as it is");
        return;
    }
    for target in actuator::shutdown_targets() {
        let _ = bus
            .ask(WriteRegister {
                name: target.name,
                value: target.write,
            })
            .await;
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
    eprintln!(
        "[main] bridge on {} (port {}, poll {}s), directives {}",
        cfg.bind, cfg.port, cfg.interval_secs, cfg.directives_path
    );

    // The planner: its own feeds, its own minute tick, publishing through the state actor.
    // It is not on the poll path — a slow OTE call must never delay a Modbus cycle.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let horizon = HorizonActor::new(
        OteClient::new(http.clone()),
        ForecastSolar::new(
            http,
            Site {
                latitude: cfg.site.latitude,
                longitude: cfg.site.longitude,
                declination: cfg.site.declination,
                azimuth: cfg.site.azimuth,
                kwp: cfg.site.kwp,
            },
        ),
        horizon_params(&Directives::default()),
    );
    let planner = PlannerActor::spawn(PlannerActor::new(
        horizon,
        state_ref.clone(),
        directives_ref.clone(),
        Some(bus_ref.clone()),
    ));
    tokio::spawn(async move {
        if let Err(error) = planner_drive(planner, PLANNER_INTERVAL).await {
            eprintln!("[planner] driver stopped: {error}");
        }
    });
    let app = AppState {
        meta,
        state: state_ref,
        directives: directives_ref.clone(),
        bus: bus_ref.clone(),
        token: cfg.token,
    };
    tokio::spawn(server::serve(listener, app));

    tokio::select! {
        result = drive(bus_ref.clone(), interval) => result,
        _ = shutdown_signal() => {
            eprintln!("[main] shutting down");
            disarm_on_shutdown(&bus_ref, &directives_ref).await;
            Ok(())
        }
    }
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
