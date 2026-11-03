//! Regression for the 2026-11-03 silent outage: a connection whose socket
//! fell behind for longer than the outbound buffer was *unsubscribed* for
//! good, leaving an open socket that never carried state again. HA sat on
//! stale values for 19 h 30 m with no error, no `unavailable`, no reconnect.

use std::{collections::BTreeMap, time::Duration};

use kameo::actor::Spawn;
use solar_controller::{
    planner::{Action, Mtu, Plan, PlanStep, Reason},
    protocol::ServerMsg,
    state::{StateActor, Subscribe, Update},
    telemetry::Snapshot,
    units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKwh, Hours},
};
use tokio::{sync::mpsc, time::timeout};

fn snap(ts: u64) -> Snapshot {
    Snapshot {
        link_ok: true,
        ts,
        values: BTreeMap::new(),
    }
}

/// One step is enough: these tests are about *when* the plan is sent, not its contents.
fn plan() -> Plan {
    Plan {
        computed_at: 1,
        steps: vec![PlanStep {
            mtu: Mtu {
                start: 1_793_746_800,
                hours: Hours::new(0.25),
            },
            action: Action::Export {
                kw: AcKw::new(6.0),
            },
            battery_out_kwh: DcKwh::new(1.5),
            ac_kwh: AcKwh::new(1.5),
            energy_after_kwh: DcKwh::new(21.4),
            net_czk: Czk::new(9.7),
            marginal_czk_per_kwh: CzkPerKwh::new(5.3),
            reason: Reason::Idle,
        }],
        expected_net_czk: Czk::new(9.7),
        horizon_end: 1_793_747_700,
    }
}

async fn recv(rx: &mut mpsc::Receiver<ServerMsg>) -> Option<ServerMsg> {
    timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("timed out waiting for a frame")
}

#[tokio::test]
async fn full_channel_does_not_unsubscribe() {
    let state = StateActor::spawn(StateActor::default());
    let (tx, mut rx) = mpsc::channel(1);
    state.ask(Subscribe { tx }).await.unwrap();

    // Five ticks into a one-slot queue: ticks 2..5 cannot be buffered.
    for ts in 1..=5 {
        state.ask(Update::Good(snap(ts))).await.unwrap();
    }

    // The buffered frame survives a reaped subscriber, so this passes either
    // way; the drain below is what tells the two behaviours apart.
    assert!(recv(&mut rx).await.is_some(), "buffered frame lost");

    state.ask(Update::Good(snap(9))).await.unwrap();
    let next = recv(&mut rx)
        .await
        .expect("queue closed: the subscriber was reaped while its socket was momentarily behind");
    assert!(
        matches!(next, ServerMsg::State { ts: 9, .. }),
        "expected the newest snapshot after catching up, got {next:?}"
    );
}

#[tokio::test]
async fn stalled_connection_does_not_starve_others() {
    let state = StateActor::spawn(StateActor::default());
    // Never drained: fills at tick 2 and stays full.
    let (slow_tx, _slow_rx) = mpsc::channel(1);
    let (fast_tx, mut fast_rx) = mpsc::channel(8);
    state.ask(Subscribe { tx: slow_tx }).await.unwrap();
    state.ask(Subscribe { tx: fast_tx }).await.unwrap();

    for ts in 1..=3 {
        state.ask(Update::Good(snap(ts))).await.unwrap();
    }

    for ts in 1..=3 {
        let frame = recv(&mut fast_rx).await.expect("healthy queue closed");
        assert!(
            matches!(frame, ServerMsg::State { ts: got, .. } if got == ts),
            "healthy connection missed tick {ts}"
        );
    }
}

#[tokio::test]
async fn closed_queue_is_reaped() {
    let state = StateActor::spawn(StateActor::default());
    let (dead_tx, dead_rx) = mpsc::channel(1);
    state.ask(Subscribe { tx: dead_tx }).await.unwrap();
    drop(dead_rx); // connection gone: writer dropped its receiver

    state.ask(Update::Good(snap(1))).await.unwrap();

    // A later subscriber still gets frames, and the dead one is not counted
    // as open any more (broadcast reaps it instead of retrying forever).
    let (tx, mut rx) = mpsc::channel(2);
    state.ask(Subscribe { tx }).await.unwrap();
    state.ask(Update::Good(snap(2))).await.unwrap();
    let frame = recv(&mut rx).await.expect("queue closed");
    assert!(matches!(frame, ServerMsg::State { ts: 2, .. }));
}

