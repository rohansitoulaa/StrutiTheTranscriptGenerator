//! What the window can ask for. Errors are returned as their display text.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use shruti_core::config::{Config, GpuChoice};
use shruti_core::db::{self, Added, Job};
use shruti_core::folders::{FolderTree, INBOX};
use shruti_core::llm::{LlmClient, Provider, ProviderKind};
use shruti_core::queue::{EnqueueReport, Preview};
use shruti_core::settings::Settings;
use shruti_core::transcript::TranscriptSource;
use shruti_core::{secrets, tools};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::core::{self, AppState, Core};

type Reply<T> = Result<T, String>;

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Thousands of rows stay smooth in the virtualised table (PDA UI-2).
const JOB_LIMIT: i64 = 20_000;

pub fn log_file() -> PathBuf {
    core::paths().data_dir.join("shruti.log")
}

#[derive(Serialize)]
pub struct AppStatus {
    /// The queue is processing jobs.
    ready: bool,
    error: Option<String>,
    out_dir: String,
    llm: Option<String>,
    paused: bool,
}

#[tauri::command]
pub fn status(state: State<'_, AppState>) -> AppStatus {
    let core = state.core().ok();
    AppStatus {
        // A dispatcher that stopped (another Shruti holds the queue) means
        // links would queue up and never run.
        ready: core.as_ref().is_some_and(|c| c.running()),
        error: state.download().or_else(|| state.error()),
        out_dir: core
            .as_ref()
            .map(|c| c.out_dir.display().to_string())
            .unwrap_or_default(),
        llm: core
            .as_ref()
            .map(|c| format!("{}/{}", c.config.llm.kind.as_str(), c.config.llm.model)),
        paused: core.is_some_and(|c| c.queue.is_paused()),
    }
}

#[tauri::command]
pub async fn jobs(state: State<'_, AppState>) -> Reply<Vec<Job>> {
    state.core()?.queue.jobs(JOB_LIMIT).await.map_err(text)
}

#[tauri::command]
pub async fn enqueue(state: State<'_, AppState>, lines: Vec<String>) -> Reply<EnqueueReport> {
    state.core()?.queue.enqueue(&lines).await.map_err(text)
}

#[tauri::command]
pub async fn preview(state: State<'_, AppState>, lines: Vec<String>) -> Reply<Vec<Preview>> {
    Ok(state.core()?.queue.preview(&lines).await)
}

#[tauri::command]
pub async fn retry(state: State<'_, AppState>, id: i64) -> Reply<bool> {
    state.core()?.queue.retry(id).await.map_err(text)
}

#[tauri::command]
pub async fn retry_failed(state: State<'_, AppState>) -> Reply<u64> {
    state.core()?.queue.retry_all_failed().await.map_err(text)
}

#[tauri::command]
pub async fn cancel(state: State<'_, AppState>, id: i64) -> Reply<bool> {
    state.core()?.queue.cancel(id).await.map_err(text)
}

#[derive(Serialize)]
pub struct RecleanResult {
    key: String,
    job_id: Option<i64>,
    error: Option<String>,
}

#[tauri::command]
pub async fn reclean(state: State<'_, AppState>, keys: Vec<String>) -> Reply<Vec<RecleanResult>> {
    let results = state.core()?.queue.reclean(&keys).await.map_err(text)?;
    Ok(results
        .into_iter()
        .map(|(key, added)| match added {
            Ok(Added::New(id) | Added::AlreadyOpen(id) | Added::Retried(id)) => RecleanResult {
                key,
                job_id: Some(id),
                error: None,
            },
            Ok(Added::AlreadyDone { job_id, .. }) => RecleanResult {
                key,
                job_id: Some(job_id),
                error: None,
            },
            Err(e) => RecleanResult {
                key,
                job_id: None,
                error: Some(e.to_string()),
            },
        })
        .collect())
}

#[tauri::command]
pub fn set_paused(state: State<'_, AppState>, paused: bool) -> Reply<bool> {
    let core = state.core()?;
    if paused {
        core.queue.pause();
    } else {
        core.queue.resume();
    }
    Ok(core.queue.is_paused())
}

#[derive(Serialize)]
pub struct Topic {
    name: String,
    path: String,
    notes: usize,
}

#[derive(Serialize)]
pub struct Area {
    name: String,
    topics: Vec<Topic>,
}

#[derive(Serialize)]
pub struct Folders {
    inbox: usize,
    areas: Vec<Area>,
}

