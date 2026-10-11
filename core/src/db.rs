//! Postgres: videos, raw transcripts and the job queue (PDA §4.4, §4.7).
//!
//! Plain `sqlx::query` calls rather than the compile-time-checked macros, so
//! building the crate never needs a live database.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;

use crate::cleanup::Cleaned;
use crate::metadata::{CaptionKind, Platform, VideoMeta};
use crate::transcript::{Segment, Transcript, TranscriptSource};
use crate::{Error, Result};

/// Connects and brings the schema up to date (PDA DB-1).
pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        // A hung server should fail jobs with a reason, not stall the queue.
        .acquire_timeout(std::time::Duration::from_secs(15))
        .connect(url)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// Advisory-lock key for "this process is working the queue" ("SHRUTI").
const DISPATCHER_LOCK_KEY: i64 = 0x5348_5255_5449;

/// Proof that this process is the only one working the queue. Two
/// dispatchers would each treat the other's in-flight jobs as interrupted.
/// The lock lives on its own connection, so it's released when this is
/// dropped or the process dies.
pub struct DispatcherLock(PgConnection);

impl DispatcherLock {
    /// Confirms the connection holding the lock is still up. If it dropped,
    /// Postgres released the lock and another process could take over.
    pub async fn check(&mut self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&mut self.0).await?;
        Ok(())
    }

    pub async fn release(self) -> Result<()> {
        self.0.close().await?;
        Ok(())
    }
}

/// `None` if another process already holds the lock.
pub async fn lock_dispatcher(pool: &PgPool) -> Result<Option<DispatcherLock>> {
    let mut conn = pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(DISPATCHER_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if locked {
        Ok(Some(DispatcherLock(conn)))
    } else {
        conn.close().await?;
        Ok(None)
    }
}

/// The next stage a job will run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Metadata,
    Fetch,
    Transcribe,
    Clean,
    File,
    Write,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Waiting,
    Failed,
    Done,
    Cancelled,
}

macro_rules! text_enum {
    ($ty:ty { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            fn parse(text: &str) -> Result<Self> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    other => Err(Error::Parse {
                        what: stringify!($ty),
                        reason: format!("unknown value {other:?} in the database"),
                    }),
                }
            }
        }
    };
}

text_enum!(Stage {
    Metadata => "metadata", Fetch => "fetch", Transcribe => "transcribe", Clean => "clean",
    File => "file", Write => "write", Done => "done",
});
text_enum!(Status {
    Queued => "queued", Running => "running", Waiting => "waiting", Failed => "failed",
    Done => "done", Cancelled => "cancelled",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Job {
    pub id: i64,
    pub batch_id: Uuid,
    pub input: String,
    pub url: String,
    pub video_key: String,
    pub stage: Stage,
    pub status: Status,
    pub reason: Option<String>,
    pub caption_error: Option<String>,
    pub attempts: i32,
    pub review: bool,
    pub note_path: Option<String>,
    /// Where the file stage placed the note: `Area/Topic` or `Inbox`.
    pub folder: Option<String>,
    /// From `videos`, once metadata has been fetched.
    pub title: Option<String>,
}

const JOB_COLUMNS: &str = "j.id, j.batch_id, j.input, j.url, j.video_key, j.stage, j.status, j.reason, \
     j.caption_error, j.attempts, j.review, j.note_path, j.folder, v.title";

fn job_from_row(row: &PgRow) -> Result<Job> {
    Ok(Job {
        id: row.try_get("id")?,
        batch_id: row.try_get("batch_id")?,
        input: row.try_get("input")?,
        url: row.try_get("url")?,
        video_key: row.try_get("video_key")?,
        stage: Stage::parse(row.try_get("stage")?)?,
        status: Status::parse(row.try_get("status")?)?,
        reason: row.try_get("reason")?,
        caption_error: row.try_get("caption_error")?,
        attempts: row.try_get("attempts")?,
        review: row.try_get("review")?,
        note_path: row.try_get("note_path")?,
        folder: row.try_get("folder")?,
        title: row.try_get("title")?,
    })
}

/// What adding a link to the queue did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    New(i64),
    /// Already queued, running or waiting.
    AlreadyOpen(i64),
    /// It had failed; re-pasting it retries it (resuming at the failed stage).
    Retried(i64),
    /// Finished before; nothing to do (PDA FO-10).
    AlreadyDone {
        job_id: i64,
        note_path: Option<String>,
    },
}

pub async fn add_job(
    pool: &PgPool,
    batch_id: Uuid,
    input: &str,
    url: &str,
    video_key: &str,
) -> Result<Added> {
    // Each pass reads, inserts, then reads again; a job changing state in
    // between (finished, cancelled) just means another pass.
    for _ in 0..3 {
        if let Some(row) = sqlx::query(
            "SELECT id, note_path FROM jobs WHERE video_key = $1 AND status = 'done'
             ORDER BY id DESC LIMIT 1",
        )
        .bind(video_key)
        .fetch_optional(pool)
        .await?
        {
            return Ok(Added::AlreadyDone {
                job_id: row.try_get("id")?,
                note_path: row.try_get("note_path")?,
            });
        }

        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO jobs (batch_id, input, url, video_key) VALUES ($1, $2, $3, $4)
             ON CONFLICT (video_key) WHERE status IN ('queued', 'running', 'waiting', 'failed') DO NOTHING
             RETURNING id",
        )
        .bind(batch_id)
        .bind(input)
        .bind(url)
        .bind(video_key)
        .fetch_optional(pool)
        .await?;
        if let Some(id) = inserted {
            return Ok(Added::New(id));
        }

        let open = sqlx::query(
            "SELECT id, status FROM jobs
             WHERE video_key = $1 AND status IN ('queued', 'running', 'waiting', 'failed')",
        )
        .bind(video_key)
        .fetch_optional(pool)
        .await?;
        if let Some(open) = open {
            let id: i64 = open.try_get("id")?;
            if Status::parse(open.try_get("status")?)? == Status::Failed && retry(pool, id).await? {
                return Ok(Added::Retried(id));
            }
            return Ok(Added::AlreadyOpen(id));
        }
    }
    Err(Error::State(format!(
        "{video_key} kept changing state while being queued; paste it again"
    )))
}

