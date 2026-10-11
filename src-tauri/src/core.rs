//! Starting, restarting and stopping the core (settings, Postgres, Whisper,
//! LLM, queue) behind the window. The core starts in the background so the
//! window opens at once; while it can't run, commands answer with the
//! reason, which the UI shows raw (PDA UI-7, UI-8).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use shruti_core::config::Config;
use shruti_core::db::{self, Status};
use shruti_core::folders::INBOX;
use shruti_core::llm::LlmClient;
use shruti_core::queue::{Queue, QueueParts};
use shruti_core::settings::{self, Settings};
use shruti_core::tools;
use shruti_core::whisper::WhisperCli;
use shruti_core::ytdlp::YtDlp;
use sqlx::PgPool;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

pub type AppQueue = Queue<YtDlp, WhisperCli, LlmClient>;

pub struct Core {
    pub queue: Arc<AppQueue>,
    pub db: PgPool,
    /// The `Shruti/` root notes are filed under.
    pub out_dir: PathBuf,
    pub config: Config,
    /// The Vulkan device Whisper runs on, if a discrete GPU was found.
    pub gpu: Option<u32>,
    shutdown: CancellationToken,
    /// Cleared when the dispatcher stops, e.g. another Shruti holds the queue.
    running: AtomicBool,
    dispatcher: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl Core {
    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub struct AppState {
    core: RwLock<Option<Arc<Core>>>,
    /// Why the core isn't running, raw.
    error: Mutex<Option<String>>,
    /// Model download progress while it lasts.
    download: Mutex<Option<String>>,
    exiting: AtomicBool,
    /// One start, restart or stop at a time: a save during first launch
    /// must not leave two cores (and two dispatchers) running.
    lifecycle: tokio::sync::Mutex<()>,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn core(&self) -> Result<Arc<Core>, String> {
        self.core
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                self.download()
                    .or_else(|| self.error())
                    .unwrap_or_else(|| "starting".to_string())
            })
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }

    pub fn download(&self) -> Option<String> {
        lock(&self.download).clone()
    }

    fn set_error(&self, e: Option<String>) {
        *lock(&self.error) = e;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where Shruti keeps its own files.
pub struct Paths {
    pub home: PathBuf,
    /// `~/.local/share/shruti`: its yt-dlp, models and log.
    pub data_dir: PathBuf,
    pub settings_file: PathBuf,
}

pub fn paths() -> Paths {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    Paths {
        data_dir: home.join(".local/share/shruti"),
        settings_file: Settings::path(&home),
        home,
    }
}

/// The repo checkout this was built from: tools and models for debug builds.
pub fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The `Shruti/` folder: Settings, else `SHRUTI_OUT_DIR`, else `out/` in
/// the repo while developing, else inside the Obsidian vault.
pub fn out_dir(settings: &Settings, paths: &Paths) -> Result<PathBuf, String> {
    if let Some(dir) = &settings.out_dir {
        return Ok(dir.clone());
    }
    if let Some(dir) = env("SHRUTI_OUT_DIR").filter(|v| !v.is_empty()) {
        return Ok(dir.into());
    }
    if cfg!(debug_assertions) {
        return Ok(repo_root().join("out"));
    }
    settings::default_out_dir(&paths.home)
        .ok_or_else(|| "no Obsidian vault found; set the Shruti folder in Settings".to_string())
}

/// The effective configuration for `settings`, with Shruti's own yt-dlp
/// once it has one (YT-3), unless `SHRUTI_YTDLP` says otherwise.
pub fn config_for(settings: &Settings, paths: &Paths) -> Result<Config, String> {
    let user = env("USER").unwrap_or_default();
    let defaults = if cfg!(debug_assertions) {
        Config::defaults(repo_root(), &paths.home, &user)
    } else {
        // Installed: the bundled tools sit next to the executable.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        Config::packaged(&exe_dir, &paths.data_dir, &paths.home, &user)
    };
    let mut config = Config::load_over(defaults, settings, env).map_err(|e| e.to_string())?;
    let managed = tools::managed_ytdlp(&paths.data_dir);
    if env("SHRUTI_YTDLP").is_none() && managed.is_file() {
        config.ytdlp_bin = managed;
    }
    Ok(config)
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let _turn = state.lifecycle.lock().await;
        start_core(&app).await;
    });
}

/// Applies saved settings: the queue stops (jobs in flight resume where
/// they were) and starts again with the new configuration.
pub async fn restart(app: &AppHandle) {
    let state = app.state::<AppState>();
    let _turn = state.lifecycle.lock().await;
    stop_core(app).await;
    start_core(app).await;
}

async fn start_core(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.set_error(None);
    let _ = app.emit("core-status", ());
    match build(app).await {
        Ok(core) => {
            let core = Arc::new(core);
            *state.core.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&core));
            forward_events(app, &core);
            run_queue(app, &core);
        }
        Err(e) => {
            tracing::error!(error = %e, "the core could not start");
            state.set_error(Some(e));
        }
    }
    let _ = app.emit("core-status", ());
}

/// Stops the dispatcher and waits for it (so the advisory lock is released
/// with the closed pool, not when Postgres notices a dead connection).
async fn stop_core(app: &AppHandle) {
    let state = app.state::<AppState>();
    let core = state.core.write().unwrap_or_else(|e| e.into_inner()).take();
    let Some(core) = core else { return };
    core.shutdown.cancel();
    let dispatcher = lock(&core.dispatcher).take();
    if let Some(handle) = dispatcher
        && tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .is_err()
    {
        tracing::warn!("the queue took over 15 s to stop");
    }
    core.db.close().await;
}

