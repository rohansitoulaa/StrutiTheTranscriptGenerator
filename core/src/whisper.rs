//! whisper.cpp's `whisper-cli`, run as a subprocess (TECH-STACK: start with
//! the sidecar; `whisper-rs` can replace it behind [`Transcriber`] later).

use serde::Deserialize;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::captions::strip_annotations;
use crate::error::fs_err;
use crate::metadata::language_tag;
use crate::process::{LONG, QUICK, run_tool};
use crate::transcript::{Segment, Transcript, TranscriptSource};
use crate::{Error, Result};

pub trait Transcriber {
    /// `language` is the primary subtag when already known from metadata;
    /// `None` lets the model detect it.
    fn transcribe(
        &self,
        audio: &Path,
        language: Option<&str>,
    ) -> impl Future<Output = Result<Transcript>> + Send;
}

#[derive(Debug, Clone)]
pub struct WhisperCli {
    pub bin: PathBuf,
    pub model: PathBuf,
    /// Silero VAD model. Measured on music-backed Hindi: roughly doubles the
    /// speech captured versus no VAD.
    pub vad_model: Option<PathBuf>,
    /// whisper.cpp GPU index. `None` means device 0, which on this laptop is
    /// the Intel iGPU, so callers should set it from [`WhisperCli::detect_discrete_gpu`].
    pub gpu_device: Option<u32>,
    pub threads: Option<u32>,
}

impl WhisperCli {
    /// Arguments for one run. `-nf` disables temperature fallback: measured
    /// 5× faster on music-heavy audio with the same output once VAD is on.
    pub fn args(&self, audio: &Path, out_base: &Path, language: Option<&str>) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "-m".into(),
            self.model.clone().into(),
            "-f".into(),
            audio.into(),
            "-l".into(),
            language.unwrap_or("auto").into(),
            "-oj".into(),
            "-of".into(),
            out_base.into(),
            "-np".into(),
            "-nf".into(),
        ];
        if let Some(vad) = &self.vad_model {
            args.extend(["--vad".into(), "--vad-model".into(), vad.clone().into()]);
        }
        if let Some(dev) = self.gpu_device {
            args.extend(["-dev".into(), dev.to_string().into()]);
        }
        if let Some(t) = self.threads {
            args.extend(["-t".into(), t.to_string().into()]);
        }
        args
    }

    /// Index of the first discrete (non-UMA) Vulkan GPU, found by running
    /// whisper-cli once on a second of silence and reading its device list.
    pub async fn detect_discrete_gpu(&self) -> Result<Option<u32>> {
        let dir = tempfile::tempdir()
            .map_err(fs_err("create a scratch folder in", std::env::temp_dir()))?;
        let wav = dir.path().join("silence.wav");
        tokio::fs::write(&wav, silent_wav(1_000))
            .await
            .map_err(fs_err("write", &wav))?;
        // `-dl` exits right after language detection; device enumeration has
        // already been logged to stderr by then. No `-np`: it would hide it.
        let args: Vec<OsString> = vec![
            "-m".into(),
            self.model.clone().into(),
            "-f".into(),
            wav.into(),
            "-dl".into(),
        ];
        let out = run_tool("whisper-cli", &self.bin, &args, QUICK).await?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let device = pick_discrete_gpu(&stderr);
        if device.is_none() {
            // Falling back is legitimate (no discrete GPU), but it's also what
            // a changed ggml log format looks like, so say which one it was.
            let listed = stderr.lines().filter(|l| device_line(l).is_some()).count();
            if listed == 0 {
                tracing::warn!(
                    "whisper-cli listed no Vulkan devices; transcription will use its default (CPU or iGPU) and be several times slower"
                );
            } else {
                tracing::warn!(
                    listed,
                    "no discrete GPU among the Vulkan devices; using the default device, which is slower"
                );
            }
            tracing::debug!(%stderr, "whisper-cli device probe output");
        }
        Ok(device)
    }
}

impl Transcriber for WhisperCli {
    async fn transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
        let dir = tempfile::tempdir()
            .map_err(fs_err("create a scratch folder in", std::env::temp_dir()))?;
        let out_base = dir.path().join("whisper");
        run_tool(
            "whisper-cli",
            &self.bin,
            &self.args(audio, &out_base, language),
            LONG,
        )
        .await?;

