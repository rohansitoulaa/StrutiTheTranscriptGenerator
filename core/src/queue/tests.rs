//! Queue behaviour against a real (throwaway) Postgres, with fake tools that
//! record calls and measure how many of each stage ran at once.

use super::*;
use crate::llm::{ChatReply, LlmError};
use crate::metadata::{CaptionKind, Platform};
use crate::transcript::{Segment, Transcript, TranscriptSource};
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

/// How many of something are running now, and the most ever at once.
#[derive(Default)]
struct Gauge {
    now: AtomicUsize,
    max: AtomicUsize,
}

impl Gauge {
    fn enter(&self) -> GaugeGuard<'_> {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        GaugeGuard(self)
    }
    fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }
    fn now(&self) -> usize {
        self.now.load(Ordering::SeqCst)
    }
}

struct GaugeGuard<'a>(&'a Gauge);

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Shared {
    calls: Mutex<Vec<String>>,
    downloads: Gauge,
    whisper: Gauge,
    /// Set when Whisper ran while a download was in progress.
    overlapped: AtomicBool,
    fail_metadata: Mutex<HashSet<String>>,
    fail_captions: Mutex<HashSet<String>>,
    /// Video ids whose Whisper run errors, or panics.
    fail_whisper: Mutex<HashSet<String>>,
    panic_whisper: Mutex<HashSet<String>>,
    /// LLM behaviour: an error to return, text to prepend to cleanups, or a
    /// fixed cleanup reply.
    llm_fail: Mutex<Option<LlmError>>,
    clean_prefix: Mutex<String>,
    clean_override: Mutex<Option<String>>,
}

impl Shared {
    fn call(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    /// Calls to yt-dlp and Whisper only, without the LLM's.
    fn tool_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| !c.starts_with("llm "))
            .collect()
    }
    fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

/// Video ids starting with `cap` have English captions; the rest go to Whisper.
struct FakeFetcher {
    shared: Arc<Shared>,
    delay: Duration,
}

fn id_of(url: &str) -> String {
    match Source::parse(url).unwrap() {
        Source::YouTubeVideo { id } => id,
        other => panic!("unexpected {other:?}"),
    }
}

impl Fetcher for FakeFetcher {
    async fn metadata(&self, url: &str) -> Result<VideoMeta> {
        let id = id_of(url);
        self.shared.call(format!("metadata {id}"));
        if self.shared.fail_metadata.lock().unwrap().contains(&id) {
            return Err(Error::ToolFailed {
                tool: "yt-dlp",
                status: "exit 1".into(),
                stderr: "HTTP Error 403".into(),
            });
        }
        let captions = id.starts_with("cap");
        Ok(VideoMeta {
            platform: Platform::YouTube,
            url: url.to_string(),
            title: format!("Video {id}"),
            channel: Some("chan".into()),
            upload_date: Some("2026-10-01".into()),
            duration_secs: Some(60),
            language: captions.then(|| "en".into()),
            subtitles: vec![],
            auto_captions: if captions {
                vec!["en-orig".into()]
            } else {
                vec![]
            },
            id,
        })
    }

    async fn captions(
        &self,
        url: &str,
        _track: &crate::metadata::CaptionTrack,
        _dir: &Path,
    ) -> Result<Vec<Segment>> {
        let id = id_of(url);
        self.shared.call(format!("captions {id}"));
        if self.shared.fail_captions.lock().unwrap().contains(&id) {
            return Err(Error::ToolFailed {
                tool: "yt-dlp",
                status: "exit 1".into(),
                stderr: "HTTP Error 429".into(),
            });
        }
        Ok(vec![Segment {
            start_ms: 0,
            end_ms: 900,
            text: "caption words.".into(),
        }])
    }

    async fn audio(&self, url: &str, dir: &Path) -> Result<PathBuf> {
        let id = id_of(url);
        self.shared.call(format!("audio {id}"));
        let _active = self.shared.downloads.enter();
        tokio::time::sleep(self.delay).await;
        let path = dir.join(AUDIO_FILE);
        std::fs::write(&path, b"fake wav").unwrap();
        Ok(path)
    }
}

struct FakeWhisper {
    shared: Arc<Shared>,
    delay: Duration,
}

impl Transcriber for FakeWhisper {
    async fn transcribe(&self, audio: &Path, _language: Option<&str>) -> Result<Transcript> {
        assert!(audio.is_file(), "whisper got a missing audio file");
        let dir = audio
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        self.shared.call(format!("whisper {dir}"));
        let id = dir.trim_start_matches("youtube_").to_string();
        if self.shared.panic_whisper.lock().unwrap().contains(&id) {
            panic!("whisper fake panicked on purpose");
        }
        if self.shared.fail_whisper.lock().unwrap().contains(&id) {
            return Err(Error::ToolFailed {
                tool: "whisper-cli",
                status: "exit 2".into(),
                stderr: "out of GPU memory".into(),
            });
        }
        let _active = self.shared.whisper.enter();
        if self.shared.downloads.now() > 0 {
            self.shared.overlapped.store(true, Ordering::SeqCst);
        }
        tokio::time::sleep(self.delay).await;
        Ok(Transcript {
            source: TranscriptSource::Whisper,
            language: Some("en".into()),
            segments: vec![Segment {
                start_ms: 0,
                end_ms: 900,
                text: "spoken words.".into(),
            }],
        })
    }
}

