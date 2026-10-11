//! Dev CLI until the app's UI exists (build steps 4–5):
//!
//! ```text
//! shruti [--out DIR] [--limit N] <link>…  queue links, show progress, wait for them
//! shruti --list [--limit N] <link>…  count what a paste would add; adds nothing
//! shruti --once [--out DIR] <link>…  one link at a time, no database or LLM
//! shruti --reclean <link>…           re-run cleanup from the stored transcript
//! shruti --test-llm                  list the provider's models, time a request
//! shruti --set-key <provider>        store an API key (read from stdin) in the keyring
//! ```
//!
//! Ctrl-C stops cleanly; the next run resumes where it left off. Warnings
//! (caption fallback, GPU fallback, repaired text) go to stderr.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use shruti_core::Error;
use shruti_core::config::Config;
use shruti_core::db::{self, Added, Status};
use shruti_core::llm::{LlmClient, ProviderKind};
use shruti_core::output::write_note;
use shruti_core::pipeline::Engine;
use shruti_core::queue::{EnqueueReport, JobEvent, Queue, QueueParts, state_label};
use shruti_core::source::Source;
use shruti_core::whisper::WhisperCli;
use shruti_core::ytdlp::YtDlp;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq)]
enum Mode {
    Run,
    Once,
    Reclean,
    List,
    TestLlm,
    SetKey(ProviderKind),
}

struct Args {
    mode: Mode,
    out_dir: PathBuf,
    /// Videos per channel, playlist or profile, at most.
    limit: Option<usize>,
    links: Vec<String>,
}

const USAGE: &str = "usage:
  shruti [--out DIR] [--limit N] <link>…
  shruti --list [--limit N] <link>…
  shruti --once [--out DIR] <link>…
  shruti --reclean <link>…
  shruti --test-llm
  shruti --set-key <lmstudio|openai|gemini|custom>   (key read from stdin)";

type CliQueue = Queue<YtDlp, WhisperCli, LlmClient>;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let Some(args) = parse_args(std::env::args().skip(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match run(args).await {
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
        mode: Mode::Run,
        out_dir: PathBuf::from("out"),
        limit: None,
        links: Vec::new(),
    };
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--out" | "-o" => args.out_dir = raw.next()?.into(),
            "--once" => args.mode = Mode::Once,
            "--reclean" => args.mode = Mode::Reclean,
            "--list" => args.mode = Mode::List,
            "--limit" => args.limit = Some(raw.next()?.parse().ok().filter(|n| *n > 0)?),
            "--test-llm" => args.mode = Mode::TestLlm,
            "--set-key" => args.mode = Mode::SetKey(ProviderKind::parse(&raw.next()?)?),
            "-h" | "--help" => return None,
            _ => args.links.push(arg),
        }
    }
    let needs_links = matches!(
        args.mode,
        Mode::Run | Mode::Once | Mode::Reclean | Mode::List
    );
    (needs_links != args.links.is_empty()).then_some(args)
}

async fn run(args: Args) -> Result<bool, String> {
    let config = load_config()?;
    match args.mode {
        Mode::SetKey(kind) => set_key(kind).await,
        Mode::TestLlm => test_llm(&config).await,
        Mode::Once => run_once(&engine(&config).await?, &args).await,
        Mode::List => list(config, &args).await,
        Mode::Run | Mode::Reclean => run_queue(config, &args).await,
    }
}

fn load_config() -> Result<Config, String> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."));
    Config::from_env(repo_root, |k| std::env::var(k).ok()).map_err(|e| e.to_string())
}

async fn engine(config: &Config) -> Result<Engine<YtDlp, WhisperCli>, String> {
    let problems = config.problems();
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let engine = config.build_engine().await.map_err(|e| e.to_string())?;
    match engine.transcriber.gpu_device {
        Some(n) => eprintln!("whisper device: GPU {n}"),
        None => eprintln!("whisper device: default (no discrete GPU found, slower)"),
    }
    Ok(engine)
}

/// Reads one line from stdin so the key never appears in shell history or
/// the process list.
async fn set_key(kind: ProviderKind) -> Result<bool, String> {
    eprintln!("paste the {} API key, then Enter:", kind.as_str());
    let mut key = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut key)
        .map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || shruti_core::secrets::set_api_key(kind, &key))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    eprintln!("stored in the keyring");
    Ok(true)
}

async fn test_llm(config: &Config) -> Result<bool, String> {
    let llm = config.build_llm().await.map_err(|e| e.to_string())?;
    println!("provider  {}", llm.provider().label());
    match llm.list_models().await {
        Ok(models) => println!("models    {}", models.join(", ")),
        Err(e) => println!("models    {e}"),
    }
    match llm.test().await {
        Ok(took) => {
            println!("request   ok in {:.1}s", took.as_secs_f32());
            Ok(true)
        }
        Err(e) => {
            println!("request   {e}");
            Ok(false)
        }
    }
}

