//! End-to-end test of the WS surface (plan.md Task 2.1 expected outcome,
//! automated): hello + stream, ping/pong, cmd -> ack + broadcast push,
//! persistence, auth rejection.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, Spawn};
use serde_json::{Value as Json, json};
use solar_controller::bus::{BusActor, LinkConfig};
use solar_controller::directives::{Directives, DirectivesActor};
use solar_controller::map::{EMBEDDED_MAP, load_from_str};
use solar_controller::server::{AppState, serve};
use solar_controller::state::{StateActor, Update};
use solar_controller::telemetry::Snapshot;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn setup() -> (u16, tempfile::TempDir) {
    let (port, dir, _state) = setup_with(None).await;
    (port, dir)
}

/// Like [`setup`], but seeds `directives.json` first — the state a restarted bridge
/// finds — and hands back the state actor so a test can push a telemetry frame.
async fn setup_with(initial: Option<Json>) -> (u16, tempfile::TempDir, ActorRef<StateActor>) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(directives) = &initial {
        std::fs::write(
            dir.path().join("directives.json"),
            serde_json::to_vec_pretty(directives).unwrap(),
        )
        .unwrap();
    }
    let map = load_from_str(EMBEDDED_MAP).unwrap();

    let state = StateActor::spawn(StateActor::default());
    let directives = DirectivesActor::spawn(
        DirectivesActor::new(dir.path().join("directives.json"), state.clone()).unwrap(),
    );
    let meta = Arc::new(serde_json::to_value(&map).unwrap());
    let bus = BusActor::spawn(BusActor::new(
        LinkConfig {
            port: "/dev/null".into(),
            baud: 9600,
            slave: 1,
        },
        map,
        Duration::from_secs(5),
        state.clone(),
    ));
    let app = AppState {
        meta,
        state: state.clone(),
        directives,
        bus,
        token: "test-token".into(),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, app));
    (port, dir, state)
}

async fn recv_json(ws: &mut Ws) -> Json {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for a frame")
        .expect("stream ended")
        .expect("ws error");
    match msg {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
        other => panic!("unexpected frame: {other:?}"),
    }
}

#[tokio::test]
async fn hello_ping_cmd_ack_push_and_persistence() {
    let (port, dir) = setup().await;
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws?token=test-token"))
        .await
        .unwrap();

    let hello = recv_json(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["meta"]["sensors"].as_array().unwrap().len(), 90);
    assert_eq!(hello["meta"]["controls"].as_array().unwrap().len(), 65);
    assert_eq!(hello["state"]["link_ok"], false);
    assert_eq!(hello["directives"]["disarm"], false);

    ws.send(Message::text(json!({"type": "ping"}).to_string()))
        .await
        .unwrap();
    assert_eq!(recv_json(&mut ws).await["type"], "pong");

    ws.send(Message::text(
        json!({"type": "cmd", "id": "c1", "name": "disarm", "value": true}).to_string(),
    ))
    .await
    .unwrap();
    // cmd_ack (direct reply) and the broadcast state push arrive in
    // unspecified order.
    let (mut saw_ack, mut saw_state) = (false, false);
    for _ in 0..2 {
        let m = recv_json(&mut ws).await;
        match m["type"].as_str() {
            Some("cmd_ack") => {
                saw_ack = true;
                assert_eq!(m["id"], "c1");
                assert_eq!(m["ok"], true);
                assert_eq!(m["directives"]["disarm"], true);
            }
            Some("state") => {
                saw_state = true;
                assert_eq!(m["directives"]["disarm"], true);
            }
            other => panic!("unexpected message type: {other:?}"),
        }
    }
    assert!(saw_ack && saw_state);

    // persisted exactly as acked
    let persisted: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("directives.json")).unwrap())
            .unwrap();
    assert_eq!(persisted["disarm"], true);
    assert_eq!(persisted["version"], 1);
}

