//! YouTube `json3` captions → segments.
//!
//! json3 is used for both manual and auto tracks. Unlike VTT, YouTube's
//! rolling auto-captions arrive in it without the repeated-line duplication,
//! so no de-duplication pass is needed.

use serde::Deserialize;

use crate::transcript::Segment;
use crate::{Error, Result};

#[derive(Deserialize)]
struct Json3 {
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    #[serde(default)]
    t_start_ms: u64,
    #[serde(default)]
    d_duration_ms: u64,
    #[serde(default)]
    a_append: u8,
    #[serde(default)]
    segs: Vec<Seg>,
}

#[derive(Deserialize)]
struct Seg {
    #[serde(default)]
    utf8: String,
}

pub fn parse_json3(json: &str) -> Result<Vec<Segment>> {
    let doc: Json3 = serde_json::from_str(json).map_err(|e| Error::Parse {
        what: "captions",
        reason: e.to_string(),
    })?;

    Ok(doc
        .events
        .into_iter()
        // Append events only carry the line break of a rolling caption.
        .filter(|e| e.a_append == 0 && !e.segs.is_empty())
        .filter_map(|e| {
            let joined: String = e.segs.iter().map(|s| s.utf8.as_str()).collect();
            let text = strip_annotations(&joined);
            (!text.is_empty()).then(|| Segment {
                start_ms: e.t_start_ms,
                end_ms: e.t_start_ms.saturating_add(e.d_duration_ms),
                text,
            })
        })
        .collect())
}

/// Drop non-speech annotations: `[Music]`, `[संगीत]`, `[Applause]`, `♪`.
/// Parenthesised text is kept, since manual captions use it for real speech.
pub(crate) fn strip_annotations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        match rest[open..].find(']') {
            Some(close) => {
                out.push_str(&rest[..open]);
                out.push(' ');
                rest = &rest[open + close + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out.replace('♪', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const HI: &str = include_str!("../tests/fixtures/captions_hi.json3");

    #[test]
    fn parses_real_hindi_auto_captions() {
        let segs = parse_json3(HI).unwrap();
        assert!(segs.len() >= 5, "got {}", segs.len());
        assert_eq!(segs[0].start_ms, 160);
        assert_eq!(segs[0].end_ms, 160 + 4320);
        assert_eq!(segs[0].text, "तो दोस्तों, इंडिया की सबसे बड़ी सस्पेंस");
        for s in &segs {
            assert!(!s.text.contains('\n'), "{:?}", s.text);
            assert!(!s.text.contains('['), "{:?}", s.text);
            assert!(!s.text.trim().is_empty());
            assert!(s.end_ms >= s.start_ms);
        }
    }

    #[test]
    fn skips_annotation_only_and_empty_events() {
        let json = serde_json::json!({"events": [
            {"tStartMs": 0, "dDurationMs": 1000},
            {"tStartMs": 0, "dDurationMs": 900, "segs": [{"utf8": "[Music]"}]},
            {"tStartMs": 1000, "dDurationMs": 500, "aAppend": 1, "segs": [{"utf8": "\n"}]},
            {"tStartMs": 1000, "dDurationMs": 2000, "segs": [
                {"utf8": "hello"}, {"utf8": "  world\n", "tOffsetMs": 300}, {"utf8": "[Applause]"}
            ]},
            {"tStartMs": 3000, "dDurationMs": 1000, "segs": [{"utf8": "♪ la la ♪"}]},
        ]});
        let segs = parse_json3(&json.to_string()).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment {
                    start_ms: 1000,
                    end_ms: 3000,
                    text: "hello world".into()
                },
                Segment {
                    start_ms: 3000,
                    end_ms: 4000,
                    text: "la la".into()
                },
            ]
        );
    }

    #[test]
    fn absurd_durations_do_not_overflow() {
        let json = format!(
            r#"{{"events":[{{"tStartMs":5,"dDurationMs":{},"segs":[{{"utf8":"x"}}]}}]}}"#,
            u64::MAX
        );
        assert_eq!(parse_json3(&json).unwrap()[0].end_ms, u64::MAX);
    }

    #[test]
    fn missing_events_is_an_empty_transcript_not_an_error() {
        assert_eq!(parse_json3(r#"{"wireMagic":"pb3"}"#).unwrap(), vec![]);
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(matches!(parse_json3("<html>"), Err(Error::Parse { .. })));
    }

    #[test]
    fn strip_annotations_cases() {
        assert_eq!(strip_annotations("[संगीत] अपनी बड़ी"), "अपनी बड़ी");
        assert_eq!(strip_annotations("a [Music] b"), "a b");
        assert_eq!(strip_annotations("(baaah!!) and"), "(baaah!!) and");
        assert_eq!(strip_annotations("unclosed [bracket"), "unclosed [bracket");
        assert_eq!(strip_annotations("  ♪♪  "), "");
    }
}
