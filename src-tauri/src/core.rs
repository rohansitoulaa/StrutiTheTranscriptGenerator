//! Starting the core (config, Postgres, Whisper, LLM, queue) and holding it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use shruti_core::config::Config;
use shruti_core::db;
use shruti_core::llm::LlmClient;
use shruti_core::queue::{Queue, QueueParts};
use shruti_core::whisper::WhisperCli;
use shruti_core::ytdlp::YtDlp;
use sqlx::PgPool;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

pub type AppQueue = Queue<YtDlp, WhisperCli, LlmClient>;

pub struct Core {
    pub queue: Arc<AppQueue>,
    pub db: PgPool,
    /// The `Shruti/` root notes are filed under.
    pub out_dir: PathBuf,
    pub llm: String,
}

#[derive(Default)]
pub struct AppState {
    core: OnceLock<Core>,
    /// Why the core isn't running, raw.
    error: Mutex<Option<String>>,
    shutdown: CancellationToken,
}

impl AppState {
    pub fn core(&self) -> Result<&Core, String> {
        self.core
            .get()
            .ok_or_else(|| self.error().unwrap_or_else(|| "starting".to_string()))
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_error(&self, e: impl Into<String>) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.into());
    }
}

/// Repo root in development; packaging (build step 6) points the tool
/// paths elsewhere.
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
}

/// Notes go to `SHRUTI_OUT_DIR`, else `out/` in the repo while developing;
/// the vault is chosen in Settings (build step 5).
fn out_dir() -> PathBuf {
    std::env::var_os("SHRUTI_OUT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("out"))
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        match build().await {
            Ok(core) => {
                let queue = Arc::clone(&core.queue);
                let _ = state.core.set(core);
                forward_events(&app, &queue);
                run_queue(app.clone(), queue, state.shutdown.clone());
            }
            Err(e) => {
                tracing::error!(error = %e, "the core could not start");
                state.set_error(e);
            }
        }
        let _ = app.emit("core-status", ());
    });
}

pub fn stop(app: &AppHandle) {
    app.state::<AppState>().shutdown.cancel();
}

async fn build() -> Result<Core, String> {
    let config =
        Config::from_env(repo_root(), |k| std::env::var(k).ok()).map_err(|e| e.to_string())?;
    let problems = config.problems();
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    let engine = config.build_engine().await.map_err(|e| e.to_string())?;
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|e| e.to_string())?;
    let llm = config.build_llm().await.map_err(|e| e.to_string())?;
    let label = llm.provider().label();
    let out_dir = out_dir();
    let queue = Queue::new(QueueParts {
        db: pool.clone(),
        engine,
        llm,
        cleanup: config.cleanup_settings(),
        work_dir: config.work_dir.clone(),
        out_dir: out_dir.clone(),
        limits: config.limits,
        expand_limit: None,
    });
    Ok(Core {
        queue,
        db: pool,
        out_dir,
        llm: label,
    })
}

/// Job events go to the window as `job`; missed ones as `jobs-stale`.
fn forward_events(app: &AppHandle, queue: &Arc<AppQueue>) {
    let mut events = queue.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(e) => {
                    let _ = app.emit("job", &e);
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = app.emit("jobs-stale", ());
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn run_queue(app: AppHandle, queue: Arc<AppQueue>, shutdown: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = queue.run(shutdown).await {
            // E.g. the dev CLI is already working the queue.
            tracing::error!(error = %e, "the queue stopped");
            app.state::<AppState>().set_error(e.to_string());
            let _ = app.emit("core-status", ());
        }
    });
}