/// The plan is 60–80 KB and changes once a minute; the poll is 5 s. Riding every
/// telemetry frame would be ~1.2 GB/day per subscriber for a value that did not move.
#[tokio::test]
async fn the_plan_is_its_own_frame_not_part_of_the_poll() {
    let state = StateActor::spawn(StateActor::default());
    let (tx, mut rx) = mpsc::channel(8);
    state.ask(Subscribe { tx }).await.unwrap();

    state.ask(Update::Good(snap(1))).await.unwrap();
    let telemetry = recv(&mut rx).await.expect("telemetry frame");
    assert!(matches!(telemetry, ServerMsg::State { ts: 1, .. }));
    let json = serde_json::to_value(&telemetry).unwrap();
    assert!(
        json.get("plan").is_none(),
        "a poll must not carry the plan: {json}"
    );

    state.ask(Update::Plan(Some(plan()))).await.unwrap();
    let frame = recv(&mut rx).await.expect("plan frame");
    let json = serde_json::to_value(&frame).unwrap();
    assert_eq!(json["type"], "plan");
    assert_eq!(json["plan"]["steps"].as_array().unwrap().len(), 1);

    state.ask(Update::Good(snap(2))).await.unwrap();
    let telemetry = recv(&mut rx).await.expect("telemetry frame");
    assert!(
        serde_json::to_value(&telemetry).unwrap().get("plan").is_none(),
        "the plan goes out on change, not again with every poll"
    );
}

/// Withdrawing the plan is a change too, and must travel on the same frame.
#[tokio::test]
async fn withdrawing_the_plan_sends_an_explicit_null() {
    let state = StateActor::spawn(StateActor::default());
    let (tx, mut rx) = mpsc::channel(8);
    state.ask(Subscribe { tx }).await.unwrap();

    state.ask(Update::Plan(Some(plan()))).await.unwrap();
    assert!(matches!(recv(&mut rx).await, Some(ServerMsg::Plan { .. })));

    state.ask(Update::Plan(None)).await.unwrap();
    let frame = recv(&mut rx).await.expect("withdrawal frame");
    let json = serde_json::to_value(&frame).unwrap();
    assert_eq!(json["type"], "plan");
    assert!(json["plan"].is_null(), "an absent plan must be explicit");
}

/// Being the one message outside the telemetry stream, the plan is the one a skipped
/// frame could lose. Catching up must therefore hand it over again.
#[tokio::test]
async fn a_lagging_connection_is_handed_the_plan_again_when_it_catches_up() {
    let state = StateActor::spawn(StateActor::default());
    let (tx, mut rx) = mpsc::channel(2);
    state.ask(Subscribe { tx }).await.unwrap();

    // Both slots taken by the plan and one poll; the poll behind them is skipped.
    state.ask(Update::Plan(Some(plan()))).await.unwrap();
    state.ask(Update::Good(snap(1))).await.unwrap();
    state.ask(Update::Good(snap(2))).await.unwrap();
    assert!(matches!(recv(&mut rx).await, Some(ServerMsg::Plan { .. })));
    assert!(matches!(
        recv(&mut rx).await,
        Some(ServerMsg::State { ts: 1, .. })
    ));

    // Drained, so the next poll gets through — and the transition back to "caught up"
    // replays the plan once, so the skipped frame costs nothing.
    state.ask(Update::Good(snap(3))).await.unwrap();
    let caught_up = recv(&mut rx).await.expect("telemetry frame");
    assert!(matches!(caught_up, ServerMsg::State { ts: 3, .. }));
    let replay = recv(&mut rx).await.expect("plan replayed after catching up");
    assert!(matches!(replay, ServerMsg::Plan { .. }));
}