/// The `Area/Topic` tree with note counts (PDA FO-12).
#[tauri::command]
pub async fn folders(state: State<'_, AppState>) -> Reply<Folders> {
    let root = state.core()?.out_dir.clone();
    tauri::async_runtime::spawn_blocking(move || scan_folders(&root))
        .await
        .map_err(text)?
}

fn scan_folders(root: &Path) -> Reply<Folders> {
    let tree = FolderTree::scan(root).map_err(text)?;
    let mut areas: Vec<Area> = Vec::new();
    for folder in tree.folders {
        let topic = Topic {
            notes: count_notes(&root.join(&folder.area).join(&folder.topic)),
            path: folder.path(),
            name: folder.topic,
        };
        match areas.iter_mut().find(|a| a.name == folder.area) {
            Some(area) => area.topics.push(topic),
            None => areas.push(Area {
                name: folder.area,
                topics: vec![topic],
            }),
        }
    }
    areas.sort_by(|a, b| a.name.cmp(&b.name));
    for area in &mut areas {
        area.topics.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(Folders {
        inbox: count_notes(&root.join(INBOX)),
        areas,
    })
}

fn count_notes(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                .count()
        })
        .unwrap_or(0)
}

#[derive(Serialize)]
pub struct TranscriptView {
    id: i64,
    key: String,
    title: Option<String>,
    language: Option<String>,
    source: String,
    raw: String,
    cleaned: Option<String>,
    translated: bool,
    review: bool,
    model: Option<String>,
    note_path: Option<String>,
}

/// The cleaned text and the raw transcript (PDA UI-4); for Hindi the raw
/// side is the original Hindi.
#[tauri::command]
pub async fn transcript(state: State<'_, AppState>, id: i64) -> Reply<TranscriptView> {
    let core = state.core()?;
    let job = job(&core, id).await?;
    let raw = db::load_transcript(&core.db, &job.video_key)
        .await
        .map_err(text)?
        .ok_or_else(|| format!("no transcript stored for {}", job.video_key))?;
    let cleaned = db::latest_cleaning(&core.db, &job.video_key)
        .await
        .map_err(text)?;
    Ok(TranscriptView {
        id,
        source: match raw.source {
            TranscriptSource::Captions(_) => "captions".into(),
            TranscriptSource::Whisper => "whisper".into(),
        },
        language: raw.language.clone(),
        raw: raw.text(),
        translated: cleaned.as_ref().is_some_and(|c| c.translated),
        review: cleaned.as_ref().is_some_and(|c| c.review),
        model: cleaned.as_ref().map(|c| c.model.clone()),
        cleaned: cleaned.map(|c| c.text),
        title: job.title,
        note_path: job.note_path,
        key: job.video_key,
    })
}

async fn job(core: &Core, id: i64) -> Reply<Job> {
    db::get_job(&core.db, id)
        .await
        .map_err(text)?
        .ok_or_else(|| format!("no job {id}"))
}

/// The job's note, as long as it's still where it was written.
async fn note(state: &State<'_, AppState>, id: i64) -> Reply<PathBuf> {
    let core = state.core()?;
    let path = job(&core, id)
        .await?
        .note_path
        .map(PathBuf::from)
        .ok_or("no note yet")?;
    match tokio::fs::metadata(&path).await {
        Ok(m) if m.is_file() => Ok(path),
        _ => Err(format!("{} is gone (moved or deleted)", path.display())),
    }
}

#[tauri::command]
pub async fn reveal(app: AppHandle, state: State<'_, AppState>, id: i64) -> Reply<()> {
    let path = note(&state, id).await?;
    app.opener().reveal_item_in_dir(path).map_err(text)
}

#[tauri::command]
pub async fn open_note(app: AppHandle, state: State<'_, AppState>, id: i64) -> Reply<()> {
    let path = note(&state, id).await?;
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(text)
}

// ---------------------------------------------------------------- Settings

/// One line per dependency: ok, or the raw error (PDA UI-7).
#[derive(Serialize)]
pub struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn check(name: &'static str, result: Result<String, String>) -> Check {
    match result {
        Ok(detail) => Check {
            name,
            ok: true,
            detail,
        },
        Err(detail) => Check {
            name,
            ok: false,
            detail,
        },
    }
}

