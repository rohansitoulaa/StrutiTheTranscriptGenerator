//! yt-dlp: metadata, captions and audio.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::captions::parse_json3;
use crate::error::{fs_err, stderr_tail};
use crate::metadata::{CaptionKind, CaptionTrack, VideoMeta};
use crate::process::{LONG, QUICK, run_tool};
use crate::transcript::Segment;
use crate::{Error, Result};

/// Everything Shruti needs from a video host. [`YtDlp`] is the real one; the
/// pipeline tests use fakes.
pub trait Fetcher {
    fn metadata(&self, url: &str) -> impl Future<Output = Result<VideoMeta>> + Send;

    fn captions(
        &self,
        url: &str,
        track: &CaptionTrack,
        dir: &Path,
    ) -> impl Future<Output = Result<Vec<Segment>>> + Send;

    /// Downloads audio as 16 kHz mono WAV into `dir` and returns its path.
    fn audio(&self, url: &str, dir: &Path) -> impl Future<Output = Result<PathBuf>> + Send;
}

#[derive(Debug, Clone)]
pub struct YtDlp {
    pub bin: PathBuf,
    /// YouTube now needs a JS runtime for its signature challenges; without
    /// one, audio formats go missing. Node is already on this machine.
    pub js_runtime: Option<String>,
    /// Browser to read cookies from (PDA IG-2). Instagram needs it.
    pub cookies_from_browser: Option<String>,
}

const CAPTION_STEM: &str = "captions";
const AUDIO_STEM: &str = "audio";

impl YtDlp {
    fn base_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["--no-warnings".into(), "--no-progress".into()];
        if let Some(rt) = &self.js_runtime {
            args.extend(["--js-runtimes".into(), rt.into()]);
        }
        if let Some(browser) = &self.cookies_from_browser {
            args.extend(["--cookies-from-browser".into(), browser.into()]);
        }
        args
    }

    pub fn metadata_args(&self, url: &str) -> Vec<OsString> {
        let mut args = self.base_args();
        args.extend([
            "-J".into(),
            "--skip-download".into(),
            "--no-playlist".into(),
            url.into(),
        ]);
        args
    }

    /// One exact track code. A pattern like `en.*` would also pull YouTube's
    /// machine translations (`en-de`, …), which got rate-limited in testing.
    pub fn captions_args(
        &self,
        url: &str,
        track: &CaptionTrack,
        out_template: &Path,
    ) -> Vec<OsString> {
        let write = match track.kind {
            CaptionKind::Manual => "--write-subs",
            CaptionKind::Auto => "--write-auto-subs",
        };
        let mut args = self.base_args();
        args.extend([
            "--skip-download".into(),
            write.into(),
            "--sub-langs".into(),
            track.code.clone().into(),
            "--sub-format".into(),
            "json3".into(),
            "--no-playlist".into(),
            "-o".into(),
            out_template.into(),
            url.into(),
        ]);
        args
    }

    pub fn audio_args(&self, url: &str, out_template: &Path) -> Vec<OsString> {
        let mut args = self.base_args();
        args.extend([
            "-f".into(),
            "bestaudio/best".into(),
            "-x".into(),
            "--audio-format".into(),
            "wav".into(),
            "--postprocessor-args".into(),
            "ffmpeg:-ar 16000 -ac 1".into(),
            "--no-playlist".into(),
            "-o".into(),
            out_template.into(),
            url.into(),
        ]);
        args
    }
}

impl Fetcher for YtDlp {
    async fn metadata(&self, url: &str) -> Result<VideoMeta> {
        let out = run_tool("yt-dlp", &self.bin, &self.metadata_args(url), QUICK).await?;
        let json = std::str::from_utf8(&out.stdout).map_err(|e| Error::Parse {
            what: "yt-dlp metadata",
            reason: e.to_string(),
        })?;
        VideoMeta::from_ytdlp_json(json)
    }