/// Cleanup echoes the transcript (plus any prefix); the sorter always
/// proposes Testing/Fakes, which exists after the first note.
struct FakeLlm {
    shared: Arc<Shared>,
}

impl Llm for FakeLlm {
    async fn chat(&self, system: &str, user: &str) -> std::result::Result<ChatReply, LlmError> {
        if let Some(e) = self.shared.llm_fail.lock().unwrap().clone() {
            return Err(e);
        }
        let text = if system.contains("Area/Topic") {
            self.shared.call("llm sort".into());
            r#"{"new": {"area": "Testing", "topic": "Fakes", "description": "Notes from fake videos"}}"#
                .to_string()
        } else {
            self.shared.call("llm clean".into());
            match self.shared.clean_override.lock().unwrap().clone() {
                Some(fixed) => fixed,
                None => {
                    let transcript = user.split("Transcript:\n\n").nth(1).unwrap_or_default();
                    format!("{}{transcript}", self.shared.clean_prefix.lock().unwrap())
                }
            }
        };
        Ok(ChatReply {
            text,
            tokens_in: Some(10),
            tokens_out: Some(5),
        })
    }

    fn label(&self) -> String {
        "fake/model".into()
    }
}

type TestQueue = Queue<FakeFetcher, FakeWhisper, FakeLlm>;

fn build_queue(
    pool: &PgPool,
    shared: &Arc<Shared>,
    work: &Path,
    out: &Path,
    limits: Limits,
    delays: (Duration, Duration),
) -> Arc<TestQueue> {
    Queue::new(QueueParts {
        db: pool.clone(),
        engine: Engine {
            fetcher: FakeFetcher {
                shared: Arc::clone(shared),
                delay: delays.0,
            },
            transcriber: FakeWhisper {
                shared: Arc::clone(shared),
                delay: delays.1,
            },
        },
        llm: FakeLlm {
            shared: Arc::clone(shared),
        },
        cleanup: CleanupSettings::default(),
        work_dir: work.to_path_buf(),
        out_dir: out.to_path_buf(),
        limits,
    })
}

struct Harness {
    queue: Arc<TestQueue>,
    shared: Arc<Shared>,
    pool: PgPool,
    _dirs: tempfile::TempDir,
    out: PathBuf,
    work: PathBuf,
}

fn harness(
    pool: PgPool,
    limits: Limits,
    download_delay: Duration,
    whisper_delay: Duration,
) -> Harness {
    let shared = Arc::new(Shared::default());
    let dirs = tempfile::tempdir().unwrap();
    let out = dirs.path().join("out");
    let work = dirs.path().join("work");
    let queue = build_queue(
        &pool,
        &shared,
        &work,
        &out,
        limits,
        (download_delay, whisper_delay),
    );
    Harness {
        queue,
        shared,
        pool,
        _dirs: dirs,
        out,
        work,
    }
}

fn quick(pool: PgPool) -> Harness {
    harness(
        pool,
        Limits::default(),
        Duration::from_millis(10),
        Duration::from_millis(10),
    )
}

fn link(id: &str) -> String {
    format!("https://youtu.be/{id}")
}

/// Starts the dispatcher; returns a token to stop it and its join handle.
fn start(h: &Harness) -> (CancellationToken, tokio::task::JoinHandle<Result<()>>) {
    let token = CancellationToken::new();
    let handle = tokio::spawn(Arc::clone(&h.queue).run(token.clone()));
    (token, handle)
}

/// Waits until no job is queued or running (or panics after 15 s).
async fn settle(h: &Harness) -> Vec<Job> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let jobs = db::list_jobs(&h.pool, 100).await.unwrap();
        if jobs
            .iter()
            .all(|j| !matches!(j.status, Status::Queued | Status::Running))
        {
            return jobs;
        }
        assert!(Instant::now() < deadline, "queue never settled: {jobs:#?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn stop(token: CancellationToken, handle: tokio::task::JoinHandle<Result<()>>) {
    token.cancel();
    handle.await.unwrap().unwrap();
}

