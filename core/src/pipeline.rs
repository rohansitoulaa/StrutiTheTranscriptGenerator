//! One link → one raw transcript: captions when the video has them in its
//! spoken language, otherwise audio → Whisper (PDA §3, TR-1, TR-2).

use std::path::{Path, PathBuf};

use crate::error::fs_err;
use crate::metadata::{CaptionTrack, VideoMeta};
use crate::source::Source;
use crate::transcript::{Transcript, TranscriptSource};
use crate::whisper::Transcriber;
use crate::ytdlp::Fetcher;
use crate::{Error, Result};

/// The canonical URL of a single-video link. Collections are expanded
/// elsewhere (build step 3).
pub fn single_video_url(input: &str) -> Result<String> {
    let source = Source::parse(input)?;
    if !source.is_single_video() {
        return Err(Error::NotYetSupported(source.kind()));
    }
    Ok(source.canonical_url())
}

pub struct Engine<F, T> {
    pub fetcher: F,
    pub transcriber: T,
}

#[derive(Debug)]
pub struct Outcome {
    pub meta: VideoMeta,
    pub transcript: Transcript,
    /// Why captions were skipped when the video had a track for its language
    /// but Whisper was used anyway. Kept so the fallback is visible, not silent.
    pub caption_error: Option<String>,
}

/// What the fetch stage produced.
#[derive(Debug)]
pub enum Fetched {
    /// Usable captions: the transcript is ready, no audio needed.
    Captions(Transcript),
    /// Audio for Whisper, plus why captions weren't used when a track existed.
    Audio {
        path: PathBuf,
        caption_error: Option<String>,
    },
}

impl<F: Fetcher + Sync, T: Transcriber + Sync> Engine<F, T> {
    /// One link start to finish, for the CLI. The queue runs the same stages
    /// one at a time so it can resume between them.
    pub async fn transcribe(&self, input: &str) -> Result<Outcome> {
        let url = single_video_url(input)?;
        let meta = self.metadata(&url).await?;
        let work = tempfile::tempdir()
            .map_err(fs_err("create a scratch folder in", std::env::temp_dir()))?;
        let (transcript, caption_error) = match self.fetch(&meta, &url, work.path()).await? {
            Fetched::Captions(transcript) => (transcript, None),
            Fetched::Audio {
                path,
                caption_error,
            } => (self.transcribe_audio(&meta, &path).await?, caption_error),
        };
        Ok(Outcome {
            meta,
            transcript,
            caption_error,
        })
    }

    pub async fn metadata(&self, url: &str) -> Result<VideoMeta> {
        self.fetcher.metadata(url).await
    }

    /// Captions when the video has a usable track in its spoken language,
    /// otherwise audio downloaded into `dir`.
    pub async fn fetch(&self, meta: &VideoMeta, url: &str, dir: &Path) -> Result<Fetched> {
        let caption_error = match meta.pick_caption() {
            None => None,
            Some(track) => match self.captions(url, track, dir).await {
                Ok(transcript) => return Ok(Fetched::Captions(transcript)),
                Err(reason) => {
                    tracing::warn!(key = %meta.key(), %reason, "captions unusable, using whisper");
                    Some(reason)
                }
            },
        };
        let path = self.fetcher.audio(url, dir).await?;
        Ok(Fetched::Audio {
            path,
            caption_error,
        })
    }

    /// Whisper on downloaded audio. Silence is an error, not an empty note.
    pub async fn transcribe_audio(&self, meta: &VideoMeta, audio: &Path) -> Result<Transcript> {
        let transcript = self
            .transcriber
            .transcribe(audio, meta.spoken_language().as_deref())
            .await?;
        if transcript.is_empty() {
            return Err(Error::Empty(format!(
                "{} has no detectable speech",
                meta.key()
            )));
        }
        Ok(transcript)
    }