    async fn captions(&self, url: &str, track: &CaptionTrack, dir: &Path) -> Result<Vec<Segment>> {
        let template = dir.join(CAPTION_STEM);
        let out = run_tool(
            "yt-dlp",
            &self.bin,
            &self.captions_args(url, track, &template),
            QUICK,
        )
        .await?;
        // yt-dlp exits 0 when a requested track doesn't exist, so a missing
        // file is the failure signal. Its stderr usually says why.
        let path = caption_file(dir, &track.code).ok_or_else(|| {
            Error::Empty(with_stderr(
                format!("yt-dlp wrote no {} caption file", track.code),
                &out.stderr,
            ))
        })?;
        let json = tokio::fs::read_to_string(&path)
            .await
            .map_err(fs_err("read captions", &path))?;
        parse_json3(&json)
    }

    async fn audio(&self, url: &str, dir: &Path) -> Result<PathBuf> {
        let template = dir.join(format!("{AUDIO_STEM}.%(ext)s"));
        let out = run_tool("yt-dlp", &self.bin, &self.audio_args(url, &template), LONG).await?;
        let wav = dir.join(format!("{AUDIO_STEM}.wav"));
        let size = match tokio::fs::metadata(&wav).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Empty(with_stderr(
                    "yt-dlp finished but wrote no audio file".into(),
                    &out.stderr,
                )));
            }
            Err(e) => return Err(fs_err("read", &wav)(e)),
        };
        // A WAV header alone is 44 bytes; anything under a tenth of a second
        // of 16 kHz mono audio means the conversion was cut short.
        if size < MIN_AUDIO_BYTES {
            return Err(Error::Empty(with_stderr(
                format!("audio file is only {size} bytes"),
                &out.stderr,
            )));
        }
        Ok(wav)
    }
}

const MIN_AUDIO_BYTES: u64 = 44 + 3_200;

fn with_stderr(msg: String, stderr: &[u8]) -> String {
    let tail = stderr_tail(stderr, 500);
    if tail.is_empty() {
        msg
    } else {
        format!("{msg}: {tail}")
    }
}

