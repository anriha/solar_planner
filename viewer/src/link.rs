//! The one websocket connection, and the discipline that keeps it honest.
//!
//! Ported from `hub.py`, which learned both lessons the hard way: the socket alone proves
//! nothing (a bridge that stopped pushing state still answered protocol pings for 19 h 30 m
//! on 2026-11-03), so liveness is proven on the message channel — a ping every 30 s and a
//! reconnect after 90 s of utter silence. Reconnect backs off 5 s → 60 s and resets on open.
//!
//! One connection carries everything: the auth token, the `get_view` request, the push, and
//! the liveness pings. There is no second channel to keep aligned.

use std::time::Duration;

// `std::time::Instant` is *unsupported* on wasm32-unknown-unknown — it panics at runtime, which
// is a blank page rather than an error. `web-time` is the same API over `performance.now()` in a
// browser and over std natively.
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use eframe::egui;

use crate::wire::{self, View};

const PING_EVERY: Duration = Duration::from_secs(30);
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Where the viewer connects and which token it presents.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Base websocket URL, e.g. `ws://127.0.0.1:8300/ws`.
    pub url: String,
    pub token: String,
}

impl Endpoint {
    /// `ws://host/ws?token=…` — the query is what the bridge's auth accepts.
    pub fn ws_url(&self) -> String {
        let separator = if self.url.contains('?') { '&' } else { '?' };
        format!("{}{separator}token={}", self.url, self.token)
    }

    /// Native: `--url URL --token TOKEN`, token falling back to `SOLAR_TOKEN`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_args(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut endpoint = Endpoint {
            url: "ws://127.0.0.1:8300/ws".to_string(),
            token: std::env::var("SOLAR_TOKEN").unwrap_or_default(),
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--url" => endpoint.url = args.next().ok_or("--url needs a value")?,
                "--token" => endpoint.token = args.next().ok_or("--token needs a value")?,
                "-h" | "--help" => {
                    return Err("usage: viewer [--url ws://host:8300/ws] [--token TOKEN]\n\
                         (token can also come from SOLAR_TOKEN)"
                        .to_string());
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        if endpoint.token.is_empty() {
            return Err("no token: pass --token or set SOLAR_TOKEN".to_string());
        }
        Ok(endpoint)
    }

    /// Wasm: same origin as the page, token from `?token=` in the address bar.
    #[cfg(target_arch = "wasm32")]
    pub fn from_location() -> Self {
        use web_sys::window;

        let location = window().expect("a browser has a window").location();
        let secure = location.protocol().map(|p| p == "https:").unwrap_or(false);
        let scheme = if secure { "wss" } else { "ws" };
        let host = location
            .host()
            .unwrap_or_else(|_| "127.0.0.1:8300".to_string());
        let search = location.search().unwrap_or_default();
        let token = search
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .unwrap_or_default()
            .to_string();
        Endpoint {
            url: format!("{scheme}://{host}/ws"),
            token,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Open,
    Backoff,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Connecting => "connecting",
            Phase::Open => "live",
            Phase::Backoff => "reconnecting",
        }
    }
}

/// The connection plus the last view it delivered.
pub struct Link {
    endpoint: Endpoint,
    socket: Option<ewebsock::WsSender>,
    receiver: Option<ewebsock::WsReceiver>,
    phase: Phase,
    /// Last frame of any kind — the silence timer.
    last_rx: Instant,
    last_ping: Instant,
    retry_at: Option<Instant>,
    backoff: Duration,
    /// The window this viewer shows. `None` = the plan's own horizon. It lives here because
    /// a re-request after a plan change must ask for the window it is actually drawing.
    window: Option<(i64, i64)>,
    /// A `get_view` is in flight. A notice that arrives meanwhile re-requests on the reply
    /// instead of queueing a second request the bridge would only answer in order anyway.
    awaiting: bool,
    /// A plan change arrived while a request was in flight.
    dirty: bool,
    next_id: u64,
    view: Option<View>,
    error: Option<String>,
}

impl Link {
    pub fn new(endpoint: Endpoint, window: Option<(i64, i64)>) -> Self {
        let now = Instant::now();
        Self {
            endpoint,
            socket: None,
            receiver: None,
            phase: Phase::Backoff,
            last_rx: now,
            last_ping: now,
            retry_at: Some(now),
            backoff: BACKOFF_MIN,
            window,
            awaiting: false,
            dirty: false,
            next_id: 0,
            view: None,
            error: None,
        }
    }