        let json_path = out_base.with_extension("json");
        let bytes = tokio::fs::read(&json_path)
            .await
            .map_err(fs_err("read whisper output", &json_path))?;
        let (detected, segments) = parse_whisper_json(&bytes)?;
        Ok(Transcript {
            source: TranscriptSource::Whisper,
            language: detected.or_else(|| language.and_then(language_tag)),
            segments,
        })
    }
}

#[derive(Deserialize)]
struct WhisperJson {
    #[serde(default)]
    result: Option<WhisperResult>,
    transcription: Vec<WhisperSegment>,
}

#[derive(Deserialize)]
struct WhisperResult {
    language: Option<String>,
}

#[derive(Deserialize)]
struct WhisperSegment {
    offsets: Offsets,
    text: String,
}

#[derive(Deserialize)]
struct Offsets {
    from: u64,
    to: u64,
}

/// whisper.cpp's `-oj` output. It can split a multi-byte character across two
/// tokens and write invalid UTF-8, so bytes are decoded lossily.
pub fn parse_whisper_json(bytes: &[u8]) -> Result<(Option<String>, Vec<Segment>)> {
    let text = String::from_utf8_lossy(bytes);
    let repaired = matches!(text, std::borrow::Cow::Owned(_));
    let doc: WhisperJson = serde_json::from_str(&text).map_err(|e| Error::Parse {
        what: "whisper output",
        reason: e.to_string(),
    })?;

    let mut dropped_fragments = 0;
    let segments = doc
        .transcription
        .into_iter()
        .filter_map(|s| {
            // A replacement character marks half of a character whisper split
            // across tokens; the visible "�" would end up in the note.
            dropped_fragments += s.text.matches('\u{FFFD}').count();
            let without_fragments = s.text.replace('\u{FFFD}', "");
            // `[BLANK_AUDIO]` and similar are whisper's non-speech markers.
            let text = strip_annotations(&without_fragments);
            (!text.is_empty()).then_some(Segment {
                start_ms: s.offsets.from,
                end_ms: s.offsets.to,
                text,
            })
        })
        .collect();
    if repaired {
        tracing::warn!(
            dropped_fragments,
            "whisper output had invalid UTF-8; broken characters were dropped"
        );
    }
    let reported = doc
        .result
        .and_then(|r| r.language)
        .filter(|l| !l.is_empty());
    let language = reported.as_deref().and_then(language_tag);
    if let (Some(raw), None) = (&reported, &language) {
        tracing::warn!(
            language = raw.as_str(),
            "whisper reported an unrecognised language code, ignoring it"
        );
    }
    Ok((language, segments))
}

/// From ggml's device list on stderr, the first device with `uma: 0`.
pub fn pick_discrete_gpu(stderr: &str) -> Option<u32> {
    stderr.lines().find_map(|line| {
        let (index, desc) = device_line(line)?;
        let discrete = desc.split('|').any(|field| field.trim() == "uma: 0");
        discrete.then_some(index)
    })
}

/// `ggml_vulkan: 1 = NVIDIA GeForce RTX 4060 … | uma: 0 | fp16: 1 | …`
fn device_line(line: &str) -> Option<(u32, &str)> {
    let rest = line.trim().strip_prefix("ggml_vulkan: ")?;
    let (index, desc) = rest.split_once(" = ")?;
    Some((index.trim().parse().ok()?, desc))
}

const SAMPLE_RATE: u32 = 16_000;