#[sqlx::test]
async fn jobs_run_to_notes_via_captions_or_whisper(pool: PgPool) {
    let h = quick(pool);
    let report = h
        .queue
        .enqueue(&[link("capAAAAAAAA"), link("whisperBBBB")])
        .await
        .unwrap();
    assert_eq!(report.added.len(), 2);
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;

    for job in &jobs {
        assert_eq!(
            (job.status, job.stage),
            (Status::Done, Stage::Done),
            "{job:#?}"
        );
        let note = std::fs::read_to_string(job.note_path.as_ref().unwrap()).unwrap();
        assert!(note.starts_with("---\ntitle: \"Video "), "{note}");
    }
    let cap = db::load_transcript(&h.pool, "youtube:capAAAAAAAA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cap.source, TranscriptSource::Captions(CaptionKind::Auto));
    let wh = db::load_transcript(&h.pool, "youtube:whisperBBBB")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wh.source, TranscriptSource::Whisper);
    assert_eq!(
        h.shared.count("audio capAAAAAAAA"),
        0,
        "captions path must not download audio"
    );
    assert!(
        !h.work.join("youtube_whisperBBBB").exists(),
        "audio folder should be cleaned up"
    );
    let filed = h.out.join("Testing/Fakes");
    let notes = std::fs::read_dir(&filed).unwrap().filter(|e| {
        e.as_ref()
            .unwrap()
            .path()
            .extension()
            .is_some_and(|x| x == "md")
    });
    assert_eq!(notes.count(), 2);
}

#[sqlx::test]
async fn enqueue_sorts_every_line_into_one_bucket(pool: PgPool) {
    let h = quick(pool);
    let report = h
        .queue
        .enqueue(&[
            link("whisperAAAA"),
            link("whisperAAAA"),
            "   ".to_string(),
            "not a link".to_string(),
            "https://www.youtube.com/@kolejain".to_string(),
        ])
        .await
        .unwrap();
    assert_eq!(report.added.len(), 1);
    assert_eq!(report.already_queued, report.added);
    assert_eq!(report.rejected.len(), 2);
    assert!(report.rejected[0].1.contains("not a supported link"));
    assert!(
        report.rejected[1]
            .1
            .contains("channel links are not supported yet")
    );

    let (token, handle) = start(&h);
    settle(&h).await;
    stop(token, handle).await;
    let again = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    assert_eq!(again.added.len(), 0);
    assert_eq!(again.already_done.len(), 1);
    assert!(again.already_done[0].1.as_deref().unwrap().ends_with(".md"));
}

#[sqlx::test]
async fn one_failure_never_stops_the_queue_and_retry_resumes(pool: PgPool) {
    let h = quick(pool);
    h.shared
        .fail_metadata
        .lock()
        .unwrap()
        .insert("whisperBBBB".into());
    h.queue
        .enqueue(&[
            link("whisperAAAA"),
            link("whisperBBBB"),
            link("whisperCCCC"),
        ])
        .await
        .unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;

    let failed: Vec<_> = jobs.iter().filter(|j| j.status == Status::Failed).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].video_key, "youtube:whisperBBBB");
    assert_eq!(failed[0].stage, Stage::Metadata);
    assert!(failed[0].reason.as_deref().unwrap().contains("403"));
    assert_eq!(jobs.iter().filter(|j| j.status == Status::Done).count(), 2);

    h.shared.fail_metadata.lock().unwrap().clear();
    assert_eq!(h.queue.retry_all_failed().await.unwrap(), 1);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert!(jobs.iter().all(|j| j.status == Status::Done), "{jobs:#?}");
    assert_eq!(
        h.shared.count("metadata whisperAAAA"),
        1,
        "finished jobs are not redone"
    );
}

#[sqlx::test]
async fn whisper_runs_one_at_a_time_while_downloads_overlap_it(pool: PgPool) {
    let limits = Limits {
        downloads: 2,
        whisper: 1,
        llm: 1,
    };
    let h = harness(
        pool,
        limits,
        Duration::from_millis(80),
        Duration::from_millis(80),
    );
    let links: Vec<String> = (0..6).map(|i| link(&format!("whisper{i:04}"))).collect();
    h.queue.enqueue(&links).await.unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;

    assert!(jobs.iter().all(|j| j.status == Status::Done));
    assert_eq!(h.shared.whisper.max(), 1, "never two Whisper runs at once");
    assert_eq!(
        h.shared.downloads.max(),
        2,
        "downloads use their full limit"
    );
    assert!(
        h.shared.overlapped.load(Ordering::SeqCst),
        "a download ran during a Whisper run"
    );
}

#[sqlx::test]
async fn a_restart_resumes_without_redoing_finished_stages(pool: PgPool) {
    let h = quick(pool);
    let report = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    let id = report.added[0];
    // State as a crash would leave it: metadata stored, audio downloaded,
    // job still marked running at the transcribe stage.
    let meta = h.queue.engine.metadata(&link("whisperAAAA")).await.unwrap();
    db::upsert_video(&h.pool, &meta).await.unwrap();
    db::claim_next(&h.pool).await.unwrap();
    db::set_stage(&h.pool, id, Stage::Transcribe).await.unwrap();
    let job_dir = h.work.join("youtube_whisperAAAA");
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(job_dir.join(AUDIO_FILE), b"fake wav").unwrap();
    let before = h.shared.tool_calls().len();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert_eq!(
        h.shared.tool_calls()[before..],
        ["whisper youtube_whisperAAAA".to_string()]
    );
}