/// Takes the oldest queued job (or waiting job whose retry time has come)
/// and marks it running. `SKIP LOCKED` keeps
/// two dispatchers from ever taking the same job.
pub async fn claim_next(pool: &PgPool) -> Result<Option<Job>> {
    // Two queries rather than one `OR`, so each can use its partial index
    // (`jobs_unfinished`, `jobs_waiting_due`); new work goes first.
    let mut claimed: Option<i64> = sqlx::query_scalar(
        "UPDATE jobs SET status = 'running', updated_at = now()
         WHERE id = (SELECT id FROM jobs WHERE status = 'queued'
                     ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1)
         RETURNING id",
    )
    .fetch_optional(pool)
    .await?;
    if claimed.is_none() {
        // The wait's reason is stale once it runs again.
        claimed = sqlx::query_scalar(
            "UPDATE jobs SET status = 'running', reason = NULL, retry_at = NULL, updated_at = now()
             WHERE id = (SELECT id FROM jobs WHERE status = 'waiting' AND retry_at <= now()
                         ORDER BY retry_at, id FOR UPDATE SKIP LOCKED LIMIT 1)
             RETURNING id",
        )
        .fetch_optional(pool)
        .await?;
    }
    match claimed {
        Some(id) => get_job(pool, id).await,
        None => Ok(None),
    }
}

