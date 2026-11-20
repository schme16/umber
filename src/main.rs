//! Umber: GPU-accelerated UV unwrapping and texture/material painting.

mod app;
mod camera;
mod history;
mod icons;
mod mesh;
mod pen;
mod renderer;
mod texset;
mod theme;
mod widgets;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("umber=info,wgpu=warn"))
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Umber")
            .with_app_id("umber")
            .with_inner_size([1600.0, 1000.0])
            .with_min_inner_size([960.0, 600.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    let initial_file = std::env::args().nth(1).map(std::path::PathBuf::from);

    eframe::run_native(
        "Umber",
        options,
        Box::new(move |cc| Ok(Box::new(app::UmberApp::new(cc, initial_file)))),
    )
}
