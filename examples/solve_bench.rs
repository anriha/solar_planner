//! Measurement harness, not production code: how long `solve` takes on a live-shaped
//! horizon as a function of the DP grid (action steps, energy steps, horizon length).
//!
//! Run with `cargo run --release --example solve_bench`.

use std::time::Instant;

use solar_controller::planner::{
    EfficiencyCurve, Mtu, Params, Plan, Problem, Slot, TerminalValue, solve,
};
use solar_controller::units::{AcKw, CzkPerKwh, DcKw, DcKwh, Hours, Quantity};

const MTU_SECS: i64 = 900;
/// 2026-11-06T00:00:00Z, an ordinary 24 h day.
const T0: i64 = 1_762_387_200;

fn curve<I: Quantity, O: Quantity>(points: &[(f64, f64)]) -> EfficiencyCurve<I, O> {
    EfficiencyCurve::new(
        points
            .iter()
            .map(|(kw, pct)| (I::from_f64(*kw), *pct))
            .collect(),
    )
    .expect("live curve points are valid")
}

/// The live machinery: device facts are boot constants, curves and policy are the
/// directives currently on the Pi (directives.json, version 34).
fn live_params(action_step_kw: f64, energy_step_kwh: f64) -> Params {
    Params {
        capacity_kwh: DcKwh::new(32.0),
        max_power_kw: AcKw::new(12.0),
        battery_discharge_max_kw: DcKw::new(10.24),
        battery_charge_max_kw: DcKw::new(10.24),
        grid_charge_max_kw: DcKw::new(9.472),
        pv_to_battery: curve(&[
            (0.25, 0.680),
            (0.8, 0.865),
            (1.6, 0.917),
            (4.0, 0.946),
            (10.0, 0.947),
        ]),
        pv_to_grid: curve(&[
            (0.25, 0.770),
            (0.6, 0.870),
            (1.1, 0.936),
            (2.0, 0.967),
            (10.0, 0.979),
        ]),
        battery_to_grid: curve(&[
            (0.5, 0.939),
            (4.5, 0.975),
            (6.0, 0.976),
            (8.0, 0.973),
            (10.24, 0.971),
        ]),
        grid_to_battery: curve(&[(1.0, 0.950), (9.472, 0.942)]),
        soc_floor: 0.20,
        soc_ceiling: 0.80,
        energy_step_kwh: DcKwh::new(energy_step_kwh),
        action_step_kw: AcKw::new(action_step_kw),
        curtailment_price: CzkPerKwh::new(7.974),
        terminal: TerminalValue {
            czk_per_kwh: CzkPerKwh::new(7.974),
        },
    }
}

fn slot_spot(index: usize) -> f64 {
    // Hours since midnight of the first slot, wrapped per day.
    let hour = (index % 96) as f64 * 0.25;
    let midday = (-((hour - 12.5) / 2.6).powi(2)).exp();
    let evening = (-((hour - 19.0) / 1.8).powi(2)).exp();
    let night = (-((hour - 3.0) / 3.0).powi(2)).exp();
    4.2 - 3.0 * midday + 6.3 * evening - 1.6 * night
}

fn slot_pv(index: usize) -> f64 {
    let hour = (index % 96) as f64 * 0.25;
    if !(6.0..19.0).contains(&hour) {
        return 0.0;
    }
    let phase = (hour - 6.0) / 13.0 * std::f64::consts::PI;
    9.0 * phase.sin().powi(2)
}

fn problem(slots: usize, action_step_kw: f64, energy_step_kwh: f64, soc: f64) -> Problem {
    let params = live_params(action_step_kw, energy_step_kwh);
    let horizon: Vec<Slot> = (0..slots)
        .map(|i| {
            let spot = slot_spot(i);
            Slot {
                mtu: Mtu {
                    start: T0 + i as i64 * MTU_SECS,
                    hours: Hours::new(0.25),
                },
                price_sell: CzkPerKwh::new(spot - 0.3),
                // Net spot × 21 % VAT, then the ceník's VAT-inclusive charges.
                price_buy: CzkPerKwh::new(spot * 1.21 + 2.515 + 0.3),
                pv_kw: DcKw::new(slot_pv(i)),
                load_kw: AcKw::new(0.09),
            }
        })
        .collect();
    Problem {
        initial_energy_kwh: DcKwh::new(params.capacity_kwh.get() * soc),
        now: T0 + 37 * 60,
        slots: horizon,
        params,
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        0.5 * (values[n / 2 - 1] + values[n / 2])
    }
}

fn count_non_idle(plan: &Plan) -> usize {
    plan.steps
        .iter()
        .filter(|step| step.ac_kwh.get().abs() > 1e-9)
        .count()
}

fn main() {
    let reps: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(20);

    println!(
        "{:>7} {:>7} {:>6} {:>7} {:>6} {:>10} {:>10} {:>10} {:>9} {:>6}",
        "slots",
        "e_step",
        "levels",
        "a_step",
        "actions",
        "min_ms",
        "median_ms",
        "mean_ms",
        "plan_czk",
        "traded"
    );

    for slots in [96usize, 140] {
        for (energy_step, action_step) in [
            (0.32, 1.2), // live today
            (0.32, 0.6),
            (0.32, 0.4),
            (0.32, 0.3),
            (0.32, 0.2),
            (0.32, 0.15),
            (0.32, 0.1),
            (0.16, 0.3),
            (0.16, 0.2),
            (0.08, 0.2),
        ] {
            let problem = problem(slots, action_step, energy_step, 0.55);
            let levels = ((problem.params.capacity_kwh.get()
                * (problem.params.soc_ceiling - problem.params.soc_floor))
                / energy_step)
                .floor() as usize
                + 1;
            let actions = ((12.0 / action_step).floor() as usize) * 2 + 1;

            // Warm up: first touch, cache, and the terminal row.
            let first = solve(&problem).expect("live problem is feasible");

            let mut times = Vec::with_capacity(reps);
            for _ in 0..reps {
                let start = Instant::now();
                let plan = solve(&problem).expect("live problem is feasible");
                times.push(start.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(&plan);
            }

            let min = times.iter().cloned().fold(f64::INFINITY, f64::min);
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            println!(
                "{slots:>7} {energy_step:>7.2} {levels:>6} {action_step:>7.2} {actions:>6} {:>10.3} {:>10.3} {:>10.3} {:>9.2} {:>6}",
                min,
                median(times),
                mean,
                first.expected_net_czk.get(),
                count_non_idle(&first)
            );
        }
    }
}