/// Jobs left `running` by a crash or quit go back in the queue; their
/// `stage` says where to resume (PDA Q-5).
pub async fn recover_interrupted(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE jobs SET status = 'queued', updated_at = now() WHERE status = 'running'",
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// The job-advancing writes below only apply while the job is `running`,
/// and say whether they did: `false` means it was cancelled (or finished)
/// elsewhere meanwhile, and the caller should stop working on it.
pub async fn set_stage(pool: &PgPool, id: i64, stage: Stage) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET stage = $2, updated_at = now() WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(stage.as_str())
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn set_caption_error(
    pool: &PgPool,
    id: i64,
    caption_error: Option<&str>,
) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET caption_error = $2, updated_at = now() WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(caption_error)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Marks a running job failed, keeping its stage so a retry resumes there.
pub async fn fail(pool: &PgPool, id: i64, reason: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET status = 'failed', reason = $2, attempts = attempts + 1, updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// `review` marks a note the cleanup guardrail flagged (PDA CL-6).
pub async fn finish(pool: &PgPool, id: i64, note_path: &str, review: bool) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET stage = 'done', status = 'done', reason = NULL, note_path = $2,
                review = $3, retry_at = NULL, finished_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(note_path)
    .bind(review)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Puts a failed or waiting job back in the queue now. Returns whether it
/// was one.
pub async fn retry(pool: &PgPool, id: i64) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET status = 'queued', reason = NULL, retry_at = NULL, updated_at = now()
         WHERE id = $1 AND status IN ('failed', 'waiting')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn retry_all_failed(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query("UPDATE jobs SET status = 'queued', reason = NULL, updated_at = now() WHERE status = 'failed'")
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

/// Parks a running job until `retry_in` has passed: the LLM is down,
/// rate limited or out of quota (PDA CL-8, LLM-6). Its stage is kept.
pub async fn wait(
    pool: &PgPool,
    id: i64,
    reason: &str,
    retry_in: std::time::Duration,
) -> Result<bool> {
    let seconds = retry_in.as_secs_f64();
    let done = sqlx::query(
        "UPDATE jobs SET status = 'waiting', reason = $2,
                retry_at = now() + make_interval(secs => $3), updated_at = now()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(reason)
    .bind(seconds)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Parks every queued job of one platform (`instagram`), e.g. when it
/// starts blocking requests (PDA IG-5). Returns each parked job and stage.
pub async fn wait_queued_platform(
    pool: &PgPool,
    platform: &str,
    reason: &str,
    retry_in: std::time::Duration,
) -> Result<Vec<(i64, Stage)>> {
    let rows = sqlx::query(
        "UPDATE jobs SET status = 'waiting', reason = $2,
                retry_at = now() + make_interval(secs => $3), updated_at = now()
         WHERE status = 'queued' AND starts_with(video_key, $1 || ':')
         RETURNING id, stage",
    )
    .bind(platform)
    .bind(reason)
    .bind(retry_in.as_secs_f64())
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("id")?, Stage::parse(r.try_get("stage")?)?)))
        .collect()
}

/// Which of these videos are already done or on their way (queued,
/// running, waiting). Failed and cancelled ones would be worked on again.
pub async fn known_keys(
    pool: &PgPool,
    keys: &[String],
) -> Result<std::collections::HashSet<String>> {
    let known: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT video_key FROM jobs
         WHERE video_key = ANY($1) AND status IN ('done', 'queued', 'running', 'waiting')",
    )
    .bind(keys)
    .fetch_all(pool)
    .await?;
    Ok(known.into_iter().collect())
}

pub async fn set_folder(pool: &PgPool, id: i64, folder: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE jobs SET folder = $2, updated_at = now() WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(folder)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Cancels a job that hasn't finished. Returns the stage it was at, or
/// `None` if it had already finished or been cancelled.
pub async fn cancel(pool: &PgPool, id: i64) -> Result<Option<Stage>> {
    let stage: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET status = 'cancelled', updated_at = now()
         WHERE id = $1 AND status IN ('queued', 'running', 'waiting', 'failed')
         RETURNING stage",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    stage.as_deref().map(Stage::parse).transpose()
}

/// A job that re-runs cleanup (and the note) from the stored raw
/// transcript, without downloading or transcribing again (PDA CL-9).
pub async fn add_reclean_job(pool: &PgPool, batch_id: Uuid, video_key: &str) -> Result<Added> {
    let url: Option<String> = sqlx::query_scalar(
        "SELECT v.url FROM videos v JOIN transcripts t ON t.video_key = v.key WHERE v.key = $1",
    )
    .bind(video_key)
    .fetch_optional(pool)
    .await?;
    let url = url
        .ok_or_else(|| Error::State(format!("{video_key} has no stored transcript to re-clean")))?;
    // The open job blocking the insert can finish before it's read back;
    // that just means another pass, as in `add_job`.
    for _ in 0..3 {
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO jobs (batch_id, input, url, video_key, stage) VALUES ($1, $2, $3, $2, 'clean')
             ON CONFLICT (video_key) WHERE status IN ('queued', 'running', 'waiting', 'failed') DO NOTHING
             RETURNING id",
        )
        .bind(batch_id)
        .bind(video_key)
        .bind(&url)
        .fetch_optional(pool)
        .await?;
        if let Some(id) = inserted {
            return Ok(Added::New(id));
        }
        let open: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE video_key = $1
             AND status IN ('queued', 'running', 'waiting', 'failed')",
        )
        .bind(video_key)
        .fetch_optional(pool)
        .await?;
        if let Some(id) = open {
            return Ok(Added::AlreadyOpen(id));
        }
    }
    Err(Error::State(format!(
        "{video_key} kept changing state while being queued; try again"
    )))
}

pub async fn get_job(pool: &PgPool, id: i64) -> Result<Option<Job>> {
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM jobs j LEFT JOIN videos v ON v.key = j.video_key WHERE j.id = $1"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(job_from_row).transpose()
}

/// Newest first.
pub async fn list_jobs(pool: &PgPool, limit: i64) -> Result<Vec<Job>> {
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM jobs j LEFT JOIN videos v ON v.key = j.video_key ORDER BY j.id DESC LIMIT $1"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(job_from_row).collect()
}

