//! The stacked panels. Each one is a plain `egui_plot::Plot`; the only shared state between
//! them is the x-axis link, which is what makes a zoom in the price panel zoom the SOC panel
//! with it.
//!
//! Nothing here computes a plan quantity. `ac_kw` is the bar height, `command_kw` is a
//! marker, and the reason text arrives pre-formatted — the bridge already decided all of it
//! (`src/view.rs`), and duplicating those rules in the browser is how the two drift apart.

use eframe::egui::{self, Color32, Id};
use egui_plot::{Bar, BarChart, GridInput, GridMark, Legend, Line, Plot, Points, VLine};
use jiff::tz::TimeZone;

use crate::timefmt;
use crate::wire::View;

/// Shared by every panel: one zoom/pan/cursor group for the whole stack.
const X_LINK: &str = "plan-x";

const SELL: Color32 = Color32::from_rgb(0x66, 0xc2, 0x5c);
const BUY: Color32 = Color32::from_rgb(0xf0, 0x9f, 0x3a);
const PV: Color32 = Color32::from_rgb(0x4c, 0xa3, 0xff);
const EXPORT: Color32 = Color32::from_rgb(0x5f, 0xc9, 0x6a);
const IMPORT: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x5f);
const IDLE: Color32 = Color32::from_rgb(0x63, 0x6a, 0x76);
const SOC: Color32 = Color32::from_rgb(0xa9, 0x8c, 0xf0);
const ACTUAL: Color32 = Color32::from_rgb(0xf5, 0xd0, 0x62);
const NOW: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);

