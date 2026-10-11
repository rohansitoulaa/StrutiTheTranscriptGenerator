//! What the window can ask for. Errors are returned as their display text.

use std::path::{Path, PathBuf};

use serde::Serialize;
use shruti_core::db::{self, Added, Job};
use shruti_core::folders::{FolderTree, INBOX};
use shruti_core::queue::{EnqueueReport, Preview};
use shruti_core::transcript::TranscriptSource;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::core::{AppState, Core};

type Reply<T> = Result<T, String>;

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Thousands of rows stay smooth in the virtualised table (PDA UI-2).
const JOB_LIMIT: i64 = 20_000;

#[derive(Serialize)]
pub struct AppStatus {
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
        ready: core.is_some(),
        error: state.error(),
        out_dir: core
            .map(|c| c.out_dir.display().to_string())
            .unwrap_or_default(),
        llm: core.map(|c| c.llm.clone()),
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
    let queue = &state.core()?.queue;
    if paused {
        queue.pause();
    } else {
        queue.resume();
    }
    Ok(queue.is_paused())
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
    let job = job(core, id).await?;
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
    let path = job(core, id)
        .await?
        .note_path
        .map(PathBuf::from)
        .ok_or("no note yet")?;
    if !path.is_file() {
        return Err(format!("{} is gone (moved or deleted)", path.display()));
    }
    Ok(path)
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
