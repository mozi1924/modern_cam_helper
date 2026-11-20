//! Binary entry point for `moderncam-gui`.

mod app;
mod fonts;
mod preview;
mod worker;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Microsoft Modern Webcam 控制台")
            .with_inner_size([1140.0, 720.0])
            .with_min_inner_size([860.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Microsoft Modern Webcam 控制台",
        options,
        Box::new(|cc| Ok(Box::new(app::ModernCamApp::new(cc)))),
    )
}