/// Every panel is built from this, so they cannot disagree about linking or the clock.
fn panel(id: &'static str, zone: &TimeZone) -> Plot<'static> {
    let zone = zone.clone();
    Plot::new(id)
        .height(150.0)
        .allow_scroll(false)
        .link_axis(Id::unique(X_LINK), [true, false])
        .link_cursor(Id::unique(X_LINK), [true, false])
        .x_axis_formatter(move |mark, _range| timefmt::clock(mark.value as i64, &zone))
        .x_grid_spacer(clock_grid)
        .legend(Legend::default())
}

/// Clock-aligned grid: a time axis has to read 02:00, 02:15, 02:30 — the default spacer picks
/// round *numbers of seconds*, which lands on 01:26 and 01:43 and makes the axis unreadable.
///
/// The step is chosen so the visible span keeps a handful of labels, and hours carry their
/// quarter-hour subdivisions so a zoomed-in plan still shows the MTU boundaries.
fn clock_grid(input: GridInput) -> Vec<GridMark> {
    const STEPS: [i64; 7] = [
        900,
        1_800,
        3_600,
        3 * 3_600,
        6 * 3_600,
        12 * 3_600,
        24 * 3_600,
    ];
    let (min, max) = input.bounds;
    let span = (max - min).max(1.0);
    let step = STEPS
        .iter()
        .copied()
        .find(|step| span / (*step as f64) <= 9.0)
        .unwrap_or(24 * 3_600);

    let mut marks = Vec::new();
    let mut value = (min / step as f64).ceil() as i64 * step;
    while (value as f64) <= max {
        marks.push(GridMark {
            value: value as f64,
            step_size: step as f64,
        });
        value += step;
    }
    if step >= 3_600 {
        let minor = step / 4;
        let mut value = (min / minor as f64).ceil() as i64 * minor;
        while (value as f64) <= max {
            if value % step != 0 {
                marks.push(GridMark {
                    value: value as f64,
                    step_size: minor as f64,
                });
            }
            value += minor;
        }
    }
    marks
}

/// The "now" marker, but **only when the plan actually covers now**.
///
/// A stale plan (the bridge has been offline, the planner is off) has `now` far outside its own
/// horizon; drawing the line there stretches the x axis until the plan is a sliver at the left
/// edge. The header already says how old the plan is — the chart's job is to show the plan.
fn now_line(view: &View) -> Option<VLine> {
    if !(view.from..=view.to).contains(&view.now) {
        return None;
    }
    Some(
        VLine::new("now", view.now as f64)
            .color(NOW)
            .width(1.0)
            .style(egui_plot::LineStyle::Dashed { length: 6.0 }),
    )
}

/// What exporting and importing each pay, CZK/kWh. The two curves are the whole reason the
/// plan looks the way it does, so they are the top panel.
pub fn prices(ui: &mut egui::Ui, view: &View, zone: &TimeZone) {
    let sell: Vec<[f64; 2]> = view
        .steps
        .iter()
        .map(|step| [step.centre(), step.price_sell])
        .collect();
    let buy: Vec<[f64; 2]> = view
        .steps
        .iter()
        .map(|step| [step.centre(), step.price_buy])
        .collect();

    panel("prices", zone).show(ui, |plot| {
        plot.line(Line::new("sell", sell).color(SELL).width(1.6));
        plot.line(Line::new("buy", buy).color(BUY).width(1.6));
        if let Some(now) = now_line(view) {
            plot.vline(now);
        }
    });
}

/// Forecast PV, as an area: the energy the plan is built around.
pub fn pv(ui: &mut egui::Ui, view: &View, zone: &TimeZone) {
    let pv: Vec<[f64; 2]> = view
        .steps
        .iter()
        .map(|step| [step.centre(), step.pv_kw])
        .collect();

    panel("pv", zone).show(ui, |plot| {
        plot.line(Line::new("PV", pv).color(PV).fill(0.25).width(1.4));
        if let Some(now) = now_line(view) {
            plot.vline(now);
        }
    });
}

/// The decision: signed metered power, with the DP's command as a marker beside it.
///
/// The bar is `ac_kw`, never `command_kw` — an 8.4 kW export target metered 7.94 kW and a
/// 9.6 kW import target metered 8.84 kW on 2026-11-08, so sizing bars from the command would
/// invent a surplus in one direction and a shortfall in the other.
pub fn power(ui: &mut egui::Ui, view: &View, zone: &TimeZone) {
    let bars: Vec<Bar> = view
        .steps
        .iter()
        .map(|step| {
            let mut bar = Bar::new(step.centre(), step.ac_kw)
                .width(step.width_secs() * 0.78)
                .fill(bar_color(step.ac_kw));
            bar.name = format!(
                "{} · {:+.2} kW · {:.2} kWh · {:+.2} CZK — {}",
                step.kind,
                step.ac_kw,
                step.ac_kw * step.hours,
                step.net_czk,
                step.reason
            );
            bar
        })
        .collect();
    let command: Vec<[f64; 2]> = view
        .steps
        .iter()
        .map(|step| [step.centre(), step.command_kw])
        .collect();

    panel("power", zone).include_y(0.0).show(ui, |plot| {
        plot.bar_chart(
            BarChart::new("metered", bars)
                .element_formatter(Box::new(|bar, _chart| bar.name.clone())),
        );
        plot.points(
            Points::new("command", command)
                .radius(2.0)
                .color(Color32::from_rgb(0xcc, 0xcc, 0xcc)),
        );
        if let Some(now) = now_line(view) {
            plot.vline(now);
        }
    });
}

/// Planned state of charge, with the measured line once the ledger exists.
pub fn soc(ui: &mut egui::Ui, view: &View, zone: &TimeZone) {
    let planned: Vec<[f64; 2]> = view
        .steps
        .iter()
        .map(|step| [step.centre(), step.soc_end_pct])
        .collect();
    let actual: Vec<[f64; 2]> = view
        .steps
        .iter()
        .filter_map(|step| Some([step.centre(), step.soc_actual_pct?]))
        .collect();

    panel("soc", zone)
        .include_y(0.0)
        .include_y(100.0)
        .show(ui, |plot| {
            plot.line(Line::new("planned", planned).color(SOC).width(1.6));
            if actual.len() > 1 {
                plot.line(Line::new("actual", actual).color(ACTUAL).width(1.6));
            }
            if let Some(now) = now_line(view) {
                plot.vline(now);
            }
        });
}

/// `|actual − planned|` per closed MTU. Empty until the ledger records actuals, so the panel
/// reports that instead of drawing a flat zero line that would read as "perfect adherence".
pub fn adherence(ui: &mut egui::Ui, view: &View, zone: &TimeZone) {
    if view.adherence.is_empty() {
        ui.label(
            egui::RichText::new("no ledger yet — adherence lands with board-aru")
                .weak()
                .italics(),
        );
        return;
    }
    let bars: Vec<Bar> = view
        .adherence
        .iter()
        .map(|row| {
            let mut bar = Bar::new(row.start as f64, row.delta_kw.abs())
                .width(700.0)
                .fill(delta_color(row.delta_kw.abs()));
            bar.name = format!(
                "planned {:+.2} kW · actual {:+.2} kW · {:+.2} off",
                row.planned_kw, row.actual_kw, row.delta_kw
            );
            bar
        })
        .collect();

    panel("adherence", zone).show(ui, |plot| {
        plot.bar_chart(
            BarChart::new("|actual - planned|", bars)
                .element_formatter(Box::new(|bar, _chart| bar.name.clone())),
        );
        if let Some(now) = now_line(view) {
            plot.vline(now);
        }
    });
}

fn bar_color(kw: f64) -> Color32 {
    if kw > 0.0 {
        EXPORT
    } else if kw < 0.0 {
        IMPORT
    } else {
        IDLE
    }
}

fn delta_color(delta_kw: f64) -> Color32 {
    if delta_kw < 0.5 {
        EXPORT
    } else if delta_kw < 2.0 {
        BUY
    } else {
        IMPORT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Rect, pos2, vec2};

    fn fixture() -> View {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/view.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Every panel drawn once, headless, against the pinned fixture and against an empty
    /// view. The app compiles either way; this is the test that a plot item cannot panic on
    /// real (or absent) data — `BarChart` with a formatter and a filled `Line` only run when
    /// a frame is actually laid out.
    #[test]
    fn every_panel_survives_a_frame() {
        let zone = crate::timefmt::prague();
        let view = fixture();
        let empty = View::default();

        let ctx = eframe::egui::Context::default();
        let input = eframe::egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 900.0))),
            ..Default::default()
        };

        for _ in 0..2 {
            // Two passes: the second is the one that reuses the plot's remembered bounds.
            let mut output = ctx.run_ui(input.clone(), |ui| {
                for view in [&view, &empty] {
                    prices(ui, view, &zone);
                    pv(ui, view, &zone);
                    power(ui, view, &zone);
                    soc(ui, view, &zone);
                    adherence(ui, view, &zone);
                }
            });
            // Nobody applies texture deltas headless; leaving them queued is a panic on drop.
            output.textures_delta.clear();
            assert!(
                !output.shapes.is_empty(),
                "a frame with a plan drew nothing at all"
            );
        }
    }
}
