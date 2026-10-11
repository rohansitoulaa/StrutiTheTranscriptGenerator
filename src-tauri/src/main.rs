//! The Shruti desktop app: the core's queue behind a Tauri window.

mod commands;
mod core;

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Logs go to stderr and to `shruti.log` in the data folder (shown in
/// Settings), started afresh once it passes 5 MB.
fn init_logging(log: &Path) {
    let file = log
        .parent()
        .and_then(|dir| std::fs::create_dir_all(dir).ok())
        .and_then(|()| {
            if std::fs::metadata(log).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
                let _ = std::fs::write(log, "");
            }
            OpenOptions::new().create(true).append(true).open(log).ok()
        });
    let builder = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false);
    match file {
        Some(file) => builder
            .with_writer(std::io::stderr.and(Mutex::new(file)))
            .init(),
        None => builder.with_writer(std::io::stderr).init(),
    }
}

fn main() {
    init_logging(&commands::log_file());

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(core::AppState::default())
        .setup(|app| {
            core::start(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::status,
            commands::jobs,
            commands::enqueue,
            commands::preview,
            commands::retry,
            commands::retry_failed,
            commands::cancel,
            commands::reclean,
            commands::set_paused,
            commands::folders,
            commands::transcript,
            commands::reveal,
            commands::open_note,
            commands::health,
            commands::get_settings,
            commands::save_settings,
            commands::set_api_key,
            commands::remove_api_key,
            commands::list_models,
            commands::test_llm,
            commands::ytdlp_info,
            commands::update_ytdlp,
            commands::open_log,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the app")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                // Jobs in flight stay `running` and resume on the next start.
                core::exit_requested(app, api);
            }
        });
}