/// Regression (found live 2026-11-06): the directives loaded from `directives.json` went
/// into the `DirectivesActor` only; `StateActor` kept `Directives::default()`. So `hello`
/// carried the persisted file (version 4) and the *first telemetry frame* after it carried
/// the compile-time defaults (version 0) — HA showed a plant policy the bridge was not
/// using, and a restart looked like every setting had been lost. The state stream must
/// carry the loaded file, not the defaults.
#[tokio::test]
async fn persisted_directives_survive_the_first_telemetry_frame() {
    let persisted = json!({
        "version": 4,
        "planner_enabled": true,
        "actuator_enabled": true,
        "disarm": false,
        "soc_floor": 30.0,
        "soc_ceiling": 70.0,
        "max_power_kw": 9.0,
        "buy_distribution_czk": 2.4,
        "buy_seller_fee_czk": 0.35,
        "sell_fee_czk": 0.75,
        "forecast_calibration": 1.1,
        "energy_step_kwh": 0.32,
        "action_step_kw": 1.2
    });
    let (port, _dir, state) = setup_with(Some(persisted.clone())).await;
    // The file has no efficiency curves: they must load as the measured defaults, and the
    // wire carries the flat map (policy scalars + every curve coordinate).
    let loaded: Directives = serde_json::from_value(persisted.clone()).unwrap();
    let wire = loaded.wire_json();
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws?token=test-token"))
        .await
        .unwrap();
    let hello = recv_json(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["directives"], wire);

    // A pong proves the subscription is registered: the server subscribes before it
    // reads the next client frame, so anything after it is broadcast to us.
    ws.send(Message::text(json!({"type": "ping"}).to_string()))
        .await
        .unwrap();
    assert_eq!(recv_json(&mut ws).await["type"], "pong");

    // One telemetry frame is all it took to wipe the values off the HA screen.
    state
        .ask(Update::Good(Snapshot {
            link_ok: true,
            ts: 42,
            values: std::collections::BTreeMap::new(),
        }))
        .await
        .unwrap();
    let frame = recv_json(&mut ws).await;
    assert_eq!(frame["type"], "state");
    assert_eq!(
        frame["directives"], wire,
        "the state stream reset the persisted directives to the defaults"
    );
}