#[sqlx::test]
async fn missing_audio_at_transcribe_is_fetched_again(pool: PgPool) {
    let h = quick(pool);
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let meta = h.queue.engine.metadata(&link("whisperAAAA")).await.unwrap();
    db::upsert_video(&h.pool, &meta).await.unwrap();
    db::claim_next(&h.pool).await.unwrap();
    db::set_stage(&h.pool, id, Stage::Transcribe).await.unwrap();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert_eq!(h.shared.count("metadata"), 1, "only the test's own call");
    assert_eq!(h.shared.count("audio whisperAAAA"), 1);
}

#[sqlx::test]
async fn shutdown_mid_job_leaves_it_to_resume_on_the_next_run(pool: PgPool) {
    let h = harness(
        pool,
        Limits::default(),
        Duration::from_millis(10),
        Duration::from_secs(30),
    );
    h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    let (token, handle) = start(&h);
    eventually("whisper to start", || h.shared.whisper.now() == 1).await;
    stop(token, handle).await;

    let job = &db::list_jobs(&h.pool, 1).await.unwrap()[0];
    assert_eq!(
        (job.status, job.stage),
        (Status::Running, Stage::Transcribe)
    );

    // Next launch, with a fast Whisper: picks up at transcribe.
    let queue = build_queue(
        &h.pool,
        &h.shared,
        &h.work,
        &h.out,
        Limits::default(),
        (Duration::ZERO, Duration::ZERO),
    );
    let token = CancellationToken::new();
    let handle = tokio::spawn(Arc::clone(&queue).run(token.clone()));
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert_eq!(h.shared.count("metadata"), 1);
    assert_eq!(h.shared.count("audio"), 1);
    assert_eq!(
        h.shared.count("whisper"),
        2,
        "the interrupted run, then the resumed one"
    );
}

#[sqlx::test]
async fn paused_queues_claim_nothing_until_resumed(pool: PgPool) {
    let h = quick(pool);
    h.queue.pause();
    h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    let (token, handle) = start(&h);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        db::list_jobs(&h.pool, 1).await.unwrap()[0].status,
        Status::Queued
    );
    assert!(h.shared.calls().is_empty());

    h.queue.resume();
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
}

#[sqlx::test]
async fn cancelling_a_running_job_stops_it_and_frees_the_video(pool: PgPool) {
    let h = harness(
        pool,
        Limits::default(),
        Duration::from_millis(10),
        Duration::from_secs(30),
    );
    let ids = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added;
    let (token, handle) = start(&h);
    eventually("whisper to start", || h.shared.whisper.now() == 1).await;

    let mut events = h.queue.subscribe();
    assert!(h.queue.cancel(ids[0]).await.unwrap());
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((event.job_id, event.status), (ids[0], Status::Cancelled));
    eventually("the Whisper run to be dropped", || {
        h.shared.whisper.now() == 0
    })
    .await;

    let again = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    assert_eq!(again.added.len(), 1);
    stop(token, handle).await;
}

#[sqlx::test]
async fn events_follow_a_job_through_its_stages(pool: PgPool) {
    let h = quick(pool);
    let mut events = h.queue.subscribe();
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let (token, handle) = start(&h);
    settle(&h).await;
    stop(token, handle).await;

    let mut seen = Vec::new();
    while let Ok(e) = events.try_recv() {
        assert_eq!(e.job_id, id);
        seen.push(state_label(e.stage, e.status));
    }
    seen.dedup();
    assert_eq!(
        seen,
        [
            "fetching metadata",
            "downloading",
            "transcribing",
            "cleaning",
            "filing",
            "writing",
            "done"
        ]
    );
}