    /// The caption track as a transcript, or why it can't be used.
    async fn captions(
        &self,
        url: &str,
        track: CaptionTrack,
        dir: &Path,
    ) -> std::result::Result<Transcript, String> {
        let segments = self
            .fetcher
            .captions(url, &track, dir)
            .await
            .map_err(|e| e.to_string())?;
        if segments.iter().all(|s| s.text.trim().is_empty()) {
            return Err(format!("{} caption track was empty", track.code));
        }
        Ok(Transcript {
            source: TranscriptSource::Captions(track.kind),
            language: Some(track.language),
            segments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{CaptionKind, Platform};
    use crate::transcript::Segment;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Calls(Mutex<Vec<String>>);

    impl Calls {
        fn push(&self, s: impl Into<String>) {
            self.0.lock().unwrap().push(s.into());
        }
        fn all(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeFetcher {
        meta: Result<VideoMeta>,
        captions: fn() -> Result<Vec<Segment>>,
        audio_fails: bool,
        calls: Calls,
    }

    impl Fetcher for FakeFetcher {
        async fn metadata(&self, url: &str) -> Result<VideoMeta> {
            self.calls.push(format!("metadata {url}"));
            match &self.meta {
                Ok(m) => Ok(m.clone()),
                Err(e) => Err(Error::Parse {
                    what: "metadata",
                    reason: e.to_string(),
                }),
            }
        }
        async fn captions(
            &self,
            url: &str,
            track: &CaptionTrack,
            _dir: &Path,
        ) -> Result<Vec<Segment>> {
            self.calls.push(format!("captions {} {url}", track.code));
            (self.captions)()
        }
        async fn audio(&self, url: &str, dir: &Path) -> Result<PathBuf> {
            self.calls.push(format!("audio {url}"));
            if self.audio_fails {
                return Err(Error::ToolFailed {
                    tool: "yt-dlp",
                    status: "exit 1".into(),
                    stderr: "403".into(),
                });
            }
            Ok(dir.join("audio.wav"))
        }
    }

    struct FakeWhisper {
        segments: Vec<Segment>,
        calls: Calls,
    }

    impl Transcriber for FakeWhisper {
        async fn transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
            assert_eq!(
                audio.file_name().unwrap(),
                "audio.wav",
                "whisper must get the downloaded file"
            );
            assert!(
                audio.parent().unwrap().is_dir(),
                "scratch folder deleted before whisper ran"
            );
            self.calls
                .push(format!("whisper {}", language.unwrap_or("auto")));
            Ok(Transcript {
                source: TranscriptSource::Whisper,
                language: Some(language.unwrap_or("en").into()),
                segments: self.segments.clone(),
            })
        }
    }

    fn seg(text: &str) -> Segment {
        Segment {
            start_ms: 0,
            end_ms: 1000,
            text: text.into(),
        }
    }

    fn meta(language: Option<&str>, auto: &[&str]) -> VideoMeta {
        VideoMeta {
            platform: Platform::YouTube,
            id: "hYQEXdm6FBQ".into(),
            url: "https://www.youtube.com/watch?v=hYQEXdm6FBQ".into(),
            title: "t".into(),
            channel: None,
            upload_date: None,
            duration_secs: None,
            language: language.map(Into::into),
            subtitles: vec![],
            auto_captions: auto.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn engine(
        meta: Result<VideoMeta>,
        captions: fn() -> Result<Vec<Segment>>,
    ) -> Engine<FakeFetcher, FakeWhisper> {
        Engine {
            fetcher: FakeFetcher {
                meta,
                captions,
                audio_fails: false,
                calls: Calls::default(),
            },
            transcriber: FakeWhisper {
                segments: vec![seg("from whisper")],
                calls: Calls::default(),
            },
        }
    }

    /// Pasted form; every tool must be handed the canonical form instead.
    const HI_URL: &str = "https://youtu.be/hYQEXdm6FBQ?si=tracking";
    const CANON: &str = "https://www.youtube.com/watch?v=hYQEXdm6FBQ";

    #[tokio::test]
    async fn captions_win_and_whisper_never_runs() {
        let e = engine(Ok(meta(Some("hi"), &["hi-orig"])), || Ok(vec![seg("कैप्शन")]));
        let out = e.transcribe(HI_URL).await.unwrap();
        assert_eq!(
            out.transcript.source,
            TranscriptSource::Captions(CaptionKind::Auto)
        );
        assert_eq!(out.transcript.language.as_deref(), Some("hi"));
        assert_eq!(out.transcript.segments, vec![seg("कैप्शन")]);
        assert_eq!(out.caption_error, None);
        assert_eq!(
            e.fetcher.calls.all(),
            [
                format!("metadata {CANON}"),
                format!("captions hi-orig {CANON}")
            ]
        );
        assert!(e.transcriber.calls.all().is_empty());
    }

    #[tokio::test]
    async fn caption_failure_falls_back_to_whisper_and_says_why() {
        let e = engine(Ok(meta(Some("hi"), &["hi-orig"])), || {
            Err(Error::ToolFailed {
                tool: "yt-dlp",
                status: "exit 1".into(),
                stderr: "HTTP Error 429".into(),
            })
        });
        let out = e.transcribe(HI_URL).await.unwrap();
        assert_eq!(out.transcript.source, TranscriptSource::Whisper);
        assert!(out.caption_error.as_deref().unwrap().contains("429"));
        assert_eq!(e.transcriber.calls.all(), ["whisper hi"]);
        assert_eq!(
            e.fetcher.calls.all().last().unwrap(),
            &format!("audio {CANON}")
        );
    }

    #[tokio::test]
    async fn every_kind_of_caption_failure_falls_back_with_its_reason() {
        fn parse_err() -> Result<Vec<Segment>> {
            Err(Error::Parse {
                what: "captions",
                reason: "truncated".into(),
            })
        }
        fn missing() -> Result<Vec<Segment>> {
            Err(Error::Empty("yt-dlp wrote no hi-orig caption file".into()))
        }
        fn timeout() -> Result<Vec<Segment>> {
            Err(Error::Timeout {
                tool: "yt-dlp",
                secs: 120,
            })
        }
        for (captions, expect) in [
            (parse_err as fn() -> Result<Vec<Segment>>, "truncated"),
            (missing, "no hi-orig caption file"),
            (timeout, "timed out"),
        ] {
            let e = engine(Ok(meta(Some("hi"), &["hi-orig"])), captions);
            let out = e.transcribe(HI_URL).await.unwrap();
            assert_eq!(out.transcript.source, TranscriptSource::Whisper);
            assert!(
                out.caption_error.as_deref().unwrap().contains(expect),
                "{:?}",
                out.caption_error
            );
        }
    }

    #[tokio::test]
    async fn when_captions_and_audio_both_fail_the_audio_error_is_returned() {
        // The caption failure was already logged; the audio error is what
        // actually stopped the job, so that's the reason shown.
        let mut e = engine(Ok(meta(Some("hi"), &["hi-orig"])), || {
            Err(Error::ToolFailed {
                tool: "yt-dlp",
                status: "exit 1".into(),
                stderr: "HTTP Error 429".into(),
            })
        });
        e.fetcher.audio_fails = true;
        let err = e.transcribe(HI_URL).await.unwrap_err();
        assert!(
            matches!(err, Error::ToolFailed { ref stderr, .. } if stderr == "403"),
            "{err:?}"
        );
        assert!(e.transcriber.calls.all().is_empty());
    }

    #[tokio::test]
    async fn empty_caption_track_falls_back_to_whisper() {
        let e = engine(Ok(meta(Some("hi"), &["hi-orig"])), || Ok(vec![seg("  ")]));
        let out = e.transcribe(HI_URL).await.unwrap();
        assert_eq!(out.transcript.source, TranscriptSource::Whisper);
        assert!(out.caption_error.as_deref().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn no_caption_track_goes_straight_to_whisper_with_detection() {
        let e = engine(Ok(meta(None, &[])), || {
            panic!("captions must not be fetched")
        });
        let out = e.transcribe(HI_URL).await.unwrap();
        assert_eq!(out.transcript.source, TranscriptSource::Whisper);
        assert_eq!(out.caption_error, None);
        assert_eq!(e.transcriber.calls.all(), ["whisper auto"]);
    }

    #[tokio::test]
    async fn collections_are_refused_before_any_network_call() {
        let e = engine(Ok(meta(None, &[])), || unreachable!());
        let err = e
            .transcribe("https://www.youtube.com/@kolejain")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotYetSupported("channel")), "{err:?}");
        let err = e
            .transcribe("https://www.instagram.com/natgeo/")
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::NotYetSupported("Instagram profile")),
            "{err:?}"
        );
        assert!(e.fetcher.calls.all().is_empty());
    }

    #[tokio::test]
    async fn junk_input_is_unsupported() {
        let e = engine(Ok(meta(None, &[])), || unreachable!());
        assert!(matches!(
            e.transcribe("hello").await,
            Err(Error::UnsupportedUrl(_))
        ));
    }

    #[tokio::test]
    async fn metadata_and_audio_errors_propagate() {
        let e = engine(Err(Error::Empty("x".into())), || unreachable!());
        assert!(matches!(
            e.transcribe(HI_URL).await,
            Err(Error::Parse {
                what: "metadata",
                ..
            })
        ));

        let mut e = engine(Ok(meta(None, &[])), || unreachable!());
        e.fetcher.audio_fails = true;
        assert!(matches!(
            e.transcribe(HI_URL).await,
            Err(Error::ToolFailed { tool: "yt-dlp", .. })
        ));
        assert!(e.transcriber.calls.all().is_empty());
    }

    #[tokio::test]
    async fn silent_audio_is_an_error_not_an_empty_note() {
        let mut e = engine(Ok(meta(None, &[])), || unreachable!());
        e.transcriber.segments = vec![];
        let err = e.transcribe(HI_URL).await.unwrap_err();
        assert!(matches!(err, Error::Empty(_)), "{err:?}");
        assert!(err.to_string().contains("youtube:hYQEXdm6FBQ"));
    }
}
