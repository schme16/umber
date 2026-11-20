//! Umber: GPU-accelerated UV unwrapping and texture/material painting.

mod app;
mod brush;
mod brush_texture;
mod camera;
mod commands;
mod export;
mod help;
mod history;
mod icons;
mod mesh;
mod pen;
mod project;
mod renderer;
mod stats;
mod texset;
mod theme;
mod unwrap;
mod widgets;

/// eframe's GPU setup, plus timestamp queries where the adapter has them, so the F3 overlay
/// can time each pass.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup};
    let mut options = WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_setup {
        let base = setup.device_descriptor.clone();
        setup.device_descriptor = std::sync::Arc::new(move |adapter| {
            let mut desc = base(adapter);
            desc.required_features |= stats::wanted_features(adapter);
            desc
        });
    }
    options
}

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
        wgpu_options: wgpu_options(),
        ..Default::default()
    };

    let initial_file = std::env::args().nth(1).map(std::path::PathBuf::from);

    eframe::run_native(
        "Umber",
        options,
        Box::new(move |cc| Ok(Box::new(app::UmberApp::new(cc, initial_file)))),
    )
}
