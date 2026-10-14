use std::process::ExitCode;

use eframe::egui;
use tine::cli::Command;

fn main() -> ExitCode {
    // From an AppImage: its variables aren't passed on to what Tine starts.
    // SAFETY: first thing, before any other thread.
    unsafe { tine::appimage::take_environment() };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // Started by git as the editor of an interactive rebase (no window).
    if let Some(code) = tine_core::interactive::run_editor(&args) {
        return exit_code(code);
    }
    // Started by git or ssh to ask for credentials (a prompt that looks
    // like an option is still a prompt).
    if let Some(code) = tine::askpass::run_client() {
        return exit_code(code);
    }
    let (ime_probe, repos) = match tine::cli::parse(args) {
        Ok(Command::Version) => {
            println!("{}", tine::cli::version());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Help) => {
            print!("{}", tine::cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::SelfCheck) => return exit_code(tine::selfcheck::run()),
        Ok(Command::Run { ime_probe, paths }) => (ime_probe, paths),
        Err(e) => {
            eprintln!("tine: {e}\n\n{}", tine::cli::USAGE);
            return ExitCode::from(2);
        }
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,tine=info,tine_core=info"))
        .init();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Tine")
        .with_app_id("tine")
        .with_inner_size([1360.0, 860.0])
        .with_min_inner_size([720.0, 480.0]);
    // The window's icon where the desktop takes it from the window (X11);
    // on Wayland it comes from tine.desktop.
    match eframe::icon_data::from_png_bytes(tine::WINDOW_ICON) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => log::warn!("the window icon can't be read: {e}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let state_file = tine::state_file::default_path();

    let result = eframe::run_native(
        "tine",
        options,
        Box::new(move |cc| {
            if ime_probe {
                tine::setup_context(&cc.egui_ctx);
                return Ok(Box::new(tine::probe::ImeProbe::default()));
            }
            let options = tine::app::AppOptions { state_file, ..Default::default() };
            Ok(Box::new(tine::app::TineApp::with_options(cc, repos, cc.storage, options)))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tine: {e}");
            ExitCode::FAILURE
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
