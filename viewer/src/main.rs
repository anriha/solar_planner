//! Entry point for both targets.
//!
//! Native: `cargo run -p viewer -- --url ws://127.0.0.1:8300/ws --token …`
//! Web: `trunk build --release` produces `viewer/dist`, which the bridge serves at `/app/`.
//! The same `ViewerApp` runs in both; only the endpoint's origin differs.

mod app;
mod link;
mod plots;
mod timefmt;
mod wire;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    use eframe::egui;

    let endpoint = match link::Endpoint::from_args(std::env::args().skip(1)) {
        Ok(endpoint) => endpoint,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 940.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("solar plan"),
        ..Default::default()
    };

    eframe::run_native(
        "solar plan",
        options,
        Box::new(move |_cc| Ok(Box::new(app::ViewerApp::new(endpoint)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast;

    console_error_panic_hook::set_once();
    wasm_bindgen_futures::spawn_local(async move {
        let document = web_sys::window()
            .expect("a browser has a window")
            .document()
            .expect("a document");
        let canvas = document
            .get_element_by_id("the_canvas_id")
            .expect("index.html carries the canvas");
        let canvas: web_sys::HtmlCanvasElement = canvas.dyn_into().expect("a canvas element");

        eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|_cc| {
                    Ok(Box::new(app::ViewerApp::new(
                        link::Endpoint::from_location(),
                    )))
                }),
            )
            .await
            .expect("the viewer failed to start");
    });
}
