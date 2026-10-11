//! Dev CLI: `shruti [--once] [--out DIR] <link>…`
//!
//! By default links go through the Postgres-backed queue: progress is
//! printed per stage, Ctrl-C stops cleanly, and the next run resumes where
//! it left off. `--once` runs each link directly with no database.
//! Warnings (caption fallback, GPU fallback, repaired text) go to stderr.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use shruti_core::Error;
use shruti_core::config::Config;
use shruti_core::db::{self, Status};
use shruti_core::output::write_note;
use shruti_core::pipeline::Engine;
use shruti_core::queue::{EnqueueReport, JobEvent, Queue, state_label};
use shruti_core::whisper::WhisperCli;
use shruti_core::ytdlp::YtDlp;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

struct Args {
    once: bool,
    out_dir: PathBuf,
    links: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let Some(args) = parse_args(std::env::args().skip(1)) else {
        eprintln!("usage: shruti [--once] [--out DIR] <link>…");
        return ExitCode::from(2);
    };
    let result = match setup().await {
        Ok((_, engine)) if args.once => run_once(&engine, &args).await,
        Ok((config, engine)) => run_queue(config, engine, &args).await,
        Err(msg) => Err(msg),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(msg) => {
            eprintln!("{}", printable(&msg));
            ExitCode::FAILURE
        }
    }
}

fn parse_args(mut raw: impl Iterator<Item = String>) -> Option<Args> {
    let mut args = Args {
        once: false,
        out_dir: PathBuf::from("out"),
        links: Vec::new(),
    };
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--out" | "-o" => args.out_dir = raw.next()?.into(),
            "--once" => args.once = true,
            "-h" | "--help" => return None,
            _ => args.links.push(arg),
        }
    }
    (!args.links.is_empty()).then_some(args)
}

async fn setup() -> Result<(Config, Engine<YtDlp, WhisperCli>), String> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."));
    let config =
        Config::from_env(repo_root, |k| std::env::var(k).ok()).map_err(|e| e.to_string())?;
    let problems = config.problems();
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let engine = config.build_engine().await.map_err(|e| e.to_string())?;
    match engine.transcriber.gpu_device {
        Some(n) => eprintln!("whisper device: GPU {n}"),
        None => eprintln!("whisper device: default (no discrete GPU found, slower)"),
    }
    Ok((config, engine))
}

type CliQueue = Queue<YtDlp, WhisperCli>;

/// Returns whether every link produced a note.
async fn run_queue(
    config: Config,
    engine: Engine<YtDlp, WhisperCli>,
    args: &Args,
) -> Result<bool, String> {
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|e| e.to_string())?;
    let queue = Queue::new(
        pool,
        engine,
        config.work_dir,
        args.out_dir.clone(),
        config.limits,
    );
    let events = queue.subscribe();
    let report = queue
        .enqueue(&args.links)
        .await
        .map_err(|e| e.to_string())?;
    print_report(&report);

    let batch: HashSet<i64> = report
        .added
        .iter()
        .chain(&report.retried)
        .chain(&report.already_queued)
        .copied()
        .collect();
    let clean_paste = report.rejected.is_empty() && report.errors.is_empty();
    if batch.is_empty() {
        return Ok(clean_paste);
    }
    let finished = wait_for_batch(&queue, events, batch.clone()).await?;
    Ok(print_notes(&queue, &batch).await? && finished && clean_paste)
}

fn print_report(report: &EnqueueReport) {
    for (input, reason) in report.rejected.iter().chain(&report.errors) {
        println!("skip  {}\n      {}", printable(input), printable(reason));
    }
    for (input, note) in &report.already_done {
        let note = note.as_deref().unwrap_or("(no note path)");
        println!("done  {}  (earlier: {})", printable(input), printable(note));
    }
}

