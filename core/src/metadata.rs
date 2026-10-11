//! The subset of yt-dlp's `-J` output Shruti uses, plus the caption-track
//! choice (PDA TR-1, TR-4).

use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::BTreeMap;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    YouTube,
    Instagram,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::YouTube => "youtube",
            Platform::Instagram => "instagram",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoMeta {
    pub platform: Platform,
    pub id: String,
    pub url: String,
    pub title: String,
    pub channel: Option<String>,
    /// `YYYY-MM-DD`.
    pub upload_date: Option<String>,
    pub duration_secs: Option<u64>,
    /// As reported by the platform, e.g. `en-US`. See [`VideoMeta::spoken_language`].
    pub language: Option<String>,
    /// Manually uploaded subtitle tracks, by language code.
    pub subtitles: Vec<String>,
    /// Auto-generated tracks, including YouTube's machine translations.
    pub auto_captions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionKind {
    Manual,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionTrack {
    /// The exact track code to pass to `--sub-langs`.
    pub code: String,
    pub kind: CaptionKind,
    /// Primary language subtag, e.g. `en`.
    pub language: String,
}

#[derive(Deserialize)]
struct RawMeta {
    id: Option<String>,
    title: Option<String>,
    extractor_key: Option<String>,
    webpage_url: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
    uploader_id: Option<String>,
    upload_date: Option<String>,
    duration: Option<f64>,
    language: Option<String>,
    #[serde(default)]
    subtitles: BTreeMap<String, IgnoredAny>,
    #[serde(default)]
    automatic_captions: BTreeMap<String, IgnoredAny>,
}

const ORIG_SUFFIX: &str = "-orig";

impl VideoMeta {
    pub fn from_ytdlp_json(json: &str) -> Result<Self> {
        let parse_err = |reason: String| Error::Parse {
            what: "yt-dlp metadata",
            reason,
        };
        let raw: RawMeta = serde_json::from_str(json).map_err(|e| parse_err(e.to_string()))?;

        let id = raw
            .id
            .filter(|s| !s.is_empty())
            .ok_or_else(|| parse_err("no video id".into()))?;
        let extractor = raw.extractor_key.unwrap_or_default();
        let platform = if extractor == "Youtube" {
            Platform::YouTube
        } else if extractor.starts_with("Instagram") {
            Platform::Instagram
        } else {
            return Err(parse_err(format!("unexpected extractor {extractor:?}")));
        };

        let title = non_empty(raw.title);
        if title.is_none() {
            tracing::warn!(id, "yt-dlp returned no title");
        }
        Ok(VideoMeta {
            platform,
            url: raw.webpage_url.unwrap_or_else(|| match platform {
                Platform::YouTube => format!("https://www.youtube.com/watch?v={id}"),
                Platform::Instagram => format!("https://www.instagram.com/reel/{id}/"),
            }),
            title: title.unwrap_or_default(),
            channel: non_empty(raw.channel)
                .or(non_empty(raw.uploader))
                .or(non_empty(raw.uploader_id)),
            upload_date: raw
                .upload_date
                .as_deref()
                .and_then(|d| checked_date(&id, d)),
            duration_secs: raw
                .duration
                .filter(|d| d.is_finite() && *d >= 0.0)
                .map(|d| d.round() as u64),
            language: non_empty(raw.language).filter(|l| checked_language(&id, l)),
            subtitles: raw
                .subtitles
                .into_keys()
                .filter(|k| k != "live_chat" && track_code_ok(&id, k))
                .collect(),
            auto_captions: raw
                .automatic_captions
                .into_keys()
                .filter(|k| track_code_ok(&id, k))
                .collect(),
            id,
        })
    }

    /// Dedupe key, `platform:id` (PDA DB-2).
    pub fn key(&self) -> String {
        format!("{}:{}", self.platform.as_str(), self.id)
    }

    /// Primary language subtag of what is spoken: the platform's language
    /// field, else YouTube's `<lang>-orig` auto track (the speech-recognition
    /// track of the original audio).
    pub fn spoken_language(&self) -> Option<String> {
        self.language.as_deref().and_then(language_tag).or_else(|| {
            self.auto_captions
                .iter()
                .find_map(|code| code.strip_suffix(ORIG_SUFFIX))
                .and_then(language_tag)
        })
    }

    /// Captions in the spoken language: manual first, then YouTube's own
    /// speech recognition. `None` when the language is unknown, so the caller
    /// uses Whisper rather than guessing a track (a guess can pick a
    /// machine-translated one).
    pub fn pick_caption(&self) -> Option<CaptionTrack> {
        let lang = self.spoken_language()?;
        let track = |code: &str, kind| CaptionTrack {
            code: code.to_string(),
            kind,
            language: lang.clone(),
        };

        let manual = self.subtitles.iter().find(|c| **c == lang).or_else(|| {
            self.subtitles
                .iter()
                .find(|c| is_regional_variant(c, &lang))
        });
        if let Some(code) = manual {
            return Some(track(code, CaptionKind::Manual));
        }

        let orig = format!("{lang}{ORIG_SUFFIX}");
        self.auto_captions
            .iter()
            .find(|c| **c == orig)
            .or_else(|| self.auto_captions.iter().find(|c| **c == lang))
            .map(|code| track(code, CaptionKind::Auto))
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

fn checked_date(id: &str, raw_date: &str) -> Option<String> {
    let date = iso_date(raw_date);
    if date.is_none() {
        tracing::warn!(id, raw_date, "unrecognised upload_date, dropping it");
    }
    date
}

// Language and track codes come from untrusted JSON and end up in
// frontmatter, file names and yt-dlp arguments: keep only the shapes real
// codes have, and log what's dropped.

fn checked_language(id: &str, language: &str) -> bool {
    let ok = language_tag(language).is_some();
    if !ok {
        tracing::warn!(id, language, "unrecognised language code, ignoring it");
    }
    ok
}

/// Primary language subtag of a code like `en-US`, `hi` or `zh_Hans`, or
/// `None` if it isn't shaped like one (2–3 ASCII letters). Also applied to
/// Whisper's detected language before it reaches frontmatter.
pub fn language_tag(code: &str) -> Option<String> {
    let primary = code.split(['-', '_']).next()?;
    ((2..=3).contains(&primary.len()) && primary.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| primary.to_ascii_lowercase())
}

/// `en-GB`, `es-419`, `zh-Hans` are variants of their language; `en-de` (a
/// lowercase second part) is YouTube's naming for a machine translation.
fn is_regional_variant(code: &str, lang: &str) -> bool {
    let Some(sub) = code
        .strip_prefix(lang)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    let region = sub.len() == 2 && sub.bytes().all(|b| b.is_ascii_uppercase());
    let numeric_region = sub.len() == 3 && sub.bytes().all(|b| b.is_ascii_digit());
    let script = sub.len() == 4
        && sub.as_bytes()[0].is_ascii_uppercase()
        && sub.bytes().skip(1).all(|b| b.is_ascii_lowercase());
    region || numeric_region || script
}

/// [`is_track_code`], logging what it drops: a dropped real track would
/// otherwise turn into a silent "no captions, used Whisper".
fn track_code_ok(id: &str, code: &str) -> bool {
    let ok = is_track_code(code);
    if !ok {
        tracing::warn!(id, code, "ignoring unrecognised caption track code");
    }
    ok
}

/// Track codes are passed to yt-dlp and used in file names: ASCII letters,
/// digits and `-`/`_`, not starting with `-`.
fn is_track_code(code: &str) -> bool {
    (1..=32).contains(&code.len())
        && !code.starts_with('-')
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// yt-dlp's `YYYYMMDD` → `YYYY-MM-DD`.
fn iso_date(raw: &str) -> Option<String> {
    (raw.len() == 8 && raw.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{}-{}-{}", &raw[0..4], &raw[4..6], &raw[6..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const EN_MANUAL: &str = include_str!("../tests/fixtures/ytdlp_meta_en_manual.json");
    const HI_AUTO: &str = include_str!("../tests/fixtures/ytdlp_meta_hi_auto.json");
    const EN_US_AUTO: &str = include_str!("../tests/fixtures/ytdlp_meta_en_us_auto.json");

    fn meta_with(language: &str, subs: &[&str], auto: &[&str]) -> VideoMeta {
        let tracks = |codes: &[&str]| -> serde_json::Value {
            codes
                .iter()
                .map(|c| (c.to_string(), serde_json::json!([{"ext": "json3"}])))
                .collect()
        };
        let json = serde_json::json!({
            "id": "abcdefghijk", "title": "t", "extractor_key": "Youtube",
            "webpage_url": "https://www.youtube.com/watch?v=abcdefghijk",
            "language": language, "subtitles": tracks(subs), "automatic_captions": tracks(auto),
        });
        VideoMeta::from_ytdlp_json(&json.to_string()).unwrap()
    }

    #[test]
    fn parses_youtube_metadata() {
        let m = VideoMeta::from_ytdlp_json(EN_MANUAL).unwrap();
        assert_eq!(m.platform, Platform::YouTube);
        assert_eq!(m.id, "jNQXAC9IVRw");
        assert_eq!(m.title, "Me at the zoo");
        assert_eq!(m.channel.as_deref(), Some("jawed"));
        assert_eq!(m.upload_date.as_deref(), Some("2005-04-24"));
        assert_eq!(m.duration_secs, Some(19));
        assert_eq!(m.url, "https://www.youtube.com/watch?v=jNQXAC9IVRw");
        assert_eq!(m.key(), "youtube:jNQXAC9IVRw");
        assert_eq!(m.subtitles, vec!["de", "en"]);
    }

    #[test]
    fn unknown_language_means_no_caption_guess() {
        let m = VideoMeta::from_ytdlp_json(EN_MANUAL).unwrap();
        assert_eq!(m.language, None);
        assert_eq!(m.spoken_language(), None);
        assert_eq!(m.pick_caption(), None);
    }

    #[test]
    fn hindi_uses_the_original_asr_track() {
        let m = VideoMeta::from_ytdlp_json(HI_AUTO).unwrap();
        assert_eq!(m.spoken_language().as_deref(), Some("hi"));
        assert_eq!(
            m.pick_caption(),
            Some(CaptionTrack {
                code: "hi-orig".into(),
                kind: CaptionKind::Auto,
                language: "hi".into()
            })
        );
    }

    #[test]
    fn regional_language_codes_are_normalised() {
        let m = VideoMeta::from_ytdlp_json(EN_US_AUTO).unwrap();
        assert_eq!(m.language.as_deref(), Some("en-US"));
        assert_eq!(m.spoken_language().as_deref(), Some("en"));
        assert_eq!(m.pick_caption().unwrap().code, "en-orig");
    }

    #[test]
    fn manual_beats_auto_and_exact_beats_regional() {
        let m = meta_with("en", &["de", "en-GB", "en"], &["en-orig"]);
        assert_eq!(
            m.pick_caption(),
            Some(CaptionTrack {
                code: "en".into(),
                kind: CaptionKind::Manual,
                language: "en".into()
            })
        );
        let m = meta_with("en", &["de", "en-GB"], &["en-orig"]);
        assert_eq!(m.pick_caption().unwrap().code, "en-GB");
        assert_eq!(m.pick_caption().unwrap().kind, CaptionKind::Manual);
    }

    #[test]
    fn translation_shaped_codes_are_not_regional_variants() {
        assert_eq!(meta_with("en", &["en-de"], &[]).pick_caption(), None);
        assert_eq!(
            meta_with("es", &["es-419"], &[])
                .pick_caption()
                .unwrap()
                .code,
            "es-419"
        );
        assert_eq!(
            meta_with("zh", &["zh-Hans"], &[])
                .pick_caption()
                .unwrap()
                .code,
            "zh-Hans"
        );
    }

    #[test]
    fn hostile_language_values_never_reach_the_note() {
        let m = meta_with("en\ntitle: \"CLICK HERE\"", &["en"], &[]);
        assert_eq!(m.language, None);
        assert_eq!(m.spoken_language(), None);
        assert_eq!(language_tag("en-US").as_deref(), Some("en"));
        assert_eq!(language_tag("fil").as_deref(), Some("fil"));
        assert_eq!(language_tag("e1"), None);
        assert_eq!(language_tag(""), None);
    }

    #[test]
    fn hostile_track_codes_are_dropped() {
        let m = meta_with(
            "en",
            &["../../etc/passwd", "--exec", "en"],
            &["en-orig", "a b"],
        );
        assert_eq!(m.subtitles, vec!["en"]);
        assert_eq!(m.auto_captions, vec!["en-orig"]);
    }

    #[test]
    fn auto_without_orig_falls_back_to_plain_code() {
        let m = meta_with("hi", &[], &["en", "hi"]);
        assert_eq!(m.pick_caption().unwrap().code, "hi");
    }

    #[test]
    fn no_track_in_the_spoken_language_means_whisper() {
        let m = meta_with("hi", &["en"], &["en", "fr"]);
        assert_eq!(m.pick_caption(), None);
    }

    #[test]
    fn language_falls_back_to_the_orig_track() {
        let json = serde_json::json!({
            "id": "abcdefghijk", "title": "t", "extractor_key": "Youtube",
            "automatic_captions": {"en": [], "hi-orig": [], "hi": []},
        });
        let m = VideoMeta::from_ytdlp_json(&json.to_string()).unwrap();
        assert_eq!(m.spoken_language().as_deref(), Some("hi"));
        assert_eq!(m.pick_caption().unwrap().code, "hi-orig");
    }

    #[test]
    fn live_chat_is_not_a_subtitle_track() {
        let m = meta_with("en", &["live_chat"], &[]);
        assert!(m.subtitles.is_empty());
        assert_eq!(m.pick_caption(), None);
    }

    #[test]
    fn instagram_reel_metadata() {
        let json = serde_json::json!({
            "id": "DPx0yXkkS5M", "title": "Video by natgeo", "extractor_key": "Instagram",
            "channel": null, "uploader": "National Geographic", "uploader_id": "natgeo",
            "duration": 30.5, "upload_date": "20261001",
            "webpage_url": "https://www.instagram.com/reel/DPx0yXkkS5M/",
        });
        let m = VideoMeta::from_ytdlp_json(&json.to_string()).unwrap();
        assert_eq!(m.platform, Platform::Instagram);
        assert_eq!(m.key(), "instagram:DPx0yXkkS5M");
        assert_eq!(m.channel.as_deref(), Some("National Geographic"));
        assert_eq!(m.duration_secs, Some(31));
        assert_eq!(m.upload_date.as_deref(), Some("2026-10-01"));
        assert_eq!(m.pick_caption(), None);
    }

    #[test]
    fn malformed_dates_are_dropped_not_fatal() {
        let json = serde_json::json!({
            "id": "abcdefghijk", "title": "t", "extractor_key": "Youtube", "upload_date": "2026-1",
        });
        assert_eq!(
            VideoMeta::from_ytdlp_json(&json.to_string())
                .unwrap()
                .upload_date,
            None
        );
    }

    #[test]
    fn rejects_garbage_and_missing_ids() {
        assert!(matches!(
            VideoMeta::from_ytdlp_json("not json"),
            Err(Error::Parse { .. })
        ));
        assert!(matches!(
            VideoMeta::from_ytdlp_json(r#"{"title":"x","extractor_key":"Youtube"}"#),
            Err(Error::Parse { .. })
        ));
        assert!(matches!(
            VideoMeta::from_ytdlp_json(r#"{"id":"x","title":"x","extractor_key":"Vimeo"}"#),
            Err(Error::Parse { .. })
        ));
    }
}