/// Quitting waits for the queue to stop cleanly, then exits.
pub fn exit_requested(app: &AppHandle, api: &tauri::ExitRequestApi) {
    let state = app.state::<AppState>();
    if state.exiting.swap(true, Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let _turn = state.lifecycle.lock().await;
        stop_core(&app).await;
        app.exit(0);
    });
}

async fn build(app: &AppHandle) -> Result<Core, String> {
    let paths = paths();
    let settings = Settings::load(&paths.settings_file).map_err(|e| e.to_string())?;
    let out_dir = out_dir(&settings, &paths)?;
    // First launch (UI-7): the root and its inbox exist from the start.
    let inbox = out_dir.join(INBOX);
    std::fs::create_dir_all(&inbox)
        .map_err(|e| format!("could not create {}: {e}", inbox.display()))?;
    let config = config_for(&settings, &paths)?;
    fetch_models(app, &config).await?;
    let problems = config.problems();
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    let engine = config.build_engine().await.map_err(|e| e.to_string())?;
    let gpu = engine.transcriber.gpu_device;
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|e| e.to_string())?;
    let llm = config.build_llm().await.map_err(|e| e.to_string())?;
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
        config,
        gpu,
        shutdown: CancellationToken::new(),
        running: AtomicBool::new(false),
        dispatcher: Mutex::new(None),
    })
}

/// Downloads the Whisper (and VAD) model if missing, showing progress
/// (UI-7). In a repo checkout they're already in `models/`.
async fn fetch_models(app: &AppHandle, config: &Config) -> Result<(), String> {
    let mut models = vec![tools::ModelFile {
        url: tools::WHISPER_MODEL_URL,
        sha256: tools::WHISPER_MODEL_SHA256,
        path: &config.whisper_model,
    }];
    if let Some(vad) = &config.vad_model {
        models.push(tools::ModelFile {
            url: tools::VAD_MODEL_URL,
            sha256: tools::VAD_MODEL_SHA256,
            path: vad,
        });
    }
    if models.iter().all(|m| m.path.is_file()) {
        return Ok(());
    }
    let state = app.state::<AppState>();
    let last = Mutex::new(u64::MAX);
    let result = tools::ensure_models(&state.http, &models, |path, done, total| {
        let Some(total) = total.filter(|t| *t > 0) else {
            return;
        };
        let pct = done * 100 / total;
        let mut last = lock(&last);
        if *last != pct {
            *last = pct;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            *lock(&state.download) = Some(format!("downloading {name} {pct}%"));
            let _ = app.emit("core-status", ());
        }
    })
    .await;
    *lock(&state.download) = None;
    result.map_err(|e| e.to_string())
}

/// Job events go to the window as `job` (missed ones as `jobs-stale`), and
/// a notification goes out when the queue empties (Q-8, FO-11).
fn forward_events(app: &AppHandle, core: &Arc<Core>) {
    let mut events = core.queue.subscribe();
    let app = app.clone();
    let core = Arc::clone(core);
    tauri::async_runtime::spawn(async move {
        let mut busy_since = None;
        loop {
            // Ends with its core; otherwise every restart would leave one
            // behind, holding the old queue.
            let event = tokio::select! {
                _ = core.shutdown.cancelled() => break,
                event = events.recv() => event,
            };
            match event {
                Ok(e) => {
                    let _ = app.emit("job", &e);
                    match e.status {
                        Status::Running => {
                            busy_since.get_or_insert_with(|| {
                                chrono::Utc::now() - chrono::Duration::seconds(5)
                            });
                        }
                        Status::Done | Status::Failed | Status::Cancelled | Status::Waiting => {
                            if let Some(since) = busy_since
                                && matches!(db::open_count(&core.db).await, Ok(0))
                            {
                                busy_since = None;
                                notify_batch(&app, &core, since).await;
                            }
                        }
                        Status::Queued => {}
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = app.emit("jobs-stale", ());
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

async fn notify_batch(app: &AppHandle, core: &Core, since: chrono::DateTime<chrono::Utc>) {
    let summary = match db::summary_since(&core.db, since).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not count the finished batch");
            return;
        }
    };
    if summary.done + summary.failed == 0 {
        return;
    }
    let mut body = format!("{} done", summary.done);
    if summary.review > 0 {
        body.push_str(&format!(" · {} review", summary.review));
    }
    if summary.failed > 0 {
        body.push_str(&format!(" · {} failed", summary.failed));
    }
    if !summary.new_folders.is_empty() {
        body.push_str(&format!("\nNew: {}", summary.new_folders.join(", ")));
    }
    if let Err(e) = app
        .notification()
        .builder()
        .title("Shruti")
        .body(body)
        .show()
    {
        tracing::warn!(error = %e, "could not show the notification");
    }
}

fn run_queue(app: &AppHandle, core: &Arc<Core>) {
    core.running.store(true, Ordering::SeqCst);
    let app = app.clone();
    let task_core = Arc::clone(core);
    let handle = tauri::async_runtime::spawn(async move {
        let result = Arc::clone(&task_core.queue)
            .run(task_core.shutdown.clone())
            .await;
        task_core.running.store(false, Ordering::SeqCst);
        if let Err(e) = result {
            // E.g. the dev CLI is already working the queue.
            tracing::error!(error = %e, "the queue stopped");
            app.state::<AppState>().set_error(Some(e.to_string()));
            let _ = app.emit("core-status", ());
        }
    });
    *lock(&core.dispatcher) = Some(handle);
}