/// yt-dlp names subtitle files `<stem>.<code>.<ext>`.
pub(crate) fn caption_file(dir: &Path, code: &str) -> Option<PathBuf> {
    let path = dir.join(format!("{CAPTION_STEM}.{code}.json3"));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const URL: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";

    fn bare() -> YtDlp {
        YtDlp {
            bin: "yt-dlp".into(),
            js_runtime: None,
            cookies_from_browser: None,
        }
    }

    fn full() -> YtDlp {
        YtDlp {
            js_runtime: Some("node".into()),
            cookies_from_browser: Some("brave".into()),
            ..bare()
        }
    }

    fn strs(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn metadata_args_bare() {
        assert_eq!(
            strs(bare().metadata_args(URL)),
            [
                "--no-warnings",
                "--no-progress",
                "-J",
                "--skip-download",
                "--no-playlist",
                URL
            ]
        );
    }

    #[test]
    fn metadata_args_with_runtime_and_cookies() {
        assert_eq!(
            strs(full().metadata_args(URL)),
            [
                "--no-warnings",
                "--no-progress",
                "--js-runtimes",
                "node",
                "--cookies-from-browser",
                "brave",
                "-J",
                "--skip-download",
                "--no-playlist",
                URL,
            ]
        );
    }

    #[test]
    fn caption_args_request_exactly_one_track() {
        let manual = CaptionTrack {
            code: "en".into(),
            kind: CaptionKind::Manual,
            language: "en".into(),
        };
        assert_eq!(
            strs(bare().captions_args(URL, &manual, Path::new("/t/captions"))),
            [
                "--no-warnings",
                "--no-progress",
                "--skip-download",
                "--write-subs",
                "--sub-langs",
                "en",
                "--sub-format",
                "json3",
                "--no-playlist",
                "-o",
                "/t/captions",
                URL,
            ]
        );
        let auto = CaptionTrack {
            code: "hi-orig".into(),
            kind: CaptionKind::Auto,
            language: "hi".into(),
        };
        let a = strs(bare().captions_args(URL, &auto, Path::new("/t/captions")));
        assert!(a.contains(&"--write-auto-subs".to_string()));
        assert!(!a.contains(&"--write-subs".to_string()));
        assert!(a.windows(2).any(|w| w == ["--sub-langs", "hi-orig"]));
    }

    #[test]
    fn audio_args_produce_16k_mono_wav() {
        assert_eq!(
            strs(bare().audio_args(URL, Path::new("/t/audio.%(ext)s"))),
            [
                "--no-warnings",
                "--no-progress",
                "-f",
                "bestaudio/best",
                "-x",
                "--audio-format",
                "wav",
                "--postprocessor-args",
                "ffmpeg:-ar 16000 -ac 1",
                "--no-playlist",
                "-o",
                "/t/audio.%(ext)s",
                URL,
            ]
        );
    }

    /// A stand-in yt-dlp: a shell script that runs `body`.
    fn fake_ytdlp(dir: &Path, body: &str) -> YtDlp {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("fake-yt-dlp");
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        YtDlp { bin, ..bare() }
    }

    fn hi_orig() -> CaptionTrack {
        CaptionTrack {
            code: "hi-orig".into(),
            kind: CaptionKind::Auto,
            language: "hi".into(),
        }
    }

    #[tokio::test]
    async fn exit_zero_without_a_caption_file_is_an_error_with_ytdlps_reason() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let y = fake_ytdlp(
            tools.path(),
            "echo 'There are no subtitles for the requested languages' >&2; exit 0",
        );
        let err = y.captions(URL, &hi_orig(), work.path()).await.unwrap_err();
        assert!(matches!(err, Error::Empty(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("no subtitles for the requested languages"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn exit_zero_without_audio_or_with_a_stub_is_an_error() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let y = fake_ytdlp(tools.path(), "exit 0");
        let err = y.audio(URL, work.path()).await.unwrap_err();
        assert!(err.to_string().contains("wrote no audio file"), "{err}");

        // Writes a header-only WAV next to wherever -o points.
        let y = fake_ytdlp(
            tools.path(),
            r#"for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
               head -c 44 /dev/zero > "$(dirname "$out")/audio.wav""#,
        );
        let err = y.audio(URL, work.path()).await.unwrap_err();
        assert!(err.to_string().contains("only 44 bytes"), "{err}");
    }

    #[tokio::test]
    async fn captions_written_by_ytdlp_are_parsed() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let y = fake_ytdlp(
            tools.path(),
            r#"for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
               echo '{"events":[{"tStartMs":0,"dDurationMs":900,"segs":[{"utf8":"नमस्ते"}]}]}' > "$out.hi-orig.json3""#,
        );
        let segs = y.captions(URL, &hi_orig(), work.path()).await.unwrap();
        assert_eq!(
            segs,
            vec![Segment {
                start_ms: 0,
                end_ms: 900,
                text: "नमस्ते".into()
            }]
        );
    }

    #[tokio::test]
    async fn invalid_utf8_metadata_is_a_parse_error_not_silent_mojibake() {
        let tools = tempfile::tempdir().unwrap();
        let y = fake_ytdlp(
            tools.path(),
            r#"printf '{"id":"x","title":"\340\244","extractor_key":"Youtube"}'"#,
        );
        let err = y.metadata(URL).await.unwrap_err();
        assert!(
            matches!(
                err,
                Error::Parse {
                    what: "yt-dlp metadata",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn finds_the_caption_file_for_the_requested_code_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("captions.en-de.json3"), "{}").unwrap();
        assert_eq!(caption_file(dir.path(), "en"), None);
        std::fs::write(dir.path().join("captions.en.json3"), "{}").unwrap();
        assert_eq!(
            caption_file(dir.path(), "en"),
            Some(dir.path().join("captions.en.json3"))
        );
    }
}
