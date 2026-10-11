//! The Shruti desktop app: the core's queue behind a Tauri window.
//!
//! The core starts in the background so the window opens at once; until it
//! is ready (or if it can't start, e.g. Postgres is down) commands answer
//! with the reason, which the UI shows raw (PDA UI-7, UI-8).

mod commands;
mod core;

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    tauri::Builder::default()
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
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Jobs in flight stay `running` and resume on the next start.
                core::stop(app);
            }
        });
}
