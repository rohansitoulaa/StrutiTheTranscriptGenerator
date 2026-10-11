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

use crate::db::{self, Added, Job, Stage, Status};
use crate::error::fs_err;
use crate::metadata::VideoMeta;
use crate::output::write_note;
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
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            downloads: 3,
            whisper: 1,
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

pub struct Queue<F, T> {
    db: PgPool,
    engine: Engine<F, T>,
    /// Per-job audio lives here between the fetch and transcribe stages, so
    /// a restart doesn't re-download it.
    work_dir: PathBuf,
    out_dir: PathBuf,
    limits: Limits,
    downloads: Semaphore,
    whisper: Semaphore,
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

impl<F, T> Queue<F, T>
where
    F: Fetcher + Send + Sync + 'static,
    T: Transcriber + Send + Sync + 'static,
{
    pub fn new(
        db: PgPool,
        engine: Engine<F, T>,
        work_dir: PathBuf,
        out_dir: PathBuf,
        limits: Limits,
    ) -> Arc<Self> {
        Arc::new(Queue {
            db,
            engine,
            work_dir,
            out_dir,
            downloads: Semaphore::new(limits.downloads.max(1)),
            whisper: Semaphore::new(limits.whisper.max(1)),
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
        let max_in_flight = self.limits.downloads + self.limits.whisper + 1;
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
        if let Err(e) = self.run_stages(&job).await {
            tracing::warn!(job = job.id, key = %job.video_key, reason = %e, "job failed");
            self.record_failure(job.id, job.stage, &e.to_string()).await;
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

    async fn run_stages(&self, job: &Job) -> Result<()> {
        let job_dir = self.work_dir.join(job_dir_name(&job.video_key));
        let mut stage = job.stage;
        let mut caption_error = job.caption_error.clone();
        loop {
            self.emit(job.id, stage, Status::Running, None);
            let next = match stage {
                Stage::Metadata => self.stage_metadata(job).await?,
                Stage::Fetch => self.stage_fetch(job, &job_dir, &mut caption_error).await?,
                Stage::Transcribe => {
                    self.stage_transcribe(job, &job_dir, caption_error.as_deref())
                        .await?
                }
                // LLM cleanup and folder sorting arrive in build step 2b.
                Stage::Clean => Stage::File,
                Stage::File => Stage::Write,
                Stage::Write => return self.stage_write(job, &job_dir).await,
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

    async fn stage_write(&self, job: &Job, job_dir: &Path) -> Result<()> {
        let meta = self.video(job).await?;
        let transcript = db::load_transcript(&self.db, &job.video_key)
            .await?
            .ok_or_else(|| Error::State(format!("no stored transcript for {}", job.video_key)))?;
        let path = write_note(&self.out_dir, &meta, &transcript, &(self.today)()).await?;
        if !db::finish(&self.db, job.id, &path.display().to_string()).await? {
            tracing::warn!(
                job = job.id,
                path = %path.display(),
                "job was cancelled while its note was being written; the note is kept"
            );
            return Ok(());
        }
        remove_job_dir(job_dir).await;
        self.emit(job.id, Stage::Done, Status::Done, None);
        Ok(())
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