#[tokio::test]
async fn bad_token_rejected_before_upgrade() {
    let (port, _dir) = setup().await;
    assert!(
        connect_async(format!("ws://127.0.0.1:{port}/ws?token=wrong"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_cmd_acks_with_error_and_malformed_input_gets_error() {
    let (port, _dir) = setup().await;
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws?token=test-token"))
        .await
        .unwrap();
    let _ = recv_json(&mut ws).await; // hello

    ws.send(Message::text(
        json!({"type": "cmd", "id": "c2", "name": "soc_target_hold", "value": 150}).to_string(),
    ))
    .await
    .unwrap();
    let m = recv_json(&mut ws).await;
    assert_eq!(m["type"], "cmd_ack");
    assert_eq!(m["ok"], false);
    assert!(m["error"].as_str().is_some());

    ws.send(Message::text("garbage".to_string())).await.unwrap();
    assert_eq!(recv_json(&mut ws).await["type"], "error");
}

#[tokio::test]
async fn write_unknown_control_rejects_without_io() {
    let (port, _dir) = setup().await;
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws?token=test-token"))
        .await
        .unwrap();
    let _ = recv_json(&mut ws).await; // hello

    ws.send(Message::text(
        json!({"type": "write", "id": "w1", "name": "no_such_control", "value": 1}).to_string(),
    ))
    .await
    .unwrap();
    let m = recv_json(&mut ws).await;
    assert_eq!(m["type"], "write_ack");
    assert_eq!(m["id"], "w1");
    assert_eq!(m["ok"], false);
    assert!(m["error"].as_str().unwrap().contains("unknown control"));
}

#[tokio::test]
async fn write_valid_control_surfaces_bus_connect_failure() {
    // The test env has no serial link: a well-formed write must still route
    // through BusActor and come back as ok=false with the connect failure.
    let (port, _dir) = setup().await;
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{port}/ws?token=test-token"))
        .await
        .unwrap();
    let _ = recv_json(&mut ws).await; // hello

    ws.send(Message::text(
        json!({"type": "write", "id": "w2", "name": "batt_max_charge_current", "value": 100})
            .to_string(),
    ))
    .await
    .unwrap();
    let m = recv_json(&mut ws).await;
    assert_eq!(m["type"], "write_ack");
    assert_eq!(m["id"], "w2");
    assert_eq!(m["ok"], false);
    assert!(m["error"].as_str().unwrap().contains("connect"));
}

/// 2026-11-04T00:00 Prague; one export MTU is enough — these tests are about `/plan.json`,
/// not the plan's contents.
fn one_step_plan() -> solar_controller::planner::Plan {
    use solar_controller::planner::{Action, Mtu, Plan, PlanStep, Reason};
    use solar_controller::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};
    Plan {
        computed_at: 1_793_746_800,
        steps: vec![PlanStep {
            mtu: Mtu {
                start: 1_793_746_800,
                hours: Hours::new(0.25),
            },
            price_sell: CzkPerKwh::new(7.9),
            price_buy: CzkPerKwh::new(3.24),
            pv_kw: DcKw::new(2.0),
            action: Action::Export { kw: AcKw::new(8.4) },
            battery_out_kwh: DcKwh::new(1.5),
            ac_kwh: AcKwh::new(1.985),
            energy_after_kwh: DcKwh::new(20.0),
            net_czk: Czk::new(15.68),
            marginal_czk_per_kwh: CzkPerKwh::new(5.32),
            reason: Reason::PriceAboveStoredValue {
                price_sell: CzkPerKwh::new(7.9),
                marginal: CzkPerKwh::new(5.32),
            },
        }],
        expected_net_czk: Czk::new(15.68),
        horizon_end: 1_793_746_800 + 900,
    }
}

/// 2026-11-09 decision: Grafana is the only surface, so the bridge must serve the two things
/// a dashboard cannot get from Home Assistant — the metrics family (prices, forecast, plan)
/// and the horizon itself. Both are pure views of the state actor: a scrape never moves the
/// plant, and `/plan.json` is the flat projection of the plan.
#[tokio::test]
async fn metrics_and_plan_json_are_views_of_the_same_state() {
    use solar_controller::decode::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(port: u16, path: &str) -> (u16, String, String) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the bridge is listening");
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .expect("a status line")
            .parse()
            .expect("a status code");
        (status, head.to_string(), body.to_string())
    }

    let (port, _dir, state) = setup_with(None).await;

    let mut values = std::collections::BTreeMap::new();
    values.insert("pv_energy_total".to_string(), Value::Number(1112.1));
    values.insert(
        "grid_internal_power_total".to_string(),
        Value::Number(-8650.0),
    );
    state
        .ask(Update::Good(Snapshot {
            link_ok: true,
            ts: 1_793_746_800,
            values,
        }))
        .await
        .unwrap();
    state
        .ask(Update::Plan(Some(one_step_plan())))
        .await
        .unwrap();

    let (status, head, body) = get(port, "/metrics").await;
    assert_eq!(status, 200);
    assert!(
        head.to_lowercase()
            .contains("content-type: text/plain; version=0.0.4"),
        "Prometheus needs the exposition format in the type: {head}"
    );
    assert!(body.contains("# TYPE solar_pv_energy_kwh_total counter"));
    assert!(body.contains("solar_pv_energy_kwh_total 1112.1"));
    // The meter keeps its own sign; the plan's export-positive convention stays a planner
    // internal.
    assert!(body.contains("solar_grid_power_w -8650"));
    // A plan in the past still reports its age and its expected money, but no "in force"
    // price: a stale plan must not masquerade as the current one.
    assert!(body.contains("solar_plan_net_czk_horizon 15.68"));
    assert!(!body.contains("solar_price_sell_czk_per_kwh"));

    let (status, head, body) = get(port, "/plan.json").await;
    assert_eq!(status, 200);
    assert!(
        head.to_lowercase()
            .contains("content-type: application/json"),
        "the horizon is a JSON document: {head}"
    );
    let view: Json = serde_json::from_str(&body).expect("/plan.json is JSON");
    assert_eq!(view["steps"][0]["price_sell"], 7.9);
    assert_eq!(view["steps"][0]["command_kw"], 8.4);
    assert_eq!(view["steps"][0]["kind"], "export");
    assert!(
        view.get("adherence").is_none(),
        "the payload is the plan alone: the measured side lives in Prometheus"
    );
    assert!(
        view["computed_at"].is_i64(),
        "the plan's own age rides the payload so a dashboard can say how stale it is"
    );
}
