//! The job queue (PDA §4.7).
//!
//! A dispatcher claims queued jobs from Postgres and runs each through its
//! stages. Stages share separate limits, so downloads (network-bound) overlap
//! with Whisper (GPU-bound): while video N transcribes, N+1 downloads (Q-2).
//! Every finished stage is recorded, so a retry or a restart resumes at the
//! next one instead of starting over (Q-4, Q-5). Only one process works the
//! queue at a time (a Postgres advisory lock).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use sqlx::PgPool;
use tokio::sync::{Notify, Semaphore, broadcast};
use tokio::task::{AbortHandle, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cleanup::{self, CleanupSettings};
use crate::db::{self, Added, Job, Stage, Status};
use crate::error::fs_err;
use crate::folders::{self, FolderTree, Placement};
use crate::llm::Llm;
use crate::metadata::VideoMeta;
use crate::output::{
    find_notes_by_id, note_title, render_note, sha256_hex, update_note, write_note,
};
use crate::pipeline::{Engine, Fetched};
use crate::source::Source;
use crate::transcript::Transcript;
use crate::whisper::Transcriber;
use crate::ytdlp::{AUDIO_FILE, Fetcher};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Concurrent metadata/caption/audio fetches.
    pub downloads: usize,
    /// Concurrent Whisper runs. One saturates the GPU; two only slow both.
    pub whisper: usize,
    /// Concurrent LLM requests: 1 for a local model, a few for cloud
    /// providers (PDA LLM-7).
    pub llm: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            downloads: 3,
            whisper: 1,
            llm: 1,
        }
    }
}

/// Sent on every stage or status change, for the UI's live queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobEvent {
    pub job_id: i64,
    pub stage: Stage,
    pub status: Status,
    pub reason: Option<String>,
}

/// What pasting a batch of lines did, line by line (PDA IN-3, IN-4).
#[derive(Debug, Default, Serialize)]
pub struct EnqueueReport {
    pub batch_id: Uuid,
    pub added: Vec<i64>,
    pub retried: Vec<i64>,
    pub already_queued: Vec<i64>,
    /// `(input, note path)` for links finished before.
    pub already_done: Vec<(String, Option<String>)>,
    /// `(input, reason)` for lines that aren't usable links.
    pub rejected: Vec<(String, String)>,
    /// `(input, reason)` for valid links that couldn't be queued (a
    /// database error). Other lines still go in.
    pub errors: Vec<(String, String)>,
}

/// The label a job row shows (PDA Q-1).
pub fn state_label(stage: Stage, status: Status) -> &'static str {
    match (status, stage) {
        (Status::Queued, _) => "queued",
        (Status::Waiting, _) => "waiting",
        (Status::Failed, _) => "failed",
        (Status::Cancelled, _) => "cancelled",
        (Status::Done, _) | (_, Stage::Done) => "done",
        (Status::Running, Stage::Metadata) => "fetching metadata",
        (Status::Running, Stage::Fetch) => "downloading",
        (Status::Running, Stage::Transcribe) => "transcribing",
        (Status::Running, Stage::Clean) => "cleaning",
        (Status::Running, Stage::File) => "filing",
        (Status::Running, Stage::Write) => "writing",
    }
}

pub struct Queue<F, T, L> {
    db: PgPool,
    engine: Engine<F, T>,
    llm: L,
    cleanup: CleanupSettings,
    /// Per-job audio lives here between the fetch and transcribe stages, so
    /// a restart doesn't re-download it.
    work_dir: PathBuf,
    /// The `Shruti/` root in the vault; notes go in `Area/Topic` below it.
    out_dir: PathBuf,
    limits: Limits,
    downloads: Semaphore,
    whisper: Semaphore,
    llm_slots: Semaphore,
    wake: Notify,
    paused: AtomicBool,
    events: broadcast::Sender<JobEvent>,
    running: Mutex<HashMap<i64, AbortHandle>>,
    today: fn() -> String,
}

fn local_today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Each spawned job task, by task id: the job it runs and its stage at claim.
type TaskJobs = HashMap<tokio::task::Id, (i64, Stage)>;

/// The pieces a queue is built from.
pub struct QueueParts<F, T, L> {
    pub db: PgPool,
    pub engine: Engine<F, T>,
    pub llm: L,
    pub cleanup: CleanupSettings,
    pub work_dir: PathBuf,
    pub out_dir: PathBuf,
    pub limits: Limits,
}

