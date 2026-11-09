//! The wire types this viewer reads: the flat `view` payload and nothing else.
//!
//! The browser deliberately does not parse `Plan` or `StatePayload`. Everything it draws is
//! already projected server-side (`src/view.rs` in the bridge), so this module is a mirror of
//! the JSON pinned by `tests/fixtures/view.json` — including the fields that are `null` until
//! the ledger exists. If the bridge renames one, the fixture test here fails too.

use serde::Deserialize;

/// One answer to `get_view`. Never pushed: a plan change only says *ask again*.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct View {
    /// Unix seconds the plan was computed; `None` = the planner published nothing.
    pub computed_at: Option<i64>,
    /// The server's clock at projection time — where the "now" line goes.
    pub now: i64,
    pub from: i64,
    pub to: i64,
    pub steps: Vec<Step>,
    pub days: Vec<Day>,
    /// Empty until the ledger records actuals (board-aru).
    #[serde(default)]
    pub adherence: Vec<Adherence>,
}

impl View {
    /// Seconds between the server's clock and the plan, i.e. how stale the drawing is.
    pub fn age_secs(&self) -> Option<i64> {
        self.computed_at.map(|computed| self.now - computed)
    }
}

/// One MTU, already reduced to what a chart needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub start: i64,
    pub hours: f64,
    pub price_sell: f64,
    pub price_buy: f64,
    pub pv_kw: f64,
    /// Signed metered AC power: positive exported, negative imported. **Bars come from this**,
    /// never from `command_kw`.
    pub ac_kw: f64,
    /// What the DP commanded, signed the same way — a marker, not a bar length.
    pub command_kw: f64,
    /// `idle` | `export` | `import`.
    pub kind: String,
    pub soc_end_pct: f64,
    pub net_czk: f64,
    pub reason: String,
    /// The machine tag beside `reason`. Part of the pinned contract and read by the tests;
    /// the panels colour by the sign of `ac_kw` and print `reason`, so nothing draws it yet.
    #[allow(dead_code)]
    pub reason_kind: String,
    /// Metered AC power, once the ledger records it (board-aru).
    #[allow(dead_code)]
    pub ac_actual_kw: Option<f64>,
    pub soc_actual_pct: Option<f64>,
}

impl Step {
    /// The MTU's width in x-axis units (seconds).
    pub fn width_secs(&self) -> f64 {
        self.hours * 3600.0
    }

    /// The centre of the MTU — where a bar or a point for this step belongs.
    pub fn centre(&self) -> f64 {
        self.start as f64 + self.width_secs() / 2.0
    }
}

/// What one Prague day comes to.
#[derive(Debug, Clone, Deserialize)]
pub struct Day {
    pub day: String,
    pub sell_kwh: f64,
    pub buy_kwh: f64,
    pub pv_kwh: f64,
    pub net_czk: f64,
}

/// Planned vs actual power for one closed MTU.
#[derive(Debug, Clone, Deserialize)]
pub struct Adherence {
    pub start: i64,
    pub planned_kw: f64,
    pub actual_kw: f64,
    pub delta_kw: f64,
}

/// What one websocket text frame meant to this viewer.
#[derive(Debug, Clone)]
pub enum Frame {
    View(Box<View>),
    /// The plan changed; the payload is the new `computed_at` (or `None` for a withdrawal).
    /// The viewer re-requests with its own window rather than accepting a pushed view —
    /// a push cannot know which window this tab is showing.
    PlanChanged(Option<i64>),
    /// The bridge answered a request with an error (`get_view` with an empty window, an
    /// unavailable actor). Shown, not swallowed: a viewer that silently keeps the old chart
    /// is indistinguishable from one that is working.
    Error(String),
    /// Hello, state, plan, pong — frames this client does not consume.
    Ignored,
}