    pub fn view(&self) -> Option<&View> {
        self.view.as_ref()
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Seconds since the last frame of any kind.
    pub fn quiet_secs(&self) -> u64 {
        self.last_rx.elapsed().as_secs()
    }

    pub fn backoff_secs(&self) -> u64 {
        self.backoff.as_secs()
    }

    /// Ask again — the "reload" button, a fresh connection, and every plan change.
    ///
    /// Always with **this** viewer's window: a pushed view would have to guess it, and a
    /// viewer showing a week would be reset to the horizon on every replan.
    pub fn request_view(&mut self) {
        if self.awaiting {
            self.dirty = true;
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let text = match self.window {
            Some((from, to)) => {
                format!(r#"{{"type":"get_view","id":"v{id}","from":{from},"to":{to}}}"#)
            }
            None => format!(r#"{{"type":"get_view","id":"v{id}"}}"#),
        };
        if self.send(&text) {
            self.awaiting = true;
            self.dirty = false;
        }
    }

    /// Send one frame; `false` when there is no socket to send it on.
    fn send(&mut self, text: &str) -> bool {
        match &mut self.socket {
            Some(socket) => {
                socket.send(ewebsock::WsMessage::Text(text.to_string()));
                true
            }
            None => false,
        }
    }

    /// Drive the connection: connect when due, drain frames, ping, and reconnect on silence.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let now = Instant::now();

        if self.phase == Phase::Backoff && self.retry_at.is_some_and(|at| now >= at) {
            self.connect(ctx);
        }

        let mut events = Vec::new();
        if let Some(receiver) = &self.receiver {
            while let Some(event) = receiver.try_recv() {
                events.push(event);
            }
        }

        let mut closed = false;
        let mut opened = false;
        for event in events {
            self.last_rx = now;
            match event {
                ewebsock::WsEvent::Opened => opened = true,
                ewebsock::WsEvent::Message(ewebsock::WsMessage::Text(text)) => {
                    self.consume(&text);
                }
                // Pongs and binary frames still prove the channel is alive.
                ewebsock::WsEvent::Message(_) => {}
                ewebsock::WsEvent::Error(message) => {
                    self.error = Some(message);
                    closed = true;
                }
                ewebsock::WsEvent::Closed => closed = true,
            }
        }

        if opened {
            self.phase = Phase::Open;
            self.backoff = BACKOFF_MIN;
            self.retry_at = None;
            self.last_ping = now;
            self.awaiting = false;
            self.dirty = false;
            self.request_view();
        }

        if closed && self.phase != Phase::Backoff {
            self.disconnect(now, "the bridge closed the connection");
            return;
        }

        if self.phase == Phase::Open {
            if now.duration_since(self.last_rx) > SILENCE_LIMIT {
                self.disconnect(now, &format!("no frame for {} s", SILENCE_LIMIT.as_secs()));
            } else if now.duration_since(self.last_ping) >= PING_EVERY {
                self.last_ping = now;
                self.send(r#"{"type":"ping"}"#);
            }
        }
    }

    fn consume(&mut self, text: &str) {
        match wire::parse(text) {
            wire::Frame::View(view) => {
                self.view = Some(*view);
                self.error = None;
                self.awaiting = false;
                if std::mem::take(&mut self.dirty) {
                    self.request_view();
                }
            }
            // Re-request, never accept a pushed window. An unchanged `computed_at` is a
            // duplicate notice (a replan that changed nothing visible) and needs no trip.
            wire::Frame::PlanChanged(computed_at) => {
                let showing = self.view.as_ref().map(|view| view.computed_at);
                if showing != Some(computed_at) {
                    self.request_view();
                }
            }
            wire::Frame::Error(message) => {
                self.error = Some(message);
                self.awaiting = false;
            }
            wire::Frame::Ignored => {}
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let url = self.endpoint.ws_url();
        let repaint = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        match ewebsock::connect_with_wakeup(url.clone(), ewebsock::Options::default(), repaint) {
            Ok((socket, receiver)) => {
                self.socket = Some(socket);
                self.receiver = Some(receiver);
                self.phase = Phase::Connecting;
                self.retry_at = None;
                self.last_rx = Instant::now();
                self.error = None;
            }
            Err(message) => {
                self.error = Some(format!("{url}: {message}"));
                self.schedule_retry(Instant::now());
            }
        }
    }

    fn disconnect(&mut self, now: Instant, reason: &str) {
        self.socket = None;
        self.receiver = None;
        self.awaiting = false;
        self.dirty = false;
        if self.error.is_none() {
            self.error = Some(reason.to_string());
        }
        self.schedule_retry(now);
    }

    fn schedule_retry(&mut self, now: Instant) {
        self.phase = Phase::Backoff;
        self.retry_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> Link {
        Link::new(
            Endpoint {
                url: "ws://example.invalid/ws".to_string(),
                token: "t".to_string(),
            },
            None,
        )
    }

    fn view_frame(computed_at: i64) -> String {
        format!(
            r#"{{"type":"view","id":"v0","view":{{"computed_at":{computed_at},"now":1,
                "from":0,"to":900,"steps":[],"days":[],"adherence":[]}}}}"#
        )
    }

    /// The client-side half of board-b0o: a notice only means "ask again" when the plan it
    /// names is not the one already on screen. Without this, every duplicate notice would
    /// cost a round trip — and a withdrawn plan would never be asked for.
    #[test]
    fn a_notice_asks_again_only_when_the_plan_actually_changed() {
        let mut link = link();
        link.consume(&view_frame(10));
        let after_view = link.next_id;

        link.consume(r#"{"type":"plan_changed","computed_at":10}"#);
        assert_eq!(link.next_id, after_view, "the plan on screen needs no request");

        link.consume(r#"{"type":"plan_changed","computed_at":11}"#);
        assert_eq!(link.next_id, after_view + 1, "a new plan is re-requested");

        link.consume(r#"{"type":"plan_changed","computed_at":null}"#);
        assert_eq!(
            link.next_id,
            after_view + 2,
            "a withdrawal is re-requested, so the chart can clear"
        );
    }
}
