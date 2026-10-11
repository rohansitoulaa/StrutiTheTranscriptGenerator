//! Live end-to-end checks: real yt-dlp, real network, real GPU.
//! Ignored by default; run with `cargo test -p shruti-core -- --ignored`.

use std::path::Path;

use shruti_core::config::Config;
use shruti_core::metadata::CaptionKind;
use shruti_core::pipeline::Engine;
use shruti_core::transcript::TranscriptSource;
use shruti_core::whisper::WhisperCli;
use shruti_core::ytdlp::YtDlp;

async fn engine() -> Engine<YtDlp, WhisperCli> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let config = Config::from_env(root, |k| std::env::var(k).ok()).unwrap();
    assert!(config.problems().is_empty(), "{:?}", config.problems());
    config.build_engine().await.unwrap()
}

#[tokio::test]
#[ignore = "network + GPU"]
async fn picks_the_rtx_4060_not_the_igpu() {
    let e = engine().await;
    assert_eq!(e.transcriber.gpu_device, Some(1));
}

#[tokio::test]
#[ignore = "network"]
async fn english_auto_captions() {
    // "How to think like a GENIUS UI/UX designer": language en-US, en-orig track.
    let out = engine().await.transcribe("https://youtu.be/HE4rLEQpiXY").await.unwrap();
    assert_eq!(out.transcript.source, TranscriptSource::Captions(CaptionKind::Auto));
    assert_eq!(out.transcript.language.as_deref(), Some("en"));
    assert!(out.transcript.text().split_whitespace().count() > 300);
}

#[tokio::test]
#[ignore = "network"]
async fn hindi_auto_captions() {
    let out = engine().await.transcribe("https://www.youtube.com/watch?v=hYQEXdm6FBQ").await.unwrap();
    assert_eq!(out.transcript.source, TranscriptSource::Captions(CaptionKind::Auto));
    assert_eq!(out.transcript.language.as_deref(), Some("hi"));
    let text = out.transcript.text();
    assert!(text.contains("दृश्यम"), "{}", text.chars().take(200).collect::<String>());
    assert!(!text.contains("[संगीत]"));
}

#[tokio::test]
#[ignore = "network + GPU"]
async fn whisper_path_when_language_is_unknown() {
    // "Me at the zoo" has no language field, so it goes through Whisper.
    let out = engine().await.transcribe("https://www.youtube.com/watch?v=jNQXAC9IVRw").await.unwrap();
    assert_eq!(out.transcript.source, TranscriptSource::Whisper);
    assert_eq!(out.transcript.language.as_deref(), Some("en"));
    assert!(out.transcript.text().to_lowercase().contains("elephant"));
}
