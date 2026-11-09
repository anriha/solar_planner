//! The plan projection contract: the flat payload served at `/plan.json`, pinned by
//! `tests/fixtures/view.json`.
//!
//! The fixture is the contract between the bridge and everything that reads the horizon —
//! Grafana's Infinity datasource and the frozen wasm viewer, neither of which shares a crate
//! with it. The server projects a fixture plan into the file. Renaming a field on either side
//! therefore fails a test instead of blanking a chart.
//!
//! Regenerate after an intentional change:
//!
//! ```text
//! UPDATE_VIEW_FIXTURE=1 cargo test --test view
//! ```

use std::path::PathBuf;

use solar_controller::planner::{Action, Mtu, Plan, PlanStep, Reason};
use solar_controller::units::{AcKw, AcKwh, Czk, CzkPerKwh, DcKw, DcKwh, Hours};
use solar_controller::view::{MAX_WINDOW_SECS, View};

/// 2026-11-04T00:00 Prague (CET, UTC+1).
const T0: i64 = 1_793_746_800;
/// 2026-11-04T23:30 Prague — the first fixture MTU, just before the civil day flips.
const LATE: i64 = T0 + 23 * 3600 + 30 * 60;
/// 2026-11-05T11:00 Prague — a midday block, so the fixture has PV to total.
const NOON: i64 = T0 + 86_400 + 11 * 3600;
/// A fixed instant for the "now" marker, so the fixture does not drift with the clock.
const NOW: i64 = NOON + 42 * 60;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/view.json")
}

/// A plan with something of everything: an export and an import either side of midnight, a
/// midday block with PV, all five reason kinds, and a hold that sells its surplus.
fn fixture_plan() -> Plan {
    let hour = Hours::new(0.25);
    let step = |start: i64,
                price_sell: f64,
                price_buy: f64,
                pv_kw: f64,
                action: Action,
                ac_kwh: f64,
                energy_after_kwh: f64,
                net_czk: f64,
                reason: Reason| PlanStep {
        mtu: Mtu { start, hours: hour },
        price_sell: CzkPerKwh::new(price_sell),
        price_buy: CzkPerKwh::new(price_buy),
        pv_kw: DcKw::new(pv_kw),
        action,
        battery_out_kwh: DcKwh::new(1.5),
        ac_kwh: AcKwh::new(ac_kwh),
        energy_after_kwh: DcKwh::new(energy_after_kwh),
        net_czk: Czk::new(net_czk),
        marginal_czk_per_kwh: CzkPerKwh::new(5.32),
        reason,
    };

    let steps = vec![
        // 2026-11-04, late evening: sell the pack down.
        step(
            LATE,
            7.9,
            3.24,
            0.0,
            Action::Export { kw: AcKw::new(8.4) },
            1.985,
            20.0,
            15.68,
            Reason::PriceAboveStoredValue {
                price_sell: CzkPerKwh::new(7.9),
                marginal: CzkPerKwh::new(5.32),
            },
        ),
        // 2026-11-04, last MTU: buy while it is cheap.
        step(
            LATE + 900,
            7.9,
            3.24,
            0.0,
            Action::Import { kw: AcKw::new(9.6) },
            -2.21,
            24.0,
            -7.16,
            Reason::BuyBelowFutureValue {
                price_buy: CzkPerKwh::new(3.24),
                marginal: CzkPerKwh::new(5.32),
            },
        ),
        // 2026-11-05, midday: hold, then sell into the peak, then hold with a spilling surplus.
        step(
            NOON,
            6.2,
            3.1,
            2.5,
            Action::Idle,
            0.0,
            22.0,
            0.0,
            Reason::HoldForLater {
                marginal: CzkPerKwh::new(5.32),
                best_later_price: CzkPerKwh::new(8.4),
            },
        ),
        step(
            NOON + 900,
            7.2,
            3.2,
            4.0,
            Action::Export { kw: AcKw::new(3.6) },
            0.9,
            21.0,
            6.48,
            Reason::PriceAboveStoredValue {
                price_sell: CzkPerKwh::new(7.2),
                marginal: CzkPerKwh::new(5.0),
            },
        ),
        step(
            NOON + 1800,
            5.1,
            2.9,
            6.5,
            Action::Idle,
            0.0,
            20.0,
            0.0,
            Reason::Idle,
        ),
        step(
            NOON + 2700,
            9.0,
            4.2,
            7.2,
            Action::Export { kw: AcKw::new(8.4) },
            1.985,
            23.0,
            17.87,
            Reason::PvCannotBeStored {
                pv_kw: DcKw::new(9.5),
                headroom_kw: DcKw::new(1.2),
            },
        ),
        // A hold whose surplus leaves anyway: the bar is positive, the command is idle.
        step(
            NOON + 3600,
            6.5,
            3.0,
            5.0,
            Action::Idle,
            0.6,
            21.0,
            3.9,
            Reason::PvCannotBeStored {
                pv_kw: DcKw::new(5.0),
                headroom_kw: DcKw::ZERO,
            },
        ),
    ];

    Plan {
        computed_at: NOON - 120,
        steps,
        expected_net_czk: Czk::new(36.77),
        horizon_end: NOON + 3600 + 900,
    }
}

fn fixture_view() -> View {
    View::of_plan(Some(&fixture_plan()), None, None, NOW)
}

