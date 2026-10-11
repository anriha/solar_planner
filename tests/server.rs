//! End-to-end test of the WS surface (plan.md Task 2.1 expected outcome,
//! automated): hello + stream, ping/pong, cmd -> ack + broadcast push,
//! persistence, auth rejection.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use kameo::actor::Spawn;
use serde_json::{Value as Json, json};
use solar_controller::bus::{BusActor, LinkConfig};
use solar_controller::directives::DirectivesActor;
use solar_controller::map::{EMBEDDED_MAP, load_from_str};
use solar_controller::server::{AppState, serve};
use solar_controller::state::StateActor;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn setup() -> (u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
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
        state,
        directives,
        bus,
        token: "test-token".into(),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, app));
    (port, dir)
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
    assert_eq!(hello["meta"]["controls"].as_array().unwrap().len(), 64);
    assert_eq!(hello["state"]["link_ok"], false);
    assert_eq!(hello["directives"]["force_mode"], "auto");

    ws.send(Message::text(json!({"type": "ping"}).to_string()))
        .await
        .unwrap();
    assert_eq!(recv_json(&mut ws).await["type"], "pong");

    ws.send(Message::text(
        json!({"type": "cmd", "id": "c1", "name": "force_mode", "value": "charge"}).to_string(),
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
                assert_eq!(m["directives"]["force_mode"], "charge");
            }
            Some("state") => {
                saw_state = true;
                assert_eq!(m["directives"]["force_mode"], "charge");
            }
            other => panic!("unexpected message type: {other:?}"),
        }
    }
    assert!(saw_ack && saw_state);

    // persisted exactly as acked
    let persisted: Json = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("directives.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["force_mode"], "charge");
    assert_eq!(persisted["version"], 1);
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