/// Metadata can change (titles get edited), so it's refreshed on conflict.
pub async fn upsert_video(pool: &PgPool, meta: &VideoMeta) -> Result<()> {
    let upload_date = meta
        .upload_date
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let duration = meta.duration_secs.and_then(|d| {
        let fits = i32::try_from(d).ok();
        if fits.is_none() {
            tracing::warn!(key = %meta.key(), duration = d, "implausible duration, not storing it");
        }
        fits
    });
    sqlx::query(
        "INSERT INTO videos (key, platform, video_id, url, title, channel, upload_date, duration_secs,
                             language, subtitles, auto_captions)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (key) DO UPDATE SET
             url = EXCLUDED.url, title = EXCLUDED.title, channel = EXCLUDED.channel,
             upload_date = EXCLUDED.upload_date, duration_secs = EXCLUDED.duration_secs,
             language = EXCLUDED.language, subtitles = EXCLUDED.subtitles,
             auto_captions = EXCLUDED.auto_captions, updated_at = now()",
    )
    .bind(meta.key())
    .bind(meta.platform.as_str())
    .bind(&meta.id)
    .bind(&meta.url)
    .bind(&meta.title)
    .bind(&meta.channel)
    .bind(upload_date)
    .bind(duration)
    .bind(&meta.language)
    .bind(&meta.subtitles)
    .bind(&meta.auto_captions)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_video(pool: &PgPool, key: &str) -> Result<Option<VideoMeta>> {
    let row = sqlx::query(
        "SELECT platform, video_id, url, title, channel, upload_date, duration_secs, language,
                subtitles, auto_captions
         FROM videos WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let platform = match row.try_get::<&str, _>("platform")? {
        "youtube" => Platform::YouTube,
        "instagram" => Platform::Instagram,
        other => {
            return Err(Error::Parse {
                what: "platform",
                reason: format!("unknown value {other:?}"),
            });
        }
    };
    Ok(Some(VideoMeta {
        platform,
        id: row.try_get("video_id")?,
        url: row.try_get("url")?,
        title: row.try_get("title")?,
        channel: row.try_get("channel")?,
        upload_date: row
            .try_get::<Option<NaiveDate>, _>("upload_date")?
            .map(|d| d.format("%Y-%m-%d").to_string()),
        duration_secs: row
            .try_get::<Option<i32>, _>("duration_secs")?
            .map(|d| d.max(0) as u64),
        language: row.try_get("language")?,
        subtitles: row.try_get("subtitles")?,
        auto_captions: row.try_get("auto_captions")?,
    }))
}

/// Stores the raw transcript once. A second insert for the same video is
/// ignored, never an overwrite (PDA DB-5). Returns whether it was stored.
pub async fn insert_transcript(
    pool: &PgPool,
    video_key: &str,
    transcript: &Transcript,
    caption_error: Option<&str>,
) -> Result<bool> {
    let (source, kind) = match transcript.source {
        TranscriptSource::Captions(CaptionKind::Manual) => ("captions", Some("manual")),
        TranscriptSource::Captions(CaptionKind::Auto) => ("captions", Some("auto")),
        TranscriptSource::Whisper => ("whisper", None),
    };
    let segments = serde_json::to_value(&transcript.segments).map_err(|e| Error::Parse {
        what: "segments",
        reason: e.to_string(),
    })?;
    let done = sqlx::query(
        "INSERT INTO transcripts (video_key, source, caption_kind, language, raw_text, segments, caption_error)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (video_key) DO NOTHING",
    )
    .bind(video_key)
    .bind(source)
    .bind(kind)
    .bind(&transcript.language)
    .bind(transcript.text())
    .bind(segments)
    .bind(caption_error)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn load_transcript(pool: &PgPool, video_key: &str) -> Result<Option<Transcript>> {
    let row = sqlx::query(
        "SELECT source, caption_kind, language, segments FROM transcripts WHERE video_key = $1",
    )
    .bind(video_key)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let source = match (
        row.try_get::<&str, _>("source")?,
        row.try_get::<Option<&str>, _>("caption_kind")?,
    ) {
        ("captions", Some("manual")) => TranscriptSource::Captions(CaptionKind::Manual),
        ("captions", Some("auto")) => TranscriptSource::Captions(CaptionKind::Auto),
        ("whisper", None) => TranscriptSource::Whisper,
        (s, k) => {
            return Err(Error::Parse {
                what: "transcript source",
                reason: format!("{s:?}/{k:?}"),
            });
        }
    };
    let segments: Vec<Segment> =
        serde_json::from_value(row.try_get("segments")?).map_err(|e| Error::Parse {
            what: "segments",
            reason: e.to_string(),
        })?;
    Ok(Some(Transcript {
        source,
        language: row.try_get("language")?,
        segments,
    }))
}

/// Stores a cleanup run; earlier runs stay (DB-5).
pub async fn insert_cleaning(pool: &PgPool, video_key: &str, cleaned: &Cleaned) -> Result<i64> {
    let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    let tokens = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let id = sqlx::query_scalar(
        "INSERT INTO cleanings (video_key, model, cleaned_text, translated, review, raw_words,
                                clean_words, tokens_in, tokens_out)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(video_key)
    .bind(&cleaned.model)
    .bind(&cleaned.text)
    .bind(cleaned.translated)
    .bind(cleaned.review)
    .bind(count(cleaned.raw_words))
    .bind(count(cleaned.clean_words))
    .bind(tokens(cleaned.tokens_in))
    .bind(tokens(cleaned.tokens_out))
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// The newest cleanup run for a video.
pub async fn latest_cleaning(pool: &PgPool, video_key: &str) -> Result<Option<Cleaned>> {
    let row = sqlx::query(
        "SELECT model, cleaned_text, translated, review, raw_words, clean_words, tokens_in, tokens_out
         FROM cleanings WHERE video_key = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(video_key)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(Cleaned {
        model: row.try_get("model")?,
        text: row.try_get("cleaned_text")?,
        translated: row.try_get("translated")?,
        review: row.try_get("review")?,
        raw_words: row.try_get::<i32, _>("raw_words")?.max(0) as usize,
        clean_words: row.try_get::<i32, _>("clean_words")?.max(0) as usize,
        tokens_in: row.try_get::<i64, _>("tokens_in")?.max(0) as u64,
        tokens_out: row.try_get::<i64, _>("tokens_out")?.max(0) as u64,
    }))
}

/// A note Shruti wrote and the fingerprint of what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRecord {
    pub path: String,
    pub content_sha256: String,
}

pub async fn notes_for(pool: &PgPool, video_key: &str) -> Result<Vec<NoteRecord>> {
    let rows = sqlx::query(
        "SELECT path, content_sha256 FROM notes WHERE video_key = $1 ORDER BY written_at",
    )
    .bind(video_key)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(NoteRecord {
                path: r.try_get("path")?,
                content_sha256: r.try_get("content_sha256")?,
            })
        })
        .collect()
}