/// Runs the queue, printing progress, until every job in `waiting` has
/// finished (true) or the user presses Ctrl-C (false).
async fn wait_for_batch(
    queue: &Arc<CliQueue>,
    mut events: broadcast::Receiver<JobEvent>,
    mut waiting: HashSet<i64>,
) -> Result<bool, String> {
    let keys: HashMap<i64, String> = queue
        .jobs(100_000)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|j| (j.id, j.video_key))
        .collect();
    let shutdown = CancellationToken::new();
    let mut runner = tokio::spawn(Arc::clone(queue).run(shutdown.clone()));
    let started = Instant::now();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));

    while !waiting.is_empty() {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("stopping; unfinished jobs resume on the next run");
                break;
            }
            // The dispatcher only returns early on an error (e.g. another
            // instance is already working the queue).
            ended = &mut runner => return Err(runner_error(ended)),
            event = events.recv() => match event {
                Ok(e) if waiting.contains(&e.job_id) => {
                    let key = keys.get(&e.job_id).map_or("?", String::as_str);
                    print_event(&e, key, started);
                    if is_terminal(e.status) {
                        waiting.remove(&e.job_id);
                    }
                }
                Ok(_) => {}
                // Missed updates may include a job finishing: ask the database.
                Err(RecvError::Lagged(_)) => reconcile(queue, &mut waiting).await?,
                Err(RecvError::Closed) => break,
            },
            _ = ticker.tick() => reconcile(queue, &mut waiting).await?,
        }
    }
    shutdown.cancel();
    runner
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(waiting.is_empty())
}

fn runner_error(ended: Result<shruti_core::Result<()>, tokio::task::JoinError>) -> String {
    match ended {
        Ok(Ok(())) => "the queue stopped unexpectedly".to_string(),
        Ok(Err(Error::QueueBusy)) => {
            format!("{}; these links were added to its queue", Error::QueueBusy)
        }
        Ok(Err(e)) => e.to_string(),
        Err(e) => e.to_string(),
    }
}

fn print_event(e: &JobEvent, key: &str, started: Instant) {
    let secs = started.elapsed().as_secs_f32();
    println!("{secs:>6.1}s  {:<18} {key}", state_label(e.stage, e.status));
    if let Some(reason) = &e.reason {
        println!("         {}", printable(reason));
    }
}

/// Prints each finished note in the batch; returns whether all finished.
async fn print_notes(queue: &CliQueue, batch: &HashSet<i64>) -> Result<bool, String> {
    let mut all_done = true;
    for job in queue.jobs(100_000).await.map_err(|e| e.to_string())? {
        if !batch.contains(&job.id) {
            continue;
        }
        match job.status {
            Status::Done => println!(
                "note  {}",
                printable(job.note_path.as_deref().unwrap_or(""))
            ),
            _ => all_done = false,
        }
    }
    Ok(all_done)
}

fn is_terminal(status: Status) -> bool {
    matches!(status, Status::Done | Status::Failed | Status::Cancelled)
}

/// Drops jobs the database already shows as finished from `waiting`.
async fn reconcile<F, T>(queue: &Queue<F, T>, waiting: &mut HashSet<i64>) -> Result<(), String>
where
    F: shruti_core::ytdlp::Fetcher + Send + Sync + 'static,
    T: shruti_core::whisper::Transcriber + Send + Sync + 'static,
{
    for job in queue.jobs(100_000).await.map_err(|e| e.to_string())? {
        if waiting.contains(&job.id) && is_terminal(job.status) {
            waiting.remove(&job.id);
        }
    }
    Ok(())
}

async fn run_once(engine: &Engine<YtDlp, WhisperCli>, args: &Args) -> Result<bool, String> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut all_ok = true;
    for link in &args.links {
        all_ok &= process_link(engine, link, &args.out_dir, &today).await;
    }
    Ok(all_ok)
}

/// Returns whether the link produced a note.
async fn process_link(
    engine: &Engine<YtDlp, WhisperCli>,
    link: &str,
    out_dir: &Path,
    today: &str,
) -> bool {
    let started = Instant::now();
    let result = match engine.transcribe(link).await {
        Ok(outcome) => write_note(out_dir, &outcome.meta, &outcome.transcript, today)
            .await
            .map(|path| (outcome, path)),
        Err(e) => Err(e),
    };
    let secs = started.elapsed().as_secs_f32();
    match result {
        Ok((outcome, path)) => {
            println!(
                "ok    {secs:>5.1}s  {:<8}  {}  {}",
                outcome.transcript.source.as_str(),
                outcome.meta.key(),
                printable(&path.display().to_string())
            );
            if let Some(why) = outcome.caption_error {
                println!("      captions skipped: {}", printable(&why));
            }
            true
        }
        Err(e) => {
            println!(
                "fail  {secs:>5.1}s  {}\n      {}",
                printable(link),
                printable(&e.to_string())
            );
            false
        }
    }
}

/// Tool output and pasted links can carry terminal escape sequences; keep
/// only printable text, newlines and tabs.
fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}