#[tauri::command]
pub async fn health(state: State<'_, AppState>) -> Reply<Vec<Check>> {
    let core = match state.core() {
        Ok(core) => core,
        Err(e) => return Ok(vec![check("Core", Err(e))]),
    };
    let postgres = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&core.db)
        .await
        .map(|_| "connected".to_string())
        .map_err(text);
    let llm = async {
        let client = core.config.build_llm().await.map_err(text)?;
        let models = client.list_models().await.map_err(text)?;
        let model = &core.config.llm.model;
        if models.iter().any(|m| m == model) {
            Ok(client.provider().label())
        } else {
            Err(format!("{model} is not in the provider's model list"))
        }
    }
    .await;
    let gpu = Ok(match core.gpu {
        Some(n) => format!("Vulkan device {n}"),
        None => "none; Whisper runs on the CPU".to_string(),
    });
    let model = &core.config.whisper_model;
    let whisper = if model.is_file() {
        Ok(model.display().to_string())
    } else {
        Err(format!("{} is missing", model.display()))
    };
    let queue = match state.error() {
        Some(e) if !core.running() => Err(e),
        _ if core.queue.is_paused() => Ok("paused".to_string()),
        _ => Ok("running".to_string()),
    };
    Ok(vec![
        check("Postgres", postgres),
        check("LLM", llm),
        check("GPU", gpu),
        check("Whisper", whisper),
        check("Queue", queue),
    ])
}

#[derive(Serialize)]
pub struct SettingsView {
    /// What's saved; unset fields use the defaults below.
    settings: Settings,
    defaults: Defaults,
    /// Whether a key is stored, per cloud provider. Keys never leave the
    /// keyring (LLM-5).
    keys: Vec<(String, bool)>,
    log_file: String,
}

/// The values in effect when a setting is left blank.
#[derive(Serialize)]
pub struct Defaults {
    out_dir: String,
    database_url: String,
    llm_provider: String,
    llm_model: String,
    llm_base_url: String,
    llm_concurrency: usize,
    chunk_tokens: usize,
    cleanup_prompt: String,
    min_ratio: f32,
    max_ratio: f32,
    max_ratio_translated: f32,
    whisper_model: String,
    gpu: String,
    ig_delay_secs: u64,
    ig_batch: u32,
    ig_pause_secs: u64,
}

fn defaults(paths: &core::Paths) -> Reply<Defaults> {
    let blank = Settings::default();
    let c: Config = core::config_for(&blank, paths)?;
    let cleanup = c.cleanup_settings();
    Ok(Defaults {
        out_dir: core::out_dir(&blank, paths)
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        database_url: c.database_url.clone(),
        llm_provider: c.llm.kind.as_str().to_string(),
        llm_model: c.llm.model.clone(),
        llm_base_url: c.llm_base_url().unwrap_or_default(),
        llm_concurrency: c.limits.llm,
        chunk_tokens: cleanup.chunk_tokens,
        cleanup_prompt: cleanup.prompt,
        min_ratio: cleanup.min_ratio,
        max_ratio: cleanup.max_ratio,
        max_ratio_translated: cleanup.max_ratio_translated,
        whisper_model: c.whisper_model.display().to_string(),
        gpu: match c.gpu {
            GpuChoice::Auto => "auto".into(),
            GpuChoice::Device(n) => n.to_string(),
        },
        ig_delay_secs: c.limits.instagram.gap.as_secs(),
        ig_batch: c.limits.instagram.batch,
        ig_pause_secs: c.limits.instagram.pause.as_secs(),
    })
}

const CLOUD: [ProviderKind; 3] = [
    ProviderKind::OpenAi,
    ProviderKind::Gemini,
    ProviderKind::Custom,
];

async fn has_key(kind: ProviderKind) -> bool {
    tauri::async_runtime::spawn_blocking(move || secrets::api_key(kind))
        .await
        .is_ok_and(|r| r.is_ok_and(|k| k.is_some()))
}

#[tauri::command]
pub async fn get_settings() -> Reply<SettingsView> {
    let paths = core::paths();
    let settings = Settings::load(&paths.settings_file).map_err(text)?;
    let mut keys = Vec::new();
    for kind in CLOUD {
        keys.push((kind.as_str().to_string(), has_key(kind).await));
    }
    Ok(SettingsView {
        settings,
        defaults: defaults(&paths)?,
        keys,
        log_file: log_file().display().to_string(),
    })
}

/// Checks the settings would load, saves them, and restarts the core.
#[tauri::command]
pub async fn save_settings(app: AppHandle, settings: Settings) -> Reply<()> {
    let paths = core::paths();
    let settings = settings.normalized();
    core::config_for(&settings, &paths)?;
    settings.save(&paths.settings_file).map_err(text)?;
    core::restart(&app).await;
    Ok(())
}