/// Records (or refreshes) what Shruti wrote at `path`. `moved_from` drops
/// the record of the same note at its old location.
pub async fn record_note(
    pool: &PgPool,
    video_key: &str,
    path: &str,
    content_sha256: &str,
    moved_from: Option<&str>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    if let Some(old) = moved_from.filter(|old| *old != path) {
        sqlx::query("DELETE FROM notes WHERE video_key = $1 AND path = $2")
            .bind(video_key)
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO notes (video_key, path, content_sha256) VALUES ($1, $2, $3)
         ON CONFLICT (video_key, path) DO UPDATE SET content_sha256 = EXCLUDED.content_sha256, written_at = now()",
    )
    .bind(video_key)
    .bind(path)
    .bind(content_sha256)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn meta(id: &str) -> VideoMeta {
        VideoMeta {
            platform: Platform::YouTube,
            id: id.into(),
            url: format!("https://www.youtube.com/watch?v={id}"),
            title: "Me at the zoo".into(),
            channel: Some("jawed".into()),
            upload_date: Some("2005-04-24".into()),
            duration_secs: Some(19),
            language: Some("en".into()),
            subtitles: vec!["en".into()],
            auto_captions: vec!["en-orig".into()],
        }
    }

    fn transcript(text: &str) -> Transcript {
        Transcript {
            source: TranscriptSource::Captions(CaptionKind::Auto),
            language: Some("hi".into()),
            segments: vec![Segment {
                start_ms: 160,
                end_ms: 4480,
                text: text.into(),
            }],
        }
    }

    async fn add(pool: &PgPool, key: &str) -> Added {
        add_job(pool, Uuid::new_v4(), key, &format!("https://x/{key}"), key)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn videos_round_trip(pool: PgPool) {
        upsert_video(&pool, &meta("jNQXAC9IVRw")).await.unwrap();
        assert_eq!(
            load_video(&pool, "youtube:jNQXAC9IVRw").await.unwrap(),
            Some(meta("jNQXAC9IVRw"))
        );
        let edited = VideoMeta {
            title: "Renamed".into(),
            ..meta("jNQXAC9IVRw")
        };
        upsert_video(&pool, &edited).await.unwrap();
        assert_eq!(
            load_video(&pool, "youtube:jNQXAC9IVRw")
                .await
                .unwrap()
                .unwrap()
                .title,
            "Renamed"
        );
        assert_eq!(load_video(&pool, "youtube:nope").await.unwrap(), None);
    }

    #[sqlx::test]
    async fn raw_transcripts_are_never_overwritten(pool: PgPool) {
        upsert_video(&pool, &meta("a")).await.unwrap();
        assert!(
            insert_transcript(&pool, "youtube:a", &transcript("पहला"), None)
                .await
                .unwrap()
        );
        assert!(
            !insert_transcript(&pool, "youtube:a", &transcript("दूसरा"), Some("x"))
                .await
                .unwrap()
        );
        assert_eq!(
            load_transcript(&pool, "youtube:a").await.unwrap(),
            Some(transcript("पहला"))
        );
    }

    #[sqlx::test]
    async fn adding_dedupes_open_jobs_and_retries_failed_ones(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        assert_eq!(add(&pool, "youtube:a").await, Added::AlreadyOpen(id));

        let job = claim_next(&pool).await.unwrap().unwrap();
        fail(&pool, job.id, "HTTP Error 403").await.unwrap();
        assert_eq!(add(&pool, "youtube:a").await, Added::Retried(id));
        let job = get_job(&pool, id).await.unwrap().unwrap();
        assert_eq!(
            (job.status, job.reason, job.attempts),
            (Status::Queued, None, 1)
        );
    }

    #[sqlx::test]
    async fn finished_videos_are_reported_not_requeued(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        finish(&pool, id, "/vault/a.md", false).await.unwrap();
        assert_eq!(
            add(&pool, "youtube:a").await,
            Added::AlreadyDone {
                job_id: id,
                note_path: Some("/vault/a.md".into())
            }
        );
    }

    #[sqlx::test]
    async fn claiming_is_oldest_first_and_exclusive(pool: PgPool) {
        let Added::New(a) = add(&pool, "youtube:a").await else {
            panic!()
        };
        let Added::New(b) = add(&pool, "youtube:b").await else {
            panic!()
        };
        assert_eq!(claim_next(&pool).await.unwrap().unwrap().id, a);
        assert_eq!(claim_next(&pool).await.unwrap().unwrap().id, b);
        assert_eq!(claim_next(&pool).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn interrupted_jobs_resume_at_their_stage(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        set_stage(&pool, id, Stage::Transcribe).await.unwrap();
        // Process dies here. On the next start:
        assert_eq!(recover_interrupted(&pool).await.unwrap(), 1);
        let job = claim_next(&pool).await.unwrap().unwrap();
        assert_eq!(
            (job.id, job.stage, job.status),
            (id, Stage::Transcribe, Status::Running)
        );
    }

    #[sqlx::test]
    async fn failing_keeps_the_stage_for_retry(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        set_stage(&pool, id, Stage::Write).await.unwrap();
        fail(&pool, id, "disk full").await.unwrap();
        let job = get_job(&pool, id).await.unwrap().unwrap();
        assert_eq!(
            (job.stage, job.status, job.reason.as_deref()),
            (Stage::Write, Status::Failed, Some("disk full"))
        );
        assert_eq!(retry_all_failed(&pool).await.unwrap(), 1);
        assert_eq!(
            claim_next(&pool).await.unwrap().unwrap().stage,
            Stage::Write
        );
    }

    #[sqlx::test]
    async fn cancelled_jobs_free_the_video_for_a_new_job(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        assert_eq!(cancel(&pool, id).await.unwrap(), Some(Stage::Metadata));
        assert_eq!(cancel(&pool, id).await.unwrap(), None);
        assert!(matches!(add(&pool, "youtube:a").await, Added::New(new) if new != id));
    }

    #[sqlx::test]
    async fn list_joins_titles_newest_first(pool: PgPool) {
        add(&pool, "youtube:a").await;
        add(&pool, "youtube:b").await;
        upsert_video(&pool, &meta("b")).await.unwrap();
        let jobs = list_jobs(&pool, 10).await.unwrap();
        assert_eq!(
            jobs.iter()
                .map(|j| j.video_key.as_str())
                .collect::<Vec<_>>(),
            ["youtube:b", "youtube:a"]
        );
        assert_eq!(jobs[0].title.as_deref(), Some("Me at the zoo"));
        assert_eq!(jobs[1].title, None);
    }

    #[sqlx::test]
    async fn the_schema_rejects_impossible_states(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        let err = sqlx::query("UPDATE jobs SET status = 'done' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await;
        assert!(
            err.is_err(),
            "done status with a non-done stage must be rejected"
        );
        let err = sqlx::query("UPDATE jobs SET stage = 'banana' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await;
        assert!(err.is_err());
        let err = sqlx::query("UPDATE jobs SET status = 'paused' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await;
        assert!(err.is_err());
        let err = sqlx::query(
            "INSERT INTO videos (key, platform, video_id, url, title) VALUES ('v:x', 'vimeo', 'x', 'u', 't')",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "unknown platform");
        upsert_video(&pool, &meta("a")).await.unwrap();
        let err = sqlx::query("UPDATE videos SET duration_secs = -1 WHERE key = 'youtube:a'")
            .execute(&pool)
            .await;
        assert!(err.is_err(), "negative duration");
        let err = sqlx::query(
            "INSERT INTO transcripts (video_key, source, caption_kind, raw_text, segments)
             VALUES ('youtube:a', 'whisper', 'auto', 'x', '[]')",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "whisper transcripts have no caption kind");
    }

    #[sqlx::test]
    async fn writes_to_a_cancelled_job_report_that_they_did_nothing(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        assert!(set_stage(&pool, id, Stage::Fetch).await.unwrap());
        cancel(&pool, id).await.unwrap();
        assert!(!set_stage(&pool, id, Stage::Transcribe).await.unwrap());
        assert!(!set_caption_error(&pool, id, Some("x")).await.unwrap());
        assert!(!fail(&pool, id, "late failure").await.unwrap());
        assert!(!finish(&pool, id, "/late.md", false).await.unwrap());
        let job = get_job(&pool, id).await.unwrap().unwrap();
        assert_eq!(
            (job.status, job.stage, job.note_path),
            (Status::Cancelled, Stage::Fetch, None)
        );
    }

    #[sqlx::test]
    async fn only_one_dispatcher_holds_the_lock(pool: PgPool) {
        let first = lock_dispatcher(&pool).await.unwrap().expect("first lock");
        assert!(lock_dispatcher(&pool).await.unwrap().is_none());
        first.release().await.unwrap();
        let again = lock_dispatcher(&pool)
            .await
            .unwrap()
            .expect("lock after release");
        drop(again);
    }

    fn cleaned(text: &str, review: bool) -> Cleaned {
        Cleaned {
            text: text.into(),
            translated: true,
            review,
            model: "lmstudio/gemma".into(),
            raw_words: 40,
            clean_words: 20,
            tokens_in: 900,
            tokens_out: 300,
        }
    }

    #[sqlx::test]
    async fn cleanings_keep_every_run_and_return_the_newest(pool: PgPool) {
        upsert_video(&pool, &meta("a")).await.unwrap();
        insert_transcript(&pool, "youtube:a", &transcript("x"), None)
            .await
            .unwrap();
        assert_eq!(latest_cleaning(&pool, "youtube:a").await.unwrap(), None);
        insert_cleaning(&pool, "youtube:a", &cleaned("first", true))
            .await
            .unwrap();
        insert_cleaning(&pool, "youtube:a", &cleaned("second", false))
            .await
            .unwrap();
        assert_eq!(
            latest_cleaning(&pool, "youtube:a").await.unwrap(),
            Some(cleaned("second", false))
        );
        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM cleanings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(runs, 2);
    }

    #[sqlx::test]
    async fn queued_jobs_go_before_due_waiting_ones_which_go_by_due_time(pool: PgPool) {
        let mut ids = Vec::new();
        for key in ["youtube:a", "youtube:b", "youtube:c"] {
            let Added::New(id) = add(&pool, key).await else {
                panic!()
            };
            ids.push(id);
        }
        // a: due a minute ago, b: due two minutes ago, c: still queued.
        for (id, ago) in [(ids[0], 60), (ids[1], 120)] {
            sqlx::query(
                "UPDATE jobs SET status = 'waiting', reason = 'x',
                 retry_at = now() - make_interval(secs => $2) WHERE id = $1",
            )
            .bind(id)
            .bind(f64::from(ago))
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut order = Vec::new();
        while let Some(job) = claim_next(&pool).await.unwrap() {
            assert_eq!(job.reason, None, "a resumed job's wait reason is cleared");
            order.push(job.id);
        }
        assert_eq!(order, [ids[2], ids[1], ids[0]]);
    }

    #[sqlx::test]
    async fn a_platform_s_queued_jobs_can_be_parked_together(pool: PgPool) {
        for key in ["instagram:a", "instagram:b", "youtube:c", "instagram:d"] {
            add(&pool, key).await;
        }
        // d is already running: only queued jobs are parked.
        sqlx::query("UPDATE jobs SET status = 'running' WHERE video_key = 'instagram:d'")
            .execute(&pool)
            .await
            .unwrap();
        let parked = wait_queued_platform(
            &pool,
            "instagram",
            "blocked",
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(parked.len(), 2);
        assert!(parked.iter().all(|(_, stage)| *stage == Stage::Metadata));
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT video_key, status FROM jobs ORDER BY video_key")
                .fetch_all(&pool)
                .await
                .unwrap();
        let s = |k: &str| statuses.iter().find(|(key, _)| key == k).unwrap().1.clone();
        assert_eq!(s("instagram:a"), "waiting");
        assert_eq!(s("instagram:b"), "waiting");
        assert_eq!(s("youtube:c"), "queued");
        assert_eq!(s("instagram:d"), "running");
    }

    #[sqlx::test]
    async fn known_keys_are_done_or_on_their_way(pool: PgPool) {
        for key in ["youtube:a", "youtube:b", "youtube:c"] {
            add(&pool, key).await;
        }
        sqlx::query("UPDATE jobs SET status = 'cancelled' WHERE video_key = 'youtube:b'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET status = 'failed' WHERE video_key = 'youtube:c'")
            .execute(&pool)
            .await
            .unwrap();
        let keys: Vec<String> = ["youtube:a", "youtube:b", "youtube:c", "youtube:z"]
            .map(String::from)
            .to_vec();
        let known = known_keys(&pool, &keys).await.unwrap();
        assert_eq!(known, ["youtube:a".to_string()].into_iter().collect());
    }

    #[sqlx::test]
    async fn waiting_jobs_come_back_when_their_time_comes(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        assert!(
            wait(
                &pool,
                id,
                "LLM not reachable",
                std::time::Duration::from_secs(3600)
            )
            .await
            .unwrap()
        );
        let job = get_job(&pool, id).await.unwrap().unwrap();
        assert_eq!(
            (job.status, job.reason.as_deref()),
            (Status::Waiting, Some("LLM not reachable"))
        );
        assert_eq!(
            claim_next(&pool).await.unwrap(),
            None,
            "not due for an hour"
        );

        sqlx::query("UPDATE jobs SET retry_at = now() - interval '1 second' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(claim_next(&pool).await.unwrap().unwrap().id, id);
    }

    #[sqlx::test]
    async fn retry_wakes_a_waiting_job_immediately(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        wait(&pool, id, "quota", std::time::Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(retry(&pool, id).await.unwrap());
        assert_eq!(claim_next(&pool).await.unwrap().unwrap().id, id);
    }

    #[sqlx::test]
    async fn reclean_jobs_start_at_clean_and_need_a_transcript(pool: PgPool) {
        upsert_video(&pool, &meta("a")).await.unwrap();
        assert!(
            add_reclean_job(&pool, Uuid::new_v4(), "youtube:a")
                .await
                .is_err()
        );
        insert_transcript(&pool, "youtube:a", &transcript("x"), None)
            .await
            .unwrap();
        let Added::New(id) = add_reclean_job(&pool, Uuid::new_v4(), "youtube:a")
            .await
            .unwrap()
        else {
            panic!()
        };
        let job = get_job(&pool, id).await.unwrap().unwrap();
        assert_eq!((job.stage, job.status), (Stage::Clean, Status::Queued));
        assert_eq!(
            add_reclean_job(&pool, Uuid::new_v4(), "youtube:a")
                .await
                .unwrap(),
            Added::AlreadyOpen(id)
        );
    }

    #[sqlx::test]
    async fn note_records_follow_moves(pool: PgPool) {
        upsert_video(&pool, &meta("a")).await.unwrap();
        record_note(&pool, "youtube:a", "/v/Inbox/a.md", "h1", None)
            .await
            .unwrap();
        record_note(
            &pool,
            "youtube:a",
            "/v/Design/UX/a.md",
            "h2",
            Some("/v/Inbox/a.md"),
        )
        .await
        .unwrap();
        assert_eq!(
            notes_for(&pool, "youtube:a").await.unwrap(),
            vec![NoteRecord {
                path: "/v/Design/UX/a.md".into(),
                content_sha256: "h2".into()
            }]
        );
    }

    #[sqlx::test]
    async fn folder_is_stored_on_the_job(pool: PgPool) {
        let Added::New(id) = add(&pool, "youtube:a").await else {
            panic!()
        };
        claim_next(&pool).await.unwrap();
        assert!(set_folder(&pool, id, "Design/UX").await.unwrap());
        assert_eq!(
            get_job(&pool, id).await.unwrap().unwrap().folder.as_deref(),
            Some("Design/UX")
        );
    }
}