impl<F, T, L> Queue<F, T, L>
where
    F: Fetcher + Send + Sync + 'static,
    T: Transcriber + Send + Sync + 'static,
    L: Llm + Send + Sync + 'static,
{
    pub fn new(parts: QueueParts<F, T, L>) -> Arc<Self> {
        let QueueParts {
            db,
            engine,
            llm,
            cleanup,
            work_dir,
            out_dir,
            limits,
        } = parts;
        Arc::new(Queue {
            db,
            engine,
            llm,
            cleanup,
            work_dir,
            out_dir,
            downloads: Semaphore::new(limits.downloads.max(1)),
            whisper: Semaphore::new(limits.whisper.max(1)),
            llm_slots: Semaphore::new(limits.llm.max(1)),
            limits,
            wake: Notify::new(),
            paused: AtomicBool::new(false),
            events: broadcast::channel(1024).0,
            running: Mutex::new(HashMap::new()),
            today: local_today,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.events.subscribe()
    }

    /// Adds pasted lines. Blank lines are skipped; every other line ends up
    /// in exactly one bucket of the report, and one bad line never stops
    /// the rest.
    pub async fn enqueue<S: AsRef<str>>(&self, inputs: &[S]) -> Result<EnqueueReport> {
        let mut report = EnqueueReport {
            batch_id: Uuid::new_v4(),
            ..Default::default()
        };
        for input in inputs
            .iter()
            .map(|s| s.as_ref().trim())
            .filter(|s| !s.is_empty())
        {
            match self.add_line(report.batch_id, input).await {
                Ok(Added::New(id)) => report.added.push(id),
                Ok(Added::Retried(id)) => report.retried.push(id),
                Ok(Added::AlreadyOpen(id)) => report.already_queued.push(id),
                Ok(Added::AlreadyDone { note_path, .. }) => {
                    report.already_done.push((input.to_string(), note_path))
                }
                Err(e @ (Error::UnsupportedUrl(_) | Error::NotYetSupported(_))) => {
                    report.rejected.push((input.to_string(), e.to_string()))
                }
                Err(e) => report.errors.push((input.to_string(), e.to_string())),
            }
        }
        self.wake.notify_one();
        Ok(report)
    }

    async fn add_line(&self, batch_id: Uuid, input: &str) -> Result<Added> {
        let source = Source::parse(input)?;
        // Expanding channels, playlists and profiles is build step 3.
        let key = source
            .video_key()
            .ok_or(Error::NotYetSupported(source.kind()))?;
        db::add_job(&self.db, batch_id, input, &source.canonical_url(), &key).await
    }

    /// Runs until `shutdown` fires. Jobs in flight at shutdown stay
    /// `running` in the database and resume on the next start. Fails with
    /// [`Error::QueueBusy`] if another process is already working the queue.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        let mut lock = db::lock_dispatcher(&self.db)
            .await?
            .ok_or(Error::QueueBusy)?;
        // Safe now: nobody else can be running these jobs.
        let recovered = db::recover_interrupted(&self.db).await?;
        if recovered > 0 {
            tracing::info!(recovered, "resuming jobs interrupted by the last shutdown");
        }
        // Enough jobs in flight to keep every stage busy, without claiming
        // the whole queue at once.
        let max_in_flight = self.limits.downloads + self.limits.whisper + self.limits.llm + 1;
        let mut tasks = JoinSet::new();
        let mut task_jobs = TaskJobs::new();
        let mut lock_check = tokio::time::interval(Duration::from_secs(30));
        let mut lost: Option<Error> = None;
        loop {
            self.claim_jobs(&mut tasks, &mut task_jobs, max_in_flight)
                .await;
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = self.wake.notified() => {}
                Some(result) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    self.reap(result, &mut task_jobs).await;
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = lock_check.tick() => {
                    if let Err(e) = lock.check().await {
                        tracing::error!(error = %e, "lost the queue lock; stopping");
                        lost = Some(Error::LockLost(e.to_string()));
                        break;
                    }
                }
            }
        }
        tasks.shutdown().await;
        self.running_jobs().clear();
        match lost {
            // In-flight jobs stay `running` and resume on the next start.
            Some(e) => Err(e),
            None => lock.release().await,
        }
    }

    async fn claim_jobs(
        self: &Arc<Self>,
        tasks: &mut JoinSet<()>,
        task_jobs: &mut TaskJobs,
        max: usize,
    ) {
        while !self.is_paused() && tasks.len() < max {
            match db::claim_next(&self.db).await {
                Ok(Some(job)) => {
                    let (id, stage) = (job.id, job.stage);
                    // Hold the lock across spawn + insert: a fast job could
                    // otherwise remove itself before it was ever inserted.
                    let mut running = self.running_jobs();
                    let handle = tasks.spawn(Arc::clone(self).process(job));
                    task_jobs.insert(handle.id(), (id, stage));
                    running.insert(id, handle);
                }
                Ok(None) => break,
                Err(e) => {
                    // A dropped connection shouldn't kill the dispatcher;
                    // the next tick tries again.
                    tracing::error!(error = %e, "could not claim the next job");
                    break;
                }
            }
        }
    }

    /// A job task ended. A panic would otherwise leave the job `running`
    /// with no reason until the next restart.
    async fn reap(
        &self,
        result: std::result::Result<(tokio::task::Id, ()), JoinError>,
        task_jobs: &mut TaskJobs,
    ) {
        let (task_id, panic) = match result {
            Ok((task_id, ())) => (task_id, None),
            // Aborted by `cancel`, which records the cancellation itself.
            Err(e) if e.is_cancelled() => (e.id(), None),
            Err(e) => (e.id(), Some(panic_message(e))),
        };
        let Some((job_id, stage)) = task_jobs.remove(&task_id) else {
            return;
        };
        if let Some(message) = panic {
            tracing::error!(job = job_id, %message, "job task panicked");
            self.running_jobs().remove(&job_id);
            self.record_failure(job_id, stage, &format!("internal error: {message}"))
                .await;
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.wake.notify_one();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub async fn retry(&self, id: i64) -> Result<bool> {
        let retried = db::retry(&self.db, id).await?;
        self.wake.notify_one();
        Ok(retried)
    }

    pub async fn retry_all_failed(&self) -> Result<u64> {
        let n = db::retry_all_failed(&self.db).await?;
        self.wake.notify_one();
        Ok(n)
    }

    /// Re-runs cleanup (and rewrites the note) for videos already
    /// transcribed, from the stored raw transcript (PDA CL-9).
    pub async fn reclean(&self, video_keys: &[String]) -> Result<Vec<(String, Result<Added>)>> {
        let batch_id = Uuid::new_v4();
        let mut results = Vec::with_capacity(video_keys.len());
        for key in video_keys {
            let added = db::add_reclean_job(&self.db, batch_id, key).await;
            results.push((key.clone(), added));
        }
        self.wake.notify_one();
        Ok(results)
    }

    /// Cancels a job. A running one is aborted (its subprocess is killed
    /// with it); if it gets past the abort, it stops at its next stage
    /// transition, since those only apply to running jobs.
    pub async fn cancel(&self, id: i64) -> Result<bool> {
        if let Some(handle) = self.running_jobs().remove(&id) {
            handle.abort();
        }
        let stage = db::cancel(&self.db, id).await?;
        if let Some(stage) = stage {
            self.emit(id, stage, Status::Cancelled, None);
        }
        Ok(stage.is_some())
    }

    pub async fn jobs(&self, limit: i64) -> Result<Vec<Job>> {
        db::list_jobs(&self.db, limit).await
    }

    fn running_jobs(&self) -> std::sync::MutexGuard<'_, HashMap<i64, AbortHandle>> {
        // A panic while holding this lock can't leave the map inconsistent
        // (single inserts/removes), so a poisoned lock is still usable.
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, job_id: i64, stage: Stage, status: Status, reason: Option<String>) {
        // No subscribers (e.g. the CLI) is fine.
        let _ = self.events.send(JobEvent {
            job_id,
            stage,
            status,
            reason,
        });
    }

    async fn process(self: Arc<Self>, job: Job) {
        match self.run_stages(&job).await {
            Ok(()) => {}
            Err(Error::Llm(e)) if e.should_wait() => {
                tracing::info!(job = job.id, reason = %e, "waiting for the LLM");
                self.record_wait(job.id, job.stage, &e.to_string(), e.retry_after())
                    .await;
            }
            Err(e) => {
                tracing::warn!(job = job.id, key = %job.video_key, reason = %e, "job failed");
                self.record_failure(job.id, job.stage, &e.to_string()).await;
            }
        }
        self.running_jobs().remove(&job.id);
        self.wake.notify_one();
    }

    /// Marks a job failed and announces it. If the database can't be
    /// reached the event still goes out, so nothing waits forever; the job
    /// then stays `running` and is recovered on the next start.
    async fn record_failure(&self, id: i64, fallback_stage: Stage, reason: &str) {
        match db::fail(&self.db, id, reason).await {
            Ok(true) => {}
            Ok(false) => {
                // Cancelled (or finished) elsewhere meanwhile; that stands.
                tracing::info!(job = id, %reason, "job errored after being stopped elsewhere");
                return;
            }
            Err(e) => tracing::error!(job = id, error = %e, "could not record the failure"),
        }
        let stage = match db::get_job(&self.db, id).await {
            Ok(Some(job)) => job.stage,
            Ok(None) => fallback_stage,
            Err(e) => {
                tracing::warn!(job = id, error = %e, "could not read the failed job's stage");
                fallback_stage
            }
        };
        self.emit(id, stage, Status::Failed, Some(reason.to_string()));
    }

    /// Parks a job until the LLM is likely back (PDA CL-8, LLM-6).
    async fn record_wait(&self, id: i64, fallback_stage: Stage, reason: &str, retry_in: Duration) {
        match db::wait(&self.db, id, reason, retry_in).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => tracing::error!(job = id, error = %e, "could not park the job"),
        }
        let stage = db::get_job(&self.db, id)
            .await
            .ok()
            .flatten()
            .map_or(fallback_stage, |j| j.stage);
        self.emit(id, stage, Status::Waiting, Some(reason.to_string()));
    }

    async fn run_stages(&self, job: &Job) -> Result<()> {
        let job_dir = self.work_dir.join(job_dir_name(&job.video_key));
        let mut stage = job.stage;
        let mut caption_error = job.caption_error.clone();
        let mut folder = job.folder.clone();
        loop {
            self.emit(job.id, stage, Status::Running, None);
            let next = match stage {
                Stage::Metadata => self.stage_metadata(job).await?,
                Stage::Fetch => self.stage_fetch(job, &job_dir, &mut caption_error).await?,
                Stage::Transcribe => {
                    self.stage_transcribe(job, &job_dir, caption_error.as_deref())
                        .await?
                }
                Stage::Clean => self.stage_clean(job).await?,
                Stage::File => self.stage_file(job, &mut folder).await?,
                Stage::Write => return self.stage_write(job, &job_dir, folder.as_deref()).await,
                Stage::Done => return Ok(()),
            };
            if !db::set_stage(&self.db, job.id, next).await? {
                tracing::info!(job = job.id, "job was stopped elsewhere; not continuing");
                return Ok(());
            }
            stage = next;
        }
    }

    async fn stage_metadata(&self, job: &Job) -> Result<Stage> {
        let meta = {
            let _slot = self.downloads.acquire().await.map_err(stopped)?;
            self.engine.metadata(&job.url).await?
        };
        db::upsert_video(&self.db, &meta).await?;
        Ok(Stage::Fetch)
    }

    async fn stage_fetch(
        &self,
        job: &Job,
        job_dir: &Path,
        caption_error: &mut Option<String>,
    ) -> Result<Stage> {
        if self.has_transcript(job).await? {
            return Ok(Stage::Clean);
        }
        let meta = self.video(job).await?;
        tokio::fs::create_dir_all(job_dir)
            .await
            .map_err(fs_err("create folder", job_dir))?;
        let fetched = {
            let _slot = self.downloads.acquire().await.map_err(stopped)?;
            self.engine.fetch(&meta, &job.url, job_dir).await?
        };
        match fetched {
            Fetched::Captions(transcript) => {
                self.store_transcript(job, &transcript, None).await?;
                Ok(Stage::Clean)
            }
            Fetched::Audio {
                caption_error: why, ..
            } => {
                db::set_caption_error(&self.db, job.id, why.as_deref()).await?;
                *caption_error = why;
                Ok(Stage::Transcribe)
            }
        }
    }

    async fn stage_transcribe(
        &self,
        job: &Job,
        job_dir: &Path,
        caption_error: Option<&str>,
    ) -> Result<Stage> {
        if self.has_transcript(job).await? {
            return Ok(Stage::Clean);
        }
        let audio = job_dir.join(AUDIO_FILE);
        if !audio.is_file() {
            // The audio went missing (cleared cache): fetch again.
            return Ok(Stage::Fetch);
        }
        let meta = self.video(job).await?;
        // The Whisper slot covers Whisper only, not the bookkeeping after it.
        let transcript = {
            let _slot = self.whisper.acquire().await.map_err(stopped)?;
            self.engine.transcribe_audio(&meta, &audio).await?
        };
        self.store_transcript(job, &transcript, caption_error)
            .await?;
        remove_job_dir(job_dir).await;
        Ok(Stage::Clean)
    }

    /// Fluff removal and translation (PDA §4.3). Every run is stored.
    async fn stage_clean(&self, job: &Job) -> Result<Stage> {
        let transcript = self.transcript(job).await?;
        let cleaned = {
            let _slot = self.llm_slots.acquire().await.map_err(stopped)?;
            cleanup::clean(&self.llm, &transcript, &self.cleanup).await?
        };
        db::insert_cleaning(&self.db, &job.video_key, &cleaned).await?;
        Ok(Stage::File)
    }

    /// Picks the note's folder (PDA §4.6). A video that already has a note
    /// (a re-clean) keeps it where it is, even if it was moved (FO-9).
    async fn stage_file(&self, job: &Job, folder: &mut Option<String>) -> Result<Stage> {
        if folder.is_some() || !db::notes_for(&self.db, &job.video_key).await?.is_empty() {
            return Ok(Stage::Write);
        }
        let meta = self.video(job).await?;
        let transcript = self.transcript(job).await?;
        let cleaned = self.cleaning(job).await?;
        let tree = FolderTree::scan(&self.out_dir)?;
        let title = note_title(&meta, &transcript, Some(&cleaned));
        let placement = {
            let _slot = self.llm_slots.acquire().await.map_err(stopped)?;
            folders::sort(&self.llm, &tree, &title, &cleaned.text).await?
        };
        // Create it now, with the sorter's description (FO-6): only the
        // path is stored on the job, and the next note may need it.
        folders::place(&self.out_dir, &placement)?;
        let chosen = placement.relative_path();
        db::set_folder(&self.db, job.id, &chosen).await?;
        *folder = Some(chosen);
        Ok(Stage::Write)
    }

    async fn stage_write(&self, job: &Job, job_dir: &Path, folder: Option<&str>) -> Result<()> {
        let meta = self.video(job).await?;
        let transcript = self.transcript(job).await?;
        let cleaned = self.cleaning(job).await?;
        let body = render_note(&meta, &transcript, Some(&cleaned), &(self.today)());
        let path = match self.update_existing_notes(job, &body).await? {
            Some(path) => path,
            None => {
                self.write_new_note(job, &meta, &transcript, &cleaned, folder)
                    .await?
            }
        };
        let shown = path.display().to_string();
        if !db::finish(&self.db, job.id, &shown, cleaned.review).await? {
            tracing::warn!(
                job = job.id,
                path = %shown,
                "job was cancelled while its note was being written; the note is kept"
            );
            return Ok(());
        }
        remove_job_dir(job_dir).await;
        self.emit(job.id, Stage::Done, Status::Done, None);
        Ok(())
    }

    /// Rewrites every note for this video that is still exactly what Shruti
    /// wrote, wherever it is now (FO-9, FO-10). Edited notes are left alone
    /// (OUT-6). Returns the first rewritten note, if any.
    async fn update_existing_notes(&self, job: &Job, body: &str) -> Result<Option<PathBuf>> {
        let records = db::notes_for(&self.db, &job.video_key).await?;
        if records.is_empty() {
            return Ok(None);
        }
        let hashes: Vec<String> = records.iter().map(|r| r.content_sha256.clone()).collect();
        let mut paths: Vec<PathBuf> = records
            .iter()
            .map(|r| PathBuf::from(&r.path))
            .filter(|p| p.is_file())
            .collect();
        let mut moved_from = None;
        if paths.is_empty() {
            // Moved in Obsidian: find it by the id in its frontmatter.
            paths = find_notes_by_id(&self.out_dir, &job.video_key)?;
            moved_from = records.first().map(|r| r.path.clone());
        }
        let sha = sha256_hex(body);
        let mut first = None;
        for path in paths {
            if update_note(&path, &hashes, body).await? {
                let shown = path.display().to_string();
                db::record_note(
                    &self.db,
                    &job.video_key,
                    &shown,
                    &sha,
                    moved_from.as_deref(),
                )
                .await?;
                first.get_or_insert(path);
            }
        }
        Ok(first)
    }

    async fn write_new_note(
        &self,
        job: &Job,
        meta: &VideoMeta,
        transcript: &Transcript,
        cleaned: &cleanup::Cleaned,
        folder: Option<&str>,
    ) -> Result<PathBuf> {
        let dir = match folder {
            Some(f) => {
                let placement = folders::from_relative_path(&FolderTree::scan(&self.out_dir)?, f)?;
                folders::place(&self.out_dir, &placement)?
            }
            // A re-clean whose earlier note was edited: the new copy goes
            // next to it (OUT-6), else to the inbox.
            None => match self.existing_note_dir(job).await? {
                Some(dir) => dir,
                None => folders::place(&self.out_dir, &Placement::Inbox)?,
            },
        };
        let path = write_note(&dir, meta, transcript, Some(cleaned), &(self.today)()).await?;
        // Fingerprint what's actually on disk: an identical earlier note
        // may have been reused rather than a new one written.
        let written = tokio::fs::read_to_string(&path)
            .await
            .map_err(fs_err("read", &path))?;
        let shown = path.display().to_string();
        db::record_note(
            &self.db,
            &job.video_key,
            &shown,
            &sha256_hex(&written),
            None,
        )
        .await?;
        Ok(path)
    }

    /// The folder of this video's current note, wherever it was moved.
    async fn existing_note_dir(&self, job: &Job) -> Result<Option<PathBuf>> {
        let recorded = db::notes_for(&self.db, &job.video_key)
            .await?
            .into_iter()
            .map(|r| PathBuf::from(r.path))
            .find(|p| p.is_file());
        let path = match recorded {
            Some(p) => Some(p),
            None => find_notes_by_id(&self.out_dir, &job.video_key)?
                .into_iter()
                .next(),
        };
        Ok(path.and_then(|p| p.parent().map(Path::to_path_buf)))
    }

    async fn transcript(&self, job: &Job) -> Result<Transcript> {
        db::load_transcript(&self.db, &job.video_key)
            .await?
            .ok_or_else(|| Error::State(format!("no stored transcript for {}", job.video_key)))
    }

    async fn cleaning(&self, job: &Job) -> Result<cleanup::Cleaned> {
        db::latest_cleaning(&self.db, &job.video_key)
            .await?
            .ok_or_else(|| Error::State(format!("no cleaned text for {}", job.video_key)))
    }

    async fn store_transcript(
        &self,
        job: &Job,
        transcript: &Transcript,
        caption_error: Option<&str>,
    ) -> Result<()> {
        let stored =
            db::insert_transcript(&self.db, &job.video_key, transcript, caption_error).await?;
        if !stored {
            // Raw transcripts are never overwritten (DB-5); the earlier one
            // is the one this job continues with.
            tracing::info!(
                job = job.id,
                key = %job.video_key,
                "a transcript was already stored; keeping it and discarding this run's"
            );
        }
        Ok(())
    }

    async fn video(&self, job: &Job) -> Result<VideoMeta> {
        db::load_video(&self.db, &job.video_key)
            .await?
            .ok_or_else(|| Error::State(format!("no stored metadata for {}", job.video_key)))
    }

    async fn has_transcript(&self, job: &Job) -> Result<bool> {
        Ok(db::load_transcript(&self.db, &job.video_key)
            .await?
            .is_some())
    }
}

/// The semaphores are never closed while a queue exists, so this only
/// fires if that changes.
fn stopped(_: tokio::sync::AcquireError) -> Error {
    Error::Stopped
}

fn panic_message(e: JoinError) -> String {
    let payload = e.into_panic();
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "the job crashed".to_string())
}

/// `youtube:abc` → `youtube_abc`: a safe folder name for any key.
fn job_dir_name(video_key: &str) -> String {
    video_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn remove_job_dir(dir: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), error = %e, "could not remove a job's audio folder");
    }
}

#[cfg(test)]
mod tests;