fn provider_kind(name: &str) -> Reply<ProviderKind> {
    ProviderKind::parse(name).ok_or_else(|| format!("unknown provider {name:?}"))
}

#[tauri::command]
pub async fn set_api_key(app: AppHandle, provider: String, key: String) -> Reply<()> {
    let kind = provider_kind(&provider)?;
    tauri::async_runtime::spawn_blocking(move || secrets::set_api_key(kind, &key))
        .await
        .map_err(text)?
        .map_err(text)?;
    // The key is read when the core starts.
    core::restart(&app).await;
    Ok(())
}

#[tauri::command]
pub async fn remove_api_key(app: AppHandle, provider: String) -> Reply<()> {
    let kind = provider_kind(&provider)?;
    tauri::async_runtime::spawn_blocking(move || secrets::delete_api_key(kind))
        .await
        .map_err(text)?
        .map_err(text)?;
    core::restart(&app).await;
    Ok(())
}

/// A client for a provider as it's being edited, before it's saved.
async fn client(provider: &str, model: &str, base_url: Option<String>) -> Reply<LlmClient> {
    let kind = provider_kind(provider)?;
    let base_url = base_url
        .filter(|u| !u.trim().is_empty())
        .or_else(|| kind.default_base_url().map(str::to_string))
        .ok_or("a custom provider needs a base URL")?;
    let api_key = if kind.is_local() {
        None
    } else {
        tauri::async_runtime::spawn_blocking(move || secrets::api_key(kind))
            .await
            .map_err(text)?
            .map_err(text)?
    };
    LlmClient::new(Provider {
        kind,
        base_url,
        api_key,
        model: model.to_string(),
    })
    .map_err(text)
}

/// The provider's own model list (LLM-3).
#[tauri::command]
pub async fn list_models(provider: String, base_url: Option<String>) -> Reply<Vec<String>> {
    let client = client(&provider, "", base_url).await?;
    let mut models = client.list_models().await.map_err(text)?;
    models.sort();
    Ok(models)
}

/// A tiny prompt: the reply time, or the exact error (LLM-4).
#[tauri::command]
pub async fn test_llm(provider: String, model: String, base_url: Option<String>) -> Reply<u64> {
    let client = client(&provider, &model, base_url).await?;
    let started = Instant::now();
    client.test().await.map_err(text)?;
    Ok(started.elapsed().as_millis() as u64)
}

#[derive(Serialize)]
pub struct YtDlpInfo {
    path: String,
    version: Option<String>,
    /// Days since that release (YT-2).
    age_days: Option<i64>,
    /// Shruti's own copy is the one in use (YT-3).
    managed: bool,
    latest: Option<String>,
    error: Option<String>,
}

#[tauri::command]
pub async fn ytdlp_info(state: State<'_, AppState>) -> Reply<YtDlpInfo> {
    let paths = core::paths();
    let settings = Settings::load(&paths.settings_file).map_err(text)?;
    let bin = core::config_for(&settings, &paths)?.ytdlp_bin;
    let (version, error) = match tools::ytdlp_version(&bin).await {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let today = chrono::Local::now().date_naive();
    let age_days = version
        .as_deref()
        .and_then(tools::version_date)
        .map(|d| (today - d).num_days());
    let latest = tools::latest_ytdlp(&state.http, tools::GITHUB_API)
        .await
        .ok()
        .map(|r| r.tag);
    Ok(YtDlpInfo {
        managed: bin == tools::managed_ytdlp(&paths.data_dir),
        path: bin.display().to_string(),
        version,
        age_days,
        latest,
        error,
    })
}

/// Installs the latest yt-dlp into the data folder and switches to it.
#[tauri::command]
pub async fn update_ytdlp(app: AppHandle, state: State<'_, AppState>) -> Reply<String> {
    let release = tools::latest_ytdlp(&state.http, tools::GITHUB_API)
        .await
        .map_err(text)?;
    let version = tools::install_ytdlp(&state.http, &release, &core::paths().data_dir)
        .await
        .map_err(text)?;
    core::restart(&app).await;
    Ok(version)
}

#[tauri::command]
pub fn open_log(app: AppHandle) -> Reply<()> {
    app.opener()
        .open_path(log_file().display().to_string(), None::<&str>)
        .map_err(text)
}
