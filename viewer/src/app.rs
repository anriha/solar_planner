//! The window: one header line about the connection, then the stacked panels.
//!
//! Deliberately thin. Everything that could disagree with the plan — metered power, the bar
//! direction, the reason text, the day totals — arrives pre-computed in the `view` payload;
//! this file lays it out and says out loud when the connection is not what it seems.

use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use jiff::tz::TimeZone;

use crate::link::{Endpoint, Link, Phase};
use crate::plots;
use crate::timefmt;
use crate::wire::View;

pub struct ViewerApp {
    link: Link,
    zone: TimeZone,
    endpoint: String,
}

impl ViewerApp {
    pub fn new(endpoint: Endpoint) -> Self {
        let shown = endpoint.url.clone();
        Self {
            link: Link::new(endpoint, None),
            zone: timefmt::prague(),
            endpoint: shown,
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, view: Option<&View>) {
        ui.horizontal(|ui| {
            let colour = match self.link.phase() {
                Phase::Open => Color32::from_rgb(0x5f, 0xc9, 0x6a),
                Phase::Connecting => Color32::from_rgb(0xf0, 0x9f, 0x3a),
                Phase::Backoff => Color32::from_rgb(0xe0, 0x6c, 0x5f),
            };
            ui.colored_label(colour, "●");
            ui.label(self.link.phase().label());
            ui.separator();
            ui.label(RichText::new(&self.endpoint).weak());

            if let Some(view) = view {
                if let Some(age) = view.age_secs() {
                    ui.separator();
                    ui.label(format!("plan computed {}", timefmt::age(age)));
                }
                ui.separator();
                ui.label(format!(
                    "{} MTUs · {} → {}",
                    view.steps.len(),
                    timefmt::date(view.from, &self.zone),
                    timefmt::date(view.to, &self.zone)
                ));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("reload").clicked() {
                    self.link.request_view();
                }
                if self.link.phase() == Phase::Backoff {
                    ui.label(
                        RichText::new(format!("retrying in {} s", self.link.backoff_secs())).weak(),
                    );
                } else {
                    ui.label(RichText::new(format!("quiet {} s", self.link.quiet_secs())).weak());
                }
            });
        });

        if let Some(error) = self.link.error() {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(0xe0, 0x6c, 0x5f), error);
            });
        }
    }

    fn day_totals(&self, ui: &mut egui::Ui, view: &View) {
        if view.days.is_empty() {
            return;
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            for day in &view.days {
                ui.label(
                    RichText::new(format!(
                        "{}  sell {:.1}  buy {:.1}  PV {:.1} kWh  net {:+.0} CZK",
                        day.day, day.sell_kwh, day.buy_kwh, day.pv_kwh, day.net_czk
                    ))
                    .monospace(),
                );
                ui.separator();
            }
        });
    }
}

impl ViewerApp {
    /// One frame. Split out of [`eframe::App::ui`] so a test can lay the window out headless
    /// with a pinned payload — the parts that can panic (a plot item, a formatter) only run
    /// when a frame is actually drawn.
    pub fn draw(&mut self, ui: &mut egui::Ui, view: Option<&View>) {
        egui::Panel::top("header").show(ui, |ui| {
            self.header(ui, view);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let Some(view) = view else {
                empty_state(ui, self.link.phase(), self.link.error(), &self.endpoint);
                return;
            };
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if view.steps.is_empty() {
                        ui.label(
                            RichText::new(
                                "the bridge has no plan to show — the planner is off, \
                                 has no horizon yet, or is disarmed",
                            )
                            .weak(),
                        );
                    } else {
                        ui.label(RichText::new("price when selling / buying, CZK/kWh").weak());
                        plots::prices(ui, view, &self.zone);
                        ui.label(RichText::new("forecast PV, DC kW").weak());
                        plots::pv(ui, view, &self.zone);
                        ui.label(RichText::new("metered AC power, kW (+ export, − import)").weak());
                        plots::power(ui, view, &self.zone);
                        ui.label(RichText::new("state of charge, %").weak());
                        plots::soc(ui, view, &self.zone);
                        ui.label(RichText::new("adherence, kW off plan").weak());
                        plots::adherence(ui, view, &self.zone);
                    }
                    self.day_totals(ui, view);
                });
        });
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.link.poll(&ctx);
        // The age line, the quiet counter and the retry countdown all tick without a frame
        // from the bridge, so repaint once a second even when nothing arrives.
        ctx.request_repaint_after(Duration::from_secs(1));

        let view = self.link.view().cloned();
        self.draw(ui, view.as_ref());
    }
}

fn empty_state(ui: &mut egui::Ui, phase: Phase, error: Option<&str>, endpoint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(80.0);
        ui.label(RichText::new("solar plan").heading());
        ui.add_space(8.0);
        match phase {
            Phase::Open => {
                ui.label("connected — waiting for the first view");
            }
            Phase::Connecting => {
                ui.label(format!("connecting to {endpoint}"));
            }
            Phase::Backoff => {
                ui.label(format!("cannot reach {endpoint} — retrying"));
            }
        }
        if let Some(error) = error {
            ui.add_space(8.0);
            ui.colored_label(Color32::from_rgb(0xe0, 0x6c, 0x5f), error);
        }
        ui.add_space(16.0);
        ui.label(
            RichText::new(
                "the page needs the bridge's token: open /app/?token=… \
                 (the desktop build takes --token or SOLAR_TOKEN)",
            )
            .weak(),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole window laid out headless: header, five panels, day totals — once with the
    /// pinned payload and once with nothing, which is the state after the bridge goes away.
    #[test]
    fn the_window_lays_out_with_a_plan_and_without_one() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/view.json");
        let view: View = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        let mut app = ViewerApp::new(Endpoint {
            url: "ws://127.0.0.1:8300/ws".to_string(),
            token: "test".to_string(),
        });

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1280.0, 940.0),
            )),
            ..Default::default()
        };

        for (index, payload) in [Some(&view), None].into_iter().enumerate() {
            let mut output = ctx.run_ui(input.clone(), |ui| app.draw(ui, payload));
            output.textures_delta.clear();
            assert!(
                !output.shapes.is_empty(),
                "frame {index} drew nothing at all"
            );
        }
    }
}
