use std::process::ExitCode;

use eframe::egui;
use pengitto::cli::Command;

fn main() -> ExitCode {
    // From an AppImage: its variables aren't passed on to what pengitto starts.
    // SAFETY: first thing, before any other thread.
    unsafe { pengitto::appimage::take_environment() };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // Started by git as the editor of an interactive rebase (no window).
    if let Some(code) = pengitto_core::interactive::run_editor(&args) {
        return exit_code(code);
    }
    // Started by git or ssh to ask for credentials (a prompt that looks
    // like an option is still a prompt).
    if let Some(code) = pengitto::askpass::run_client() {
        return exit_code(code);
    }
    let (ime_probe, repos) = match pengitto::cli::parse(args) {
        Ok(Command::Version) => {
            println!("{}", pengitto::cli::version());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Help) => {
            print!("{}", pengitto::cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::SelfCheck) => return exit_code(pengitto::selfcheck::run()),
        Ok(Command::Run { ime_probe, paths }) => (ime_probe, paths),
        Err(e) => {
            eprintln!("pengitto: {e}\n\n{}", pengitto::cli::USAGE);
            return ExitCode::from(2);
        }
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,pengitto=info,pengitto_core=info"))
        .init();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("pengitto")
        .with_app_id("pengitto")
        .with_inner_size([1360.0, 860.0])
        .with_min_inner_size([720.0, 480.0]);
    // The window's icon where the desktop takes it from the window (X11);
    // on Wayland it comes from pengitto.desktop.
    match eframe::icon_data::from_png_bytes(pengitto::WINDOW_ICON) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => log::warn!("the window icon can't be read: {e}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let state_file = pengitto::state_file::default_path();

    let result = eframe::run_native(
        "pengitto",
        options,
        Box::new(move |cc| {
            if ime_probe {
                pengitto::setup_context(&cc.egui_ctx);
                return Ok(Box::new(pengitto::probe::ImeProbe::default()));
            }
            let options =
                pengitto::app::AppOptions { state_file, legacy_dir: pengitto::state_file::legacy_dir(), ..Default::default() };
            Ok(Box::new(pengitto::app::PengittoApp::with_options(cc, repos, cc.storage, options)))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pengitto: {e}");
            ExitCode::FAILURE
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
