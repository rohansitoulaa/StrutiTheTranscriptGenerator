//! Dev CLI for build step 1: `shruti [--out DIR] <link>…`
//!
//! Writes one note per link. A failing link is reported and the rest carry on
//! (PDA Q-3); the exit code is non-zero if any failed. Warnings (caption
//! fallback, GPU fallback, repaired text) go to stderr.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use shruti_core::config::Config;
use shruti_core::output::write_note;
use shruti_core::pipeline::Engine;
use shruti_core::whisper::WhisperCli;
use shruti_core::ytdlp::YtDlp;

struct Args {
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
        eprintln!("usage: shruti [--out DIR] <link>…");
        return ExitCode::from(2);
    };
    let engine = match setup().await {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("{}", printable(&msg));
            return ExitCode::FAILURE;
        }
    };

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut failed = 0;
    for link in &args.links {
        if !process_link(&engine, link, &args.out_dir, &today).await {
            failed += 1;
        }
    }
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn parse_args(mut raw: impl Iterator<Item = String>) -> Option<Args> {
    let mut args = Args { out_dir: PathBuf::from("out"), links: Vec::new() };
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--out" | "-o" => args.out_dir = raw.next()?.into(),
            "-h" | "--help" => return None,
            _ => args.links.push(arg),
        }
    }
    (!args.links.is_empty()).then_some(args)
}

async fn setup() -> Result<Engine<YtDlp, WhisperCli>, String> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap_or(Path::new("."));
    let config = Config::from_env(repo_root, |k| std::env::var(k).ok()).map_err(|e| e.to_string())?;
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

/// Returns whether the link produced a note.
async fn process_link(engine: &Engine<YtDlp, WhisperCli>, link: &str, out_dir: &Path, today: &str) -> bool {
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
            println!("fail  {secs:>5.1}s  {}\n      {}", printable(link), printable(&e.to_string()));
            false
        }
    }
}

/// Tool output and pasted links can carry terminal escape sequences; keep
/// only printable text, newlines and tabs.
fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}
