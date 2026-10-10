use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,tine=info,tine_core=info"))
        .init();

    let mut probe = false;
    let mut repos = Vec::new();
    for arg in std::env::args_os().skip(1) {
        if arg == "--ime-probe" {
            probe = true;
        } else {
            repos.push(PathBuf::from(arg));
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tine")
            .with_app_id("tine")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "tine",
        options,
        Box::new(move |cc| {
            if probe {
                tine::setup_context(&cc.egui_ctx);
                return Ok(Box::new(tine::probe::ImeProbe::default()));
            }
            Ok(Box::new(tine::app::TineApp::new(cc, repos)))
        }),
    )
}
