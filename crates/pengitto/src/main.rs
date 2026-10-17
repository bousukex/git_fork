use std::process::ExitCode;

use eframe::egui;
use pengitto::cli::Command;

fn main() -> ExitCode {
    // From an AppImage: its variables aren't passed on to what pengitto starts;
    // nor the launcher's activation token (kept for this window).
    // SAFETY: first thing, before any other thread.
    unsafe {
        pengitto::appimage::take_environment();
        pengitto::instance::take_launch_tokens();
    }
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
    let (ime_probe, alone, repos) = match pengitto::cli::parse(args) {
        Ok(Command::Version) => {
            println!("{}", pengitto::cli::version());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Help) => {
            print!("{}", pengitto::cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::SelfCheck) => return exit_code(pengitto::selfcheck::run()),
        Ok(Command::Run { ime_probe, alone, paths }) => (ime_probe, alone, paths),
        Err(e) => {
            eprintln!("pengitto: {e}\n\n{}", pengitto::cli::USAGE);
            return ExitCode::from(2);
        }
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,pengitto=info,pengitto_core=info"))
        .init();

    // One pengitto at a time: with one running, this one hands it the paths
    // (and the launcher's token) and is done.
    let mut notices = Vec::new();
    let primary = if ime_probe || alone {
        None
    } else {
        match one_at_a_time(&repos, &mut notices) {
            Ok(p) => p,
            Err(code) => return code,
        }
    };
    let link = primary.as_ref().map(|p| p.link.clone());

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
            let options = pengitto::app::AppOptions {
                state_file,
                legacy_dir: pengitto::state_file::legacy_dir(),
                instance: link,
                notices,
                // (Its own window's, one at a time or not.)
                launch_token: pengitto::instance::launch_tokens().0,
                setup: Some(pengitto_core::setup::Env::from_process("git".into())),
                ..Default::default()
            };
            Ok(Box::new(pengitto::app::PengittoApp::with_options(cc, repos, cc.storage, options)))
        }),
    );
    // (The window gone: no longer served; the socket removed.)
    drop(primary);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pengitto: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Takes the place of the one running, or hands `paths` to the one running
/// (`Err`: this one is done, with that exit code).
fn one_at_a_time(paths: &[std::path::PathBuf], notices: &mut Vec<String>) -> Result<Option<pengitto::instance::Primary>, ExitCode> {
    use pengitto::instance::{self, Claim};
    let place = match instance::place_from_env() {
        Ok(p) => p,
        Err(why) => {
            log::info!("not one at a time: {why}");
            return Ok(None);
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let (token, startup_id) = instance::launch_tokens();
    let request = instance::Request {
        paths: paths.iter().map(|p| if p.is_absolute() { p.clone() } else { cwd.join(p) }).collect(),
        cwd,
        token,
        startup_id,
        activated: false,
    };
    match instance::claim(&place, &request, instance::DEADLINE) {
        Claim::Primary(p) => Ok(Some(p)),
        Claim::Forwarded => Err(ExitCode::SUCCESS),
        Claim::Alone(notice) => {
            log::warn!("{notice}");
            notices.push(notice);
            Ok(None)
        }
        Claim::Refused(why) => {
            eprintln!("pengitto: {why}");
            Err(ExitCode::from(1))
        }
        Claim::Off(why) => {
            log::info!("not one at a time: {why}");
            Ok(None)
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