/// 16 kHz mono 16-bit PCM silence. `u16` milliseconds keeps every size in
/// range of the u32 WAV header fields.
pub(crate) fn silent_wav(ms: u16) -> Vec<u8> {
    let data_len = SAMPLE_RATE / 1_000 * u32::from(ms) * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const EN: &[u8] = include_bytes!("../tests/fixtures/whisper_en.json");
    const HI_BAD: &[u8] = include_bytes!("../tests/fixtures/whisper_hi_bad_utf8.json");
    const DEVICES: &str = include_str!("../tests/fixtures/whisper_stderr_devices.txt");

    fn cli() -> WhisperCli {
        WhisperCli {
            bin: "whisper-cli".into(),
            model: "/m/model.bin".into(),
            vad_model: None,
            gpu_device: None,
            threads: None,
        }
    }

    fn strs(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn parses_real_whisper_output() {
        let (lang, segs) = parse_whisper_json(EN).unwrap();
        assert_eq!(lang.as_deref(), Some("en"));
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].start_ms, 0);
        assert_eq!(segs[0].end_ms, 4000);
        assert!(segs[0].text.starts_with("Alright"), "{:?}", segs[0].text);
        assert_eq!(segs[0].text, segs[0].text.trim());
        assert!(segs.iter().any(|s| s.text.contains("elephants")));
    }

    #[test]
    fn survives_invalid_utf8_from_split_devanagari() {
        let (lang, segs) = parse_whisper_json(HI_BAD).unwrap();
        assert_eq!(lang.as_deref(), Some("hi"));
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "तो दोस्तों");
        // The split second character is dropped; no "�" reaches the note.
        assert_eq!(segs[1].text, "इ ok");
    }

    #[test]
    fn hostile_detected_language_is_ignored() {
        let json = br#"{"result":{"language":"en\nx: y"},"transcription":[
            {"offsets":{"from":0,"to":1},"text":"hi"}]}"#;
        assert_eq!(parse_whisper_json(json).unwrap().0, None);
    }

    #[test]
    fn drops_blank_audio_markers() {
        let json = br#"{"result":{"language":"en"},"transcription":[
            {"offsets":{"from":0,"to":1000},"text":" [BLANK_AUDIO]"},
            {"offsets":{"from":1000,"to":2000},"text":" Hello."}]}"#;
        let (_, segs) = parse_whisper_json(json).unwrap();
        assert_eq!(
            segs,
            vec![Segment {
                start_ms: 1000,
                end_ms: 2000,
                text: "Hello.".into()
            }]
        );
    }

    #[test]
    fn rejects_output_without_transcription() {
        assert!(matches!(
            parse_whisper_json(b"{}"),
            Err(Error::Parse { .. })
        ));
        assert!(matches!(
            parse_whisper_json(b"nope"),
            Err(Error::Parse { .. })
        ));
    }

    #[test]
    fn picks_the_nvidia_card_not_the_igpu() {
        assert_eq!(pick_discrete_gpu(DEVICES), Some(1));
    }

    #[test]
    fn integrated_only_or_no_list_means_default() {
        let igpu_only = "ggml_vulkan: Found 1 Vulkan devices:\n\
            ggml_vulkan: 0 = Intel(R) Arc(tm) Graphics (MTL) | uma: 1 | fp16: 1";
        assert_eq!(pick_discrete_gpu(igpu_only), None);
        assert_eq!(pick_discrete_gpu(""), None);
        assert_eq!(
            pick_discrete_gpu("ggml_vulkan: 0 = NVIDIA X | uma: 0 | fp16: 1"),
            Some(0)
        );
    }

    #[test]
    fn args_minimal() {
        let a = strs(cli().args(Path::new("/a/in.wav"), Path::new("/t/out"), None));
        assert_eq!(
            a,
            [
                "-m",
                "/m/model.bin",
                "-f",
                "/a/in.wav",
                "-l",
                "auto",
                "-oj",
                "-of",
                "/t/out",
                "-np",
                "-nf"
            ]
        );
    }

    #[test]
    fn args_with_vad_gpu_threads_and_language() {
        let w = WhisperCli {
            vad_model: Some("/m/vad.bin".into()),
            gpu_device: Some(1),
            threads: Some(8),
            ..cli()
        };
        let a = strs(w.args(Path::new("/a/in.wav"), Path::new("/t/out"), Some("hi")));
        assert_eq!(
            a,
            [
                "-m",
                "/m/model.bin",
                "-f",
                "/a/in.wav",
                "-l",
                "hi",
                "-oj",
                "-of",
                "/t/out",
                "-np",
                "-nf",
                "--vad",
                "--vad-model",
                "/m/vad.bin",
                "-dev",
                "1",
                "-t",
                "8",
            ]
        );
    }

    #[test]
    fn silent_wav_header() {
        let wav = silent_wav(1000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 32_000);
        assert_eq!(wav.len(), 44 + 32_000);
    }
}
