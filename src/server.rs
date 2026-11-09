use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{
        ConnectInfo, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    extract::Path as UrlPath,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::ActorRef;

use crate::{
    bus::{BusActor, WriteRegister},
    directives::{ApplyDirective, Directives, DirectivesActor, GetCurrent},
    protocol::{ClientMsg, ServerMsg},
    state::{GetState, StateActor, Subscribe, Unsubscribe, WatchPlan},
    view::View,
};

/// Outbound frames buffered per connection. At the 5 s poll this is ~2.7 min
/// of backlog before StateActor starts skipping frames for a slow socket
/// (skipping, never unsubscribing — see `StateActor::broadcast`).
const CHANNEL_CAP: usize = 32;

/// One frame must be handed to the kernel within this window. A peer that
/// stops reading for longer is gone for practical purposes — the 2026-11-03
/// HA connection sat behind a multi-minute path stall and then never came
/// back. Closing the socket makes the client see it and reconnect.
const SEND_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct AppState {
    pub meta: Arc<serde_json::Value>,
    pub state: ActorRef<StateActor>,
    pub directives: ActorRef<DirectivesActor>,
    pub bus: ActorRef<BusActor>,
    pub token: String,
}

pub async fn serve(listener: tokio::net::TcpListener, app: AppState) -> anyhow::Result<()> {
    let router = Router::new()
        .route("/ws", get(ws_handler))
        .route("/state", get(state_handler))
        .route("/meta", get(meta_handler))
        .route("/directives", get(directives_handler))
        // The viewer: a static shell plus its wasm, same origin as the socket so there is no
        // CORS and no mixed content (board-b0o).
        .route("/", get(root_redirect))
        // The viewer: a browser shell plus its wasm, served out of this binary's own memory
        // (board-b0o). Same origin as the socket, so no CORS and no mixed content.
        .route("/app", get(app_root))
        .route("/app/", get(app_index))
        .route("/app/{*path}", get(app_asset))
        .with_state(app);
    // Connect info powers the peer address in the connection log lines.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn authorized(token: &str, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    let header_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == token);
    let query_ok = query.get("token").is_some_and(|t| t == token);
    header_ok || query_ok
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if !authorized(&app.token, &headers, &query) {
        eprintln!("[server] {peer}: rejected, bad token");
        return (StatusCode::UNAUTHORIZED, "invalid or missing token\n").into_response();
    }
    ws.on_upgrade(move |socket| connection(app, socket, peer))
}

async fn connection(app: AppState, mut socket: WebSocket, peer: SocketAddr) {
    let hello = match (
        app.state.ask(GetState).await,
        app.directives.ask(GetCurrent).await,
    ) {
        (Ok(state), Ok(directives)) => ServerMsg::Hello {
            meta: (*app.meta).clone(),
            state,
            directives: directives.wire_json(),
        },
        _ => return, // actor death = process is going down anyway
    };
    if socket
        .send(Message::Text(hello.to_text().into()))
        .await
        .is_err()
    {
        return;
    }

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ServerMsg>(CHANNEL_CAP);
    let ack_tx = tx.clone();
    let Ok(conn_id) = app.state.ask(Subscribe { tx }).await else {
        return;
    };
    eprintln!("[server] {peer}: connected as conn {conn_id}");

    // Single writer owns the sink; on exit it unsubscribes so a dead
    // connection can't leak a channel sender in StateActor.
    let writer_state = app.state.clone();
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Text(msg.to_text().into())))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("[server] {peer}: send failed: {e}");
                    break;
                }
                Err(_) => {
                    eprintln!("[server] {peer}: not reading for {SEND_TIMEOUT:?}, dropping");
                    break;
                }
            }
        }
        let _ = writer_state.tell(Unsubscribe(conn_id)).send().await;
    });

    loop {
        let text = tokio::select! {
            // Writer gone: the socket can never carry state again, so close
            // it instead of leaving a peer that looks connected but is deaf.
            _ = &mut writer => {
                eprintln!("[server] {peer}: writer stopped, closing");
                break;
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(t))) => t,
                // axum answers protocol-level pings itself; the rest is ignored.
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    eprintln!("[server] {peer}: read error: {e}");
                    break;
                }
                None => break,
            },
        };
        let reply = match serde_json::from_str::<ClientMsg>(text.as_str()) {
            Ok(ClientMsg::Ping) => ServerMsg::Pong,
            Ok(ClientMsg::Cmd { id, name, value }) => {
                match app.directives.ask(ApplyDirective { name, value }).await {
                    Ok(directives) => ServerMsg::CmdAck {
                        id,
                        ok: true,
                        error: None,
                        directives: directives.wire_json(),
                    },
                    Err(kameo::error::SendError::HandlerError(e)) => ServerMsg::CmdAck {
                        id,
                        ok: false,
                        error: Some(e.to_string()),
                        directives: app
                            .directives
                            .ask(GetCurrent)
                            .await
                            .map(|d| d.wire_json())
                            .unwrap_or_else(|_| Directives::default().wire_json()),
                    },
                    Err(_) => ServerMsg::Error {
                        message: "directives actor unavailable".into(),
                    },
                }
            }
            Ok(ClientMsg::Write { id, name, value }) => {
                match app.bus.ask(WriteRegister { name, value }).await {
                    Ok(()) => ServerMsg::WriteAck {
                        id,
                        ok: true,
                        error: None,
                    },
                    Err(kameo::error::SendError::HandlerError(e)) => ServerMsg::WriteAck {
                        id,
                        ok: false,
                        error: Some(e.to_string()),
                    },
                    Err(_) => ServerMsg::Error {
                        message: "bus actor unavailable".into(),
                    },
                }
            }
            Ok(ClientMsg::GetView { id, from, to }) => {
                view_reply(&app, conn_id, id, from, to).await
            }
            Err(e) => ServerMsg::Error {
                message: format!("malformed message: {e}"),
            },
        };
        if ack_tx.send(reply).await.is_err() {
            break; // writer gone → socket dead
        }
    }

    // Socket closed or writer died: drop our sender, unsubscribe, reap writer.
    drop(ack_tx);
    let _ = app.state.tell(Unsubscribe(conn_id)).send().await;
    writer.abort();
    eprintln!("[server] {peer}: closed");
}

