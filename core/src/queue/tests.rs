//! Queue behaviour against a real (throwaway) Postgres, with fake tools that
//! record calls and measure how many of each stage ran at once.

use super::*;
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
}

impl Shared {
    fn call(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
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

struct Harness {
    queue: Arc<Queue<FakeFetcher, FakeWhisper>>,
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
    let engine = Engine {
        fetcher: FakeFetcher {
            shared: Arc::clone(&shared),
            delay: download_delay,
        },
        transcriber: FakeWhisper {
            shared: Arc::clone(&shared),
            delay: whisper_delay,
        },
    };
    let queue = Queue::new(pool.clone(), engine, work.clone(), out.clone(), limits);
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
    assert_eq!(std::fs::read_dir(&h.out).unwrap().count(), 2);
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
    let before = h.shared.calls().len();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert_eq!(
        h.shared.calls()[before..],
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
    let engine = Engine {
        fetcher: FakeFetcher {
            shared: Arc::clone(&h.shared),
            delay: Duration::ZERO,
        },
        transcriber: FakeWhisper {
            shared: Arc::clone(&h.shared),
            delay: Duration::ZERO,
        },
    };
    let queue = Queue::new(
        h.pool.clone(),
        engine,
        h.work.clone(),
        h.out.clone(),
        Limits::default(),
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
    let engine = Engine {
        fetcher: FakeFetcher {
            shared: Arc::clone(&h.shared),
            delay: Duration::ZERO,
        },
        transcriber: FakeWhisper {
            shared: Arc::clone(&h.shared),
            delay: Duration::ZERO,
        },
    };
    let queue = Queue::new(
        h.pool.clone(),
        engine,
        h.work.clone(),
        h.out.clone(),
        Limits::default(),
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
    let before = h.shared.calls().len();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert!(
        h.shared.calls()[before..].is_empty(),
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
    let before = h.shared.calls().len();

    let (token, handle) = start(&h);
    let jobs = settle(&h).await;
    stop(token, handle).await;
    assert_eq!(jobs[0].status, Status::Done);
    assert!(
        h.shared.calls()[before..].is_empty(),
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

#[test]
fn job_folders_are_safe_names() {
    assert_eq!(job_dir_name("youtube:abc-_D"), "youtube_abc-_D");
    assert_eq!(job_dir_name("instagram:../x"), "instagram____x");
}
