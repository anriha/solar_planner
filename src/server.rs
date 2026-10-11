use std::{collections::HashMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::ActorRef;

use crate::{
    bus::{BusActor, WriteRegister},
    directives::{ApplyDirective, DirectivesActor, GetCurrent},
    protocol::{ClientMsg, ServerMsg},
    state::{GetState, StateActor, Subscribe, Unsubscribe},
};

const CHANNEL_CAP: usize = 32;

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
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

fn authorized(token: &str, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    let header_ok = headers
        .get(AUTHORIZATION)
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
) -> Response {
    if !authorized(&app.token, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing token\n").into_response();
    }
    ws.on_upgrade(move |socket| connection(app, socket))
}

async fn connection(app: AppState, mut socket: WebSocket) {
    let hello = match (
        app.state.ask(GetState).await,
        app.directives.ask(GetCurrent).await,
    ) {
        (Ok(state), Ok(directives)) => ServerMsg::Hello {
            meta: (*app.meta).clone(),
            state,
            directives,
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
    let Ok(id) = app.state.ask(Subscribe { tx }).await else {
        return;
    };

    // Single writer owns the sink; on exit it unsubscribes so a dead
    // connection can't leak a channel sender in StateActor.
    let writer_state = app.state.clone();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink
                .send(Message::Text(msg.to_text().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = writer_state.tell(Unsubscribe(id)).send().await;
    });

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            // axum answers protocol-level pings itself; the rest is ignored.
            _ => continue,
        };
        let reply = match serde_json::from_str::<ClientMsg>(text.as_str()) {
            Ok(ClientMsg::Ping) => ServerMsg::Pong,
            Ok(ClientMsg::Cmd { id, name, value }) => {
                match app.directives.ask(ApplyDirective { name, value }).await {
                    Ok(directives) => ServerMsg::CmdAck {
                        id,
                        ok: true,
                        error: None,
                        directives,
                    },
                    Err(kameo::error::SendError::HandlerError(e)) => ServerMsg::CmdAck {
                        id,
                        ok: false,
                        error: Some(e.to_string()),
                        directives: app.directives.ask(GetCurrent).await.unwrap_or_default(),
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
    let _ = app.state.tell(Unsubscribe(id)).send().await;
    writer.abort();
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
        Ok(d) => Json(d).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "directives actor unavailable",
        )
            .into_response(),
    }
}