async fn build_queue(config: &Config, args: &Args) -> Result<Arc<CliQueue>, String> {
    let engine = engine(config).await?;
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|e| e.to_string())?;
    let llm = config.build_llm().await.map_err(|e| e.to_string())?;
    eprintln!("llm: {}", llm.provider().label());
    Ok(Queue::new(QueueParts {
        db: pool,
        engine,
        llm,
        cleanup: config.cleanup_settings(),
        work_dir: config.work_dir.clone(),
        out_dir: args.out_dir.clone(),
        limits: config.limits,
        expand_limit: args.limit,
    }))
}

/// Prints what each line would add (PDA IN-4); returns whether all listed.
async fn list(config: Config, args: &Args) -> Result<bool, String> {
    let queue = build_queue(&config, args).await?;
    let mut ok = true;
    for p in queue.preview(&args.links).await {
        match (&p.error, &p.title) {
            (Some(e), _) => {
                println!(
                    "skip  {}
      {}",
                    printable(&p.input),
                    printable(e)
                );
                ok = false;
            }
            (None, Some(title)) => println!(
                "list  {}: {} videos, {} new",
                printable(title),
                p.total,
                p.new
            ),
            (None, None) => {
                let state = if p.new == 1 {
                    "new"
                } else {
                    "already done or queued"
                };
                println!("video {}  ({state})", printable(&p.input));
            }
        }
    }
    Ok(ok)
}

/// Returns whether every link produced a note.
async fn run_queue(config: Config, args: &Args) -> Result<bool, String> {
    let queue = build_queue(&config, args).await?;
    let events = queue.subscribe();
    let (batch, clean_paste) = match args.mode {
        Mode::Reclean => reclean_batch(&queue, &args.links).await?,
        _ => {
            let report = queue
                .enqueue(&args.links)
                .await
                .map_err(|e| e.to_string())?;
            print_report(&report);
            let clean = report.rejected.is_empty() && report.errors.is_empty();
            let ids = report
                .added
                .iter()
                .chain(&report.retried)
                .chain(&report.already_queued)
                .copied()
                .collect();
            (ids, clean)
        }
    };
    if batch.is_empty() {
        return Ok(clean_paste);
    }
    let finished = wait_for_batch(&queue, events, batch.clone()).await?;
    Ok(print_notes(&queue, &batch).await? && finished && clean_paste)
}

async fn reclean_batch(queue: &CliQueue, links: &[String]) -> Result<(HashSet<i64>, bool), String> {
    let mut keys = Vec::new();
    let mut clean = true;
    for link in links {
        match Source::parse(link).ok().and_then(|s| s.video_key()) {
            Some(key) => keys.push(key),
            None => {
                println!("skip  {}\n      not a single-video link", printable(link));
                clean = false;
            }
        }
    }
    let mut ids = HashSet::new();
    for (key, added) in queue.reclean(&keys).await.map_err(|e| e.to_string())? {
        match added {
            Ok(Added::New(id) | Added::AlreadyOpen(id) | Added::Retried(id)) => {
                ids.insert(id);
            }
            Ok(Added::AlreadyDone { .. }) => {}
            Err(e) => {
                println!("skip  {key}\n      {}", printable(&e.to_string()));
                clean = false;
            }
        }
    }
    Ok((ids, clean))
}

fn print_report(report: &EnqueueReport) {
    for c in &report.collections {
        println!(
            "list  {}: {} videos, {} new",
            printable(&c.title),
            c.total,
            c.new
        );
    }
    for (input, reason) in report.rejected.iter().chain(&report.errors) {
        println!("skip  {}\n      {}", printable(input), printable(reason));
    }
    for (input, note) in &report.already_done {
        let note = note.as_deref().unwrap_or("(no note path)");
        println!("done  {}  (earlier: {})", printable(input), printable(note));
    }
}

/// Runs the queue, printing progress, until every job in `waiting` has
/// finished or is waiting on the LLM (true), or the user presses Ctrl-C.
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
                    if settled(e.status) {
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
                "note  {}{}",
                printable(job.note_path.as_deref().unwrap_or("")),
                if job.review { "  (review)" } else { "" }
            ),
            Status::Waiting => {
                all_done = false;
                println!(
                    "wait  {}  {}",
                    job.video_key,
                    printable(job.reason.as_deref().unwrap_or(""))
                );
            }
            _ => all_done = false,
        }
    }
    Ok(all_done)
}

/// Done, failed and cancelled jobs are over; a job waiting on the LLM is
/// parked until it's back, so the CLI stops waiting for it too.
fn settled(status: Status) -> bool {
    matches!(
        status,
        Status::Done | Status::Failed | Status::Cancelled | Status::Waiting
    )
}

/// Drops jobs the database already shows as settled from `waiting`.
async fn reconcile(queue: &CliQueue, waiting: &mut HashSet<i64>) -> Result<(), String> {
    for job in queue.jobs(100_000).await.map_err(|e| e.to_string())? {
        if waiting.contains(&job.id) && settled(job.status) {
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
        Ok(outcome) => write_note(out_dir, &outcome.meta, &outcome.transcript, None, today)
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