/// The `get_view` reply: the flat plan projection over the requested window (board-zpe).
///
/// Registering the watch comes **before** the reply is sent. A plan published in the gap
/// between the two would otherwise never reach this connection, and the viewer would sit on
/// a chart it has no way to refresh until it reconnects.
///
/// The watch is only an "ask again" notice ([`ServerMsg::PlanChanged`]): the window lives in
/// the request, so it is always the client's own window that gets re-drawn.
async fn view_reply(
    app: &AppState,
    conn_id: u64,
    id: String,
    from: Option<i64>,
    to: Option<i64>,
) -> ServerMsg {
    if let (Some(from), Some(to)) = (from, to)
        && to <= from
    {
        return ServerMsg::Error {
            message: format!("get_view: empty window {from}..{to}"),
        };
    }
    let Ok(payload) = app.state.ask(GetState).await else {
        return ServerMsg::Error {
            message: "state actor unavailable".into(),
        };
    };
    let _ = app.state.tell(WatchPlan(conn_id)).send().await;
    let now = jiff::Timestamp::now().as_second();
    ServerMsg::View {
        id,
        view: View::of_plan(payload.plan.as_ref(), from, to, now),
    }
}

/// `/` is the API root for nothing; send a browser that lands here to the viewer.
async fn root_redirect() -> Redirect {
    Redirect::temporary("/app/")
}

async fn app_root() -> Redirect {
    Redirect::temporary("/app/")
}

async fn app_index() -> Response {
    serve_asset("")
}

async fn app_asset(UrlPath(path): UrlPath<String>) -> Response {
    serve_asset(&path)
}

/// Serve one embedded file.
///
/// The bundle is **not** behind the token, and that is deliberate: it carries no plant data, and
/// a multi-megabyte wasm fetched with `?token=` puts the secret in Referer headers and proxy logs.
/// Everything that does carry data (the socket) still requires it.
///
/// Hashed trunk filenames never change content, so they are cached hard; the html is not, because
/// that is what points at the new hashes.
fn serve_asset(path: &str) -> Response {
    match crate::assets::get(path) {
        Some((bytes, content_type)) => {
            let cache = if content_type.starts_with("text/html") {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache),
                ],
                bytes,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "no such asset\n").into_response(),
    }
}

async fn state_handler(State(app): State<AppState>) -> Response {
    match app.state.ask(GetState).await {
        Ok(payload) => Json(payload).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state actor unavailable").into_response(),
    }
}

async fn meta_handler(State(app): State<AppState>) -> Response {
    Json(app.meta.as_ref().clone()).into_response()
}

async fn directives_handler(State(app): State<AppState>) -> Response {
    match app.directives.ask(GetCurrent).await {
        Ok(d) => Json(d.wire_json()).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "directives actor unavailable",
        )
            .into_response(),
    }
}