/// Decode one server frame. Unparseable text is an error frame, not a panic: a bridge
/// upgrade mid-session must not take the viewer down.
pub fn parse(text: &str) -> Frame {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => return Frame::Error(format!("malformed frame: {e}")),
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("view") => match value.get("view") {
            Some(payload) => match serde_json::from_value::<View>(payload.clone()) {
                Ok(view) => Frame::View(Box::new(view)),
                Err(e) => Frame::Error(format!("unreadable view payload: {e}")),
            },
            None => Frame::Error("view frame without a view".to_string()),
        },
        Some("plan_changed") => Frame::PlanChanged(
            value
                .get("computed_at")
                .and_then(serde_json::Value::as_i64),
        ),
        Some("error") => Frame::Error(
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the bridge reported an error")
                .to_string(),
        ),
        _ => Frame::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract between the bridge and this viewer is a file: the server projects a
    /// fixture plan into it, and this parser reads it. A renamed field fails here, before a
    /// blank chart ever reaches a browser.
    #[test]
    fn the_pinned_fixture_parses_into_these_types() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/view.json");
        let text = std::fs::read_to_string(path).expect("tests/fixtures/view.json is checked in");
        let view: View = serde_json::from_str(&text).expect("the fixture is a valid view");

        assert_eq!(view.steps.len(), 7);
        assert_eq!(view.days.len(), 2);
        assert!(view.adherence.is_empty());

        // The bar is metered power; the command sits beside it as a marker.
        assert_eq!(view.steps[0].ac_kw, 7.94);
        assert_eq!(view.steps[0].command_kw, 8.4);
        assert_eq!(view.steps[0].reason, "sell 7.90 > stored 5.32 CZK/kWh");
        assert_eq!(view.steps[0].reason_kind, "price_above_stored_value");
        assert_eq!(view.steps[1].ac_kw, -8.84);
        assert_eq!(view.days[0].day, "2026-11-04");
    }

    /// A hold that sells its surplus is a sale: the action says idle, the meter does not.
    #[test]
    fn an_idle_step_can_still_carry_a_positive_bar() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/view.json");
        let view: View = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let hold = &view.steps[6];
        assert_eq!(hold.kind, "idle");
        assert_eq!(hold.command_kw, 0.0);
        assert!(hold.ac_kw > 0.0);
    }

    /// A plan change is a notice, not a payload: it must never be mistaken for a view, and a
    /// withdrawal must be distinguishable from a plan that merely recomputed.
    #[test]
    fn a_plan_change_notice_carries_no_view() {
        assert!(matches!(
            parse(r#"{"type":"plan_changed","computed_at":1793746700}"#),
            Frame::PlanChanged(Some(1_793_746_700))
        ));
        assert!(matches!(
            parse(r#"{"type":"plan_changed","computed_at":null}"#),
            Frame::PlanChanged(None)
        ));
        assert!(matches!(
            parse(r#"{"type":"plan_changed"}"#),
            Frame::PlanChanged(None)
        ));
    }

    #[test]
    fn only_view_frames_are_consumed() {
        let frame = parse(r#"{"type":"state","ts":1,"link_ok":true,"values":{}}"#);
        assert!(matches!(frame, Frame::Ignored));
        let frame = parse(r#"{"type":"pong"}"#);
        assert!(matches!(frame, Frame::Ignored));
        let frame = parse(r#"{"type":"plan","plan":null}"#);
        assert!(matches!(frame, Frame::Ignored));
    }

    #[test]
    fn a_view_frame_is_unwrapped() {
        let frame = parse(
            r#"{"type":"view","id":"v1","view":{"computed_at":10,"now":12,"from":0,"to":900,
                "steps":[],"days":[],"adherence":[]}}"#,
        );
        match frame {
            Frame::View(view) => {
                assert_eq!(view.computed_at, Some(10));
                assert_eq!(view.now, 12);
                assert_eq!(view.age_secs(), Some(2));
            }
            other => panic!("expected a view, got {other:?}"),
        }
    }

    #[test]
    fn an_error_frame_is_surfaced() {
        let frame = parse(r#"{"type":"error","message":"get_view: empty window 1..1"}"#);
        match frame {
            Frame::Error(message) => assert!(message.contains("empty window")),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_text_is_an_error_not_a_panic() {
        assert!(matches!(parse("not json"), Frame::Error(_)));
        assert!(matches!(parse(r#"{"type":"view"}"#), Frame::Error(_)));
        assert!(matches!(
            parse(r#"{"type":"view","view":{"steps":[]}}"#),
            Frame::Error(_)
        ));
    }
}