async fn transcript_caption_error(pool: &PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT caption_error FROM transcripts WHERE video_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn caption_failure_falls_back_and_its_reason_survives_a_restart(pool: PgPool) {
    let h = harness(
        pool,
        Limits::default(),
        Duration::from_millis(10),
        Duration::from_secs(30),
    );
    h.shared
        .fail_captions
        .lock()
        .unwrap()
        .insert("capAAAAAAAA".into());
    h.queue.enqueue(&[link("capAAAAAAAA")]).await.unwrap();
    let (token, handle) = start(&h);
    eventually("whisper to start", || h.shared.whisper.now() == 1).await;
    stop(token, handle).await;

    // Next launch picks up at transcribe with the reason it stored.
    let queue = build_queue(
        &h.pool,
        &h.shared,
        &h.work,
        &h.out,
        Limits::default(),
        (Duration::ZERO, Duration::ZERO),
    );
    let token = CancellationToken::new();
    let handle = tokio::spawn(Arc::clone(&queue).run(token.clone()));
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    let transcript = db::load_transcript(&h.pool, "youtube:capAAAAAAAA")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(transcript.source, TranscriptSource::Whisper);
    let why = transcript_caption_error(&h.pool, "youtube:capAAAAAAAA").await;
    assert!(
        why.as_deref().unwrap_or_default().contains("429"),
        "{why:?}"
    );
    assert_eq!(
        h.shared.count("captions"),
        1,
        "captions not retried after the restart"
    );
}

#[sqlx::test]
async fn a_transcribe_failure_is_retried_without_redoing_earlier_stages(pool: PgPool) {
    let h = quick(pool);
    h.shared
        .fail_whisper
        .lock()
        .unwrap()
        .insert("whisperBBBB".into());
    let mut events = h.queue.subscribe();
    h.queue
        .enqueue(&[link("whisperAAAA"), link("whisperBBBB")])
        .await
        .unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;

    let failed = jobs.iter().find(|j| j.status == Status::Failed).unwrap();
    assert_eq!(
        (failed.video_key.as_str(), failed.stage),
        ("youtube:whisperBBBB", Stage::Transcribe)
    );
    assert!(
        failed
            .reason
            .as_deref()
            .unwrap()
            .contains("out of GPU memory")
    );
    assert_eq!(jobs.iter().filter(|j| j.status == Status::Done).count(), 1);
    let mut failure_event = None;
    while let Ok(e) = events.try_recv() {
        if e.status == Status::Failed {
            failure_event = Some(e);
        }
    }
    let e = failure_event.expect("a Failed event");
    assert_eq!((e.job_id, e.stage), (failed.id, Stage::Transcribe));
    assert!(e.reason.unwrap().contains("out of GPU memory"));

    h.shared.fail_whisper.lock().unwrap().clear();
    h.queue.retry(failed.id).await.unwrap();
    settle(&h).await;
    stop(token, handle).await;
    assert_eq!(
        db::get_job(&h.pool, failed.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        Status::Done
    );
    assert_eq!(h.shared.count("metadata whisperBBBB"), 1);
    assert_eq!(
        h.shared.count("audio whisperBBBB"),
        1,
        "the downloaded audio was kept"
    );
    assert_eq!(h.shared.count("whisper youtube_whisperBBBB"), 2);
}

#[sqlx::test]
async fn a_stored_transcript_skips_fetching_and_whisper(pool: PgPool) {
    let h = quick(pool);
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    // A crash right after storing the transcript, before the stage advanced.
    let meta = h.queue.engine.metadata(&link("whisperAAAA")).await.unwrap();
    db::upsert_video(&h.pool, &meta).await.unwrap();
    let stored = Transcript {
        source: TranscriptSource::Whisper,
        language: Some("en".into()),
        segments: vec![Segment {
            start_ms: 0,
            end_ms: 1,
            text: "already here.".into(),
        }],
    };
    db::insert_transcript(&h.pool, "youtube:whisperAAAA", &stored, None)
        .await
        .unwrap();
    db::claim_next(&h.pool).await.unwrap();
    db::set_stage(&h.pool, id, Stage::Fetch).await.unwrap();
    let before = h.shared.tool_calls().len();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert!(
        h.shared.tool_calls()[before..].is_empty(),
        "{:?}",
        h.shared.calls()
    );
    let note = std::fs::read_to_string(jobs[0].note_path.as_ref().unwrap()).unwrap();
    assert!(note.ends_with("already here.\n"));
}

#[sqlx::test]
async fn a_stored_transcript_also_skips_whisper_when_resuming_at_transcribe(pool: PgPool) {
    let h = quick(pool);
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let meta = h.queue.engine.metadata(&link("whisperAAAA")).await.unwrap();
    db::upsert_video(&h.pool, &meta).await.unwrap();
    let stored = Transcript {
        source: TranscriptSource::Whisper,
        language: Some("en".into()),
        segments: vec![Segment {
            start_ms: 0,
            end_ms: 1,
            text: "stored before the crash.".into(),
        }],
    };
    db::insert_transcript(&h.pool, "youtube:whisperAAAA", &stored, None)
        .await
        .unwrap();
    db::claim_next(&h.pool).await.unwrap();
    db::set_stage(&h.pool, id, Stage::Transcribe).await.unwrap();
    // Audio is still there, so only the stored transcript stops Whisper.
    let job_dir = h.work.join("youtube_whisperAAAA");
    std::fs::create_dir_all(&job_dir).unwrap();
    std::fs::write(job_dir.join(AUDIO_FILE), b"fake wav").unwrap();
    let before = h.shared.tool_calls().len();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert!(
        h.shared.tool_calls()[before..].is_empty(),
        "{:?}",
        h.shared.calls()
    );
}

#[sqlx::test]
async fn cancelling_a_queued_job_emits_and_it_never_runs(pool: PgPool) {
    let h = quick(pool);
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let mut events = h.queue.subscribe();
    assert!(h.queue.cancel(id).await.unwrap());
    let e = events.try_recv().unwrap();
    assert_eq!(
        (e.job_id, e.stage, e.status),
        (id, Stage::Metadata, Status::Cancelled)
    );
    assert!(
        !h.queue.cancel(id).await.unwrap(),
        "second cancel is a no-op"
    );

    let (token, handle) = start(&h);
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop(token, handle).await;
    assert!(h.shared.calls().is_empty());
}

#[sqlx::test]
async fn a_job_cancelled_elsewhere_stops_at_its_next_stage(pool: PgPool) {
    let h = harness(
        pool,
        Limits::default(),
        Duration::from_millis(300),
        Duration::from_millis(10),
    );
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let (token, handle) = start(&h);
    eventually("the download to start", || h.shared.downloads.now() == 1).await;
    // Cancelled by another process (no abort here), mid-download.
    db::cancel(&h.pool, id).await.unwrap();
    eventually("the download to finish", || h.shared.downloads.now() == 0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop(token, handle).await;

    let job = db::get_job(&h.pool, id).await.unwrap().unwrap();
    assert_eq!((job.status, job.note_path), (Status::Cancelled, None));
    assert_eq!(h.shared.count("whisper"), 0, "stopped before transcribing");
}

#[sqlx::test]
async fn a_panicking_job_is_failed_with_a_reason_and_the_rest_continue(pool: PgPool) {
    let h = quick(pool);
    h.shared
        .panic_whisper
        .lock()
        .unwrap()
        .insert("whisperBBBB".into());
    h.queue
        .enqueue(&[
            link("whisperAAAA"),
            link("whisperBBBB"),
            link("whisperCCCC"),
        ])
        .await
        .unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;

    let crashed = jobs
        .iter()
        .find(|j| j.video_key == "youtube:whisperBBBB")
        .unwrap();
    assert_eq!(crashed.status, Status::Failed);
    let reason = crashed.reason.as_deref().unwrap();
    assert!(
        reason.starts_with("internal error") && reason.contains("on purpose"),
        "{reason}"
    );
    assert_eq!(jobs.iter().filter(|j| j.status == Status::Done).count(), 2);
}

#[sqlx::test]
async fn a_second_dispatcher_refuses_to_run(pool: PgPool) {
    let h = quick(pool);
    let (token, handle) = start(&h);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let other = harness(
        h.pool.clone(),
        Limits::default(),
        Duration::ZERO,
        Duration::ZERO,
    );
    let err = Arc::clone(&other.queue)
        .run(CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::QueueBusy), "{err:?}");
    stop(token, handle).await;
    // Released on shutdown: the other one can run now.
    let (token, handle) = start(&other);
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop(token, handle).await;
}

#[sqlx::test]
async fn re_pasting_a_failed_link_lands_in_the_retried_bucket(pool: PgPool) {
    let h = quick(pool);
    h.shared
        .fail_metadata
        .lock()
        .unwrap()
        .insert("whisperAAAA".into());
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let (token, handle) = start(&h);
    settle(&h).await;
    h.shared.fail_metadata.lock().unwrap().clear();
    let report = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    assert_eq!((report.retried, report.added.len()), (vec![id], 0));
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
}

fn note_of(job: &Job) -> String {
    std::fs::read_to_string(job.note_path.as_ref().expect("a note path")).unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test]
async fn notes_are_cleaned_and_filed_into_a_sorted_folder(pool: PgPool) {
    let h = quick(pool);
    h.queue
        .enqueue(&[link("capAAAAAAAA"), link("whisperBBBB")])
        .await
        .unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;

    for job in &jobs {
        assert_eq!(job.status, Status::Done, "{job:#?}");
        assert!(!job.review);
        assert_eq!(job.folder.as_deref(), Some("Testing/Fakes"));
        let path = PathBuf::from(job.note_path.as_ref().unwrap());
        assert_eq!(path.parent().unwrap(), h.out.join("Testing/Fakes"));
        let note = note_of(job);
        assert!(
            note.contains("cleaned_by: \"fake/model\"\ncleanup: ok"),
            "{note}"
        );
        assert!(
            note.ends_with("---\n\ncaption words.\n") || note.ends_with("---\n\nspoken words.\n"),
            "{note}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(h.out.join("Testing/Fakes/.shruti")).unwrap(),
        "Notes from fake videos\n"
    );
    assert_eq!(count(&h.pool, "SELECT count(*) FROM cleanings").await, 2);
    assert_eq!(count(&h.pool, "SELECT count(*) FROM notes").await, 2);
}

#[sqlx::test]
async fn an_unreachable_llm_parks_jobs_until_it_is_back(pool: PgPool) {
    let h = quick(pool);
    *h.shared.llm_fail.lock().unwrap() = Some(LlmError::Unreachable {
        url: "http://localhost:1234/v1/chat/completions".into(),
        reason: "could not connect".into(),
    });
    let mut events = h.queue.subscribe();
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    let job = &jobs[0];
    assert_eq!((job.status, job.stage), (Status::Waiting, Stage::Clean));
    assert!(job.reason.as_deref().unwrap().contains("could not connect"));
    // The event follows the database write, so it may lag `settle`.
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(e) if e.status == Status::Waiting && e.stage == Stage::Clean => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await;
    assert!(waited.is_ok(), "a Waiting event");
    assert_eq!(
        h.shared.count("whisper"),
        1,
        "transcription isn't redone while waiting"
    );

    *h.shared.llm_fail.lock().unwrap() = None;
    assert!(h.queue.retry(id).await.unwrap());
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert_eq!(h.shared.count("whisper"), 1);
}

/// Records what it was asked and how many requests overlapped.
#[derive(Default)]
struct SlowLlm {
    log: Mutex<Vec<String>>,
    now: AtomicUsize,
    max: AtomicUsize,
}

impl Llm for SlowLlm {
    async fn chat(&self, _system: &str, user: &str) -> std::result::Result<ChatReply, LlmError> {
        use std::sync::atomic::Ordering::SeqCst;
        let n = self.now.fetch_add(1, SeqCst) + 1;
        self.max.fetch_max(n, SeqCst);
        self.log.lock().unwrap().push(user.to_string());
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.now.fetch_sub(1, SeqCst);
        if user.ends_with("fail") {
            return Err(LlmError::BadResponse("x".into()));
        }
        Ok(ChatReply {
            text: "ok".into(),
            tokens_in: None,
            tokens_out: None,
        })
    }

    fn label(&self) -> String {
        "slow".into()
    }
}

#[tokio::test]
async fn llm_slots_are_per_request_so_two_jobs_chunks_interleave() {
    use std::sync::atomic::Ordering::SeqCst;
    let llm = SlowLlm::default();
    let slots = Semaphore::new(1);
    let gated = Gated {
        llm: &llm,
        slots: &slots,
    };
    let g = &gated;
    let job = |name: &'static str| async move {
        for i in 0..3 {
            // A failed request must hand its slot on too.
            let user = if (name, i) == ("a", 0) {
                "a0 fail".to_string()
            } else {
                format!("{name}{i}")
            };
            let _ = g.chat("s", &user).await;
        }
    };
    tokio::join!(job("a"), job("b"));
    assert_eq!(llm.max.load(SeqCst), 1, "one request at a time");
    assert_eq!(
        *llm.log.lock().unwrap(),
        ["a0 fail", "b0", "a1", "b1", "a2", "b2"]
    );
    assert_eq!(slots.available_permits(), 1);
}

#[sqlx::test]
async fn a_waiting_job_resumes_by_itself_once_due(pool: PgPool) {
    let h = quick(pool);
    *h.shared.llm_fail.lock().unwrap() = Some(LlmError::RateLimited { retry_after: None });
    let id = h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap().added[0];
    let (token, handle) = start(&h);
    assert_eq!(settle(&h).await[0].status, Status::Waiting);

    *h.shared.llm_fail.lock().unwrap() = None;
    sqlx::query("UPDATE jobs SET retry_at = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(&h.pool)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let job = db::get_job(&h.pool, id).await.unwrap().unwrap();
        if job.status == Status::Running {
            assert_eq!(job.reason, None, "the wait's reason is cleared");
        }
        if job.status == Status::Done {
            break;
        }
        assert!(Instant::now() < deadline, "never resumed: {job:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop(token, handle).await;
    assert_eq!(h.shared.count("whisper"), 1);
}

#[sqlx::test]
async fn an_unusable_llm_reply_fails_the_job(pool: PgPool) {
    let h = quick(pool);
    *h.shared.llm_fail.lock().unwrap() = Some(LlmError::BadResponse("<html>".into()));
    h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(
        (jobs[0].status, jobs[0].stage),
        (Status::Failed, Stage::Clean)
    );
}

#[sqlx::test]
async fn a_suspicious_cleanup_is_written_but_flagged_for_review(pool: PgPool) {
    let h = quick(pool);
    *h.shared.clean_override.lock().unwrap() = Some(String::new());
    h.queue.enqueue(&[link("whisperAAAA")]).await.unwrap();
    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert!(jobs[0].review);
    assert!(note_of(&jobs[0]).contains("cleanup: review"));
}

/// Runs one link to a finished note and returns the job.
async fn finished(h: &Harness, id: &str) -> Job {
    h.queue.enqueue(&[link(id)]).await.unwrap();
    let (token, handle) = start(h);
    let jobs = settle(h).await;
    stop(token, handle).await;
    jobs.into_iter()
        .find(|j| j.video_key.ends_with(id))
        .unwrap()
}

async fn reclean(h: &Harness, id: &str) -> Job {
    let key = format!("youtube:{id}");
    let results = h.queue.reclean(std::slice::from_ref(&key)).await.unwrap();
    assert!(
        matches!(results[0].1, Ok(Added::New(_))),
        "{:?}",
        results[0].1
    );
    let (token, handle) = start(h);
    let jobs = settle(h).await;
    stop(token, handle).await;
    jobs.into_iter().find(|j| j.video_key == key).unwrap()
}

#[sqlx::test]
async fn reclean_rewrites_an_unedited_note_in_place(pool: PgPool) {
    let h = quick(pool);
    let first = finished(&h, "whisperAAAA").await;
    let sorts = h.shared.count("llm sort");
    *h.shared.clean_prefix.lock().unwrap() = "Revised. ".into();

    let again = reclean(&h, "whisperAAAA").await;
    assert_eq!(again.status, Status::Done);
    assert_eq!(
        again.note_path, first.note_path,
        "same note, updated in place"
    );
    assert!(note_of(&again).contains("Revised. spoken words."));
    assert_eq!(again.folder.as_deref(), Some("Testing/Fakes"));
    assert_eq!(h.shared.count("whisper"), 1, "no re-transcription");
    assert_eq!(h.shared.count("llm sort"), sorts, "no re-filing");
    assert_eq!(count(&h.pool, "SELECT count(*) FROM cleanings").await, 2);
}

#[sqlx::test]
async fn reclean_never_overwrites_an_edited_note(pool: PgPool) {
    let h = quick(pool);
    let first = finished(&h, "whisperAAAA").await;
    let original = PathBuf::from(first.note_path.as_ref().unwrap());
    std::fs::write(&original, "my own notes").unwrap();
    *h.shared.clean_prefix.lock().unwrap() = "Revised. ".into();

    let again = reclean(&h, "whisperAAAA").await;
    let copy = PathBuf::from(again.note_path.as_ref().unwrap());
    assert_ne!(copy, original);
    assert_eq!(
        copy.parent(),
        original.parent(),
        "the new copy sits next to the edited one"
    );
    assert_eq!(std::fs::read_to_string(&original).unwrap(), "my own notes");
    assert!(note_of(&again).contains("Revised."));
}

#[sqlx::test]
async fn reclean_follows_a_note_moved_in_obsidian(pool: PgPool) {
    let h = quick(pool);
    let first = finished(&h, "whisperAAAA").await;
    let original = PathBuf::from(first.note_path.as_ref().unwrap());
    let moved = h.out.join("Mine/Picked By Hand/renamed.md");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&original, &moved).unwrap();
    *h.shared.clean_prefix.lock().unwrap() = "Revised. ".into();

    let again = reclean(&h, "whisperAAAA").await;
    assert_eq!(
        again.note_path.as_deref(),
        Some(moved.display().to_string().as_str())
    );
    assert!(
        std::fs::read_to_string(&moved)
            .unwrap()
            .contains("Revised.")
    );
    assert!(!original.exists(), "never moved back");
    assert_eq!(again.folder.as_deref(), Some("Mine/Picked By Hand"));
    let recorded = db::notes_for(&h.pool, "youtube:whisperAAAA").await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].path, moved.display().to_string());
}

#[sqlx::test]
async fn reclean_of_a_moved_and_edited_note_writes_a_copy_beside_it(pool: PgPool) {
    let h = quick(pool);
    let first = finished(&h, "whisperAAAA").await;
    let original = PathBuf::from(first.note_path.as_ref().unwrap());
    let moved = h.out.join("Mine/Picked By Hand/renamed.md");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&original, &moved).unwrap();
    // Edited, but the frontmatter (and its id) is kept.
    let edited = format!(
        "{}\nMy own thoughts.\n",
        std::fs::read_to_string(&moved).unwrap()
    );
    std::fs::write(&moved, &edited).unwrap();
    *h.shared.clean_prefix.lock().unwrap() = "Revised. ".into();

    let again = reclean(&h, "whisperAAAA").await;
    assert_eq!(again.status, Status::Done);
    let copy = PathBuf::from(again.note_path.as_ref().unwrap());
    assert_ne!(copy, moved);
    assert_eq!(copy.parent(), moved.parent(), "beside the moved note");
    assert_eq!(
        std::fs::read_to_string(&moved).unwrap(),
        edited,
        "edit kept"
    );
    assert!(note_of(&again).contains("Revised."));
    assert!(!original.exists());
}

#[test]
fn job_folders_are_safe_names() {
    assert_eq!(job_dir_name("youtube:abc-_D"), "youtube_abc-_D");
    assert_eq!(job_dir_name("instagram:../x"), "instagram____x");
}