#[test]
fn the_view_fixture_is_pinned() {
    let view = serde_json::to_value(fixture_view()).expect("the view always serialises");

    if std::env::var_os("UPDATE_VIEW_FIXTURE").is_some() {
        let pretty = format!(
            "{}\n",
            serde_json::to_string_pretty(&view).expect("the view always serialises")
        );
        std::fs::write(fixture_path(), pretty).expect("the fixture is writable");
        return;
    }

    let fixture = std::fs::read_to_string(fixture_path()).expect(
        "tests/fixtures/view.json is missing; regenerate with UPDATE_VIEW_FIXTURE=1 cargo test --test view",
    );
    let expected: serde_json::Value = serde_json::from_str(&fixture).expect("the fixture is JSON");
    assert_eq!(
        view, expected,
        "the view payload changed; if that is intended, regenerate the fixture with \
         UPDATE_VIEW_FIXTURE=1 cargo test --test view and update the viewer"
    );
}

/// The 2026-11-08 measurement that board-vht, board-zpe and board-b0o all turn on: an export
/// commanded at 8.4 kW metered 7.94 kW, an import commanded at 9.6 kW metered 8.84 kW. A bar
/// drawn from `command_kw` would invent a surplus on one side and a shortfall on the other.
#[test]
fn bars_are_the_metered_power_and_the_command_is_a_marker() {
    let view = fixture_view();
    let export = &view.steps[0];
    assert_eq!(export.kind, "export");
    assert_eq!(export.ac_kw, 7.94);
    assert_eq!(export.command_kw, 8.4);

    let import = &view.steps[1];
    assert_eq!(import.kind, "import");
    assert_eq!(import.ac_kw, -8.84);
    assert_eq!(import.command_kw, -9.6);
}

/// A hold that sells its surplus is a sale: `action` says idle, the meter says otherwise.
#[test]
fn an_idle_that_sells_has_a_positive_bar() {
    let view = fixture_view();
    let hold = &view.steps[6];
    assert_eq!(hold.kind, "idle");
    assert_eq!(hold.command_kw, 0.0);
    assert_eq!(hold.ac_kw, 2.4);
}

#[test]
fn soc_end_is_a_percentage_of_the_nameplate() {
    let view = fixture_view();
    // 20.0 kWh of the 32 kWh pack.
    assert_eq!(view.steps[0].soc_end_pct, 62.5);
}

#[test]
fn day_totals_split_at_prague_midnight() {
    let view = fixture_view();
    assert_eq!(view.days.len(), 2);
    let first = &view.days[0];
    assert_eq!(first.day, "2026-11-04");
    assert_eq!(first.sell_kwh, 1.99);
    assert_eq!(first.buy_kwh, 2.21);
    assert_eq!(first.pv_kwh, 0.0);
    assert_eq!(first.net_czk, 8.52);
    assert_eq!(first.mtus, 2);

    let second = &view.days[1];
    assert_eq!(second.day, "2026-11-05");
    // 0.9 kWh spot export + 1.985 kWh sell block + 0.6 kWh hold surplus
    assert_eq!(second.sell_kwh, 3.49);
    assert_eq!(second.buy_kwh, 0.0);
    // (2.5 + 4.0 + 6.5 + 7.2 + 5.0) kW × 0.25 h
    assert_eq!(second.pv_kwh, 6.3);
    assert_eq!(second.mtus, 5);
}

#[test]
fn a_window_clips_the_steps_it_serves() {
    let view = View::of_plan(Some(&fixture_plan()), Some(NOON), Some(NOON + 1800), NOW);
    assert_eq!(view.from, NOON);
    assert_eq!(view.to, NOON + 1800);
    let starts: Vec<i64> = view.steps.iter().map(|step| step.start).collect();
    assert_eq!(starts, vec![NOON, NOON + 900]);
}

/// The request is clamped, never rejected: a viewer asking for a year gets the last 60 days
/// and the window it actually received, so it can say so instead of hanging.
#[test]
fn a_window_longer_than_sixty_days_is_clamped() {
    let to = 2_000_000_000;
    let view = View::of_plan(Some(&fixture_plan()), Some(0), Some(to), NOW);
    assert_eq!(view.to, to);
    assert_eq!(view.from, to - MAX_WINDOW_SECS);
    assert!(
        view.steps.is_empty(),
        "the fixture plan is far outside the window"
    );
}

#[test]
fn a_withdrawn_plan_projects_to_an_empty_view() {
    let view = View::of_plan(None, None, None, NOW);
    assert_eq!(view.computed_at, None);
    assert!(view.steps.is_empty());
    assert!(view.days.is_empty());
}

#[test]
fn every_reason_kind_matches_its_wire_tag_and_describes_itself() {
    let reasons = [
        Reason::PriceAboveStoredValue {
            price_sell: CzkPerKwh::new(7.9),
            marginal: CzkPerKwh::new(5.32),
        },
        Reason::PvCannotBeStored {
            pv_kw: DcKw::new(9.5),
            headroom_kw: DcKw::new(1.2),
        },
        Reason::BuyBelowFutureValue {
            price_buy: CzkPerKwh::new(3.24),
            marginal: CzkPerKwh::new(5.32),
        },
        Reason::HoldForLater {
            marginal: CzkPerKwh::new(5.32),
            best_later_price: CzkPerKwh::new(8.4),
        },
        Reason::Idle,
    ];
    for reason in reasons {
        let wire = serde_json::to_value(reason).expect("reasons always serialise");
        assert_eq!(reason.kind(), wire["kind"].as_str().unwrap());
        assert!(!reason.describe().is_empty());
    }
}
