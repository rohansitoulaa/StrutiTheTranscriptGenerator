//! A raw transcript: timed segments plus where they came from.

use crate::metadata::CaptionKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSource {
    Captions(CaptionKind),
    Whisper,
}

impl TranscriptSource {
    /// Frontmatter value (PDA §5: `captions | whisper`).
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptSource::Captions(_) => "captions",
            TranscriptSource::Whisper => "whisper",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub source: TranscriptSource,
    /// Primary language subtag of the transcript text, e.g. `hi`.
    pub language: Option<String>,
    pub segments: Vec<Segment>,
}

/// A pause this long starts a new paragraph.
const PARAGRAPH_GAP_MS: u64 = 2_000;
/// Past this length, the next sentence end starts a new paragraph.
const PARAGRAPH_SOFT_MAX_CHARS: usize = 600;

impl Transcript {
    /// Plain paragraphs separated by blank lines (PDA OUT-4).
    pub fn text(&self) -> String {
        paragraphs(&self.segments).join("\n\n")
    }

    pub fn is_empty(&self) -> bool {
        self.segments.iter().all(|s| s.text.trim().is_empty())
    }
}

pub fn paragraphs(segments: &[Segment]) -> Vec<String> {
    paragraphs_with(segments, PARAGRAPH_GAP_MS, PARAGRAPH_SOFT_MAX_CHARS)
}

fn paragraphs_with(segments: &[Segment], gap_ms: u64, soft_max_chars: usize) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut last_end: Option<u64> = None;

    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        let long_pause = last_end.is_some_and(|end| seg.start_ms.saturating_sub(end) >= gap_ms);
        let long_enough = current.chars().count() >= soft_max_chars && ends_sentence(&current);
        if !current.is_empty() && (long_pause || long_enough) {
            paragraphs.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(text);
        last_end = Some(seg.end_ms);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end().ends_with(['.', '?', '!', '।'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn seg(start_ms: u64, end_ms: u64, text: &str) -> Segment {
        Segment {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    #[test]
    fn joins_segments_and_trims_whitespace() {
        let segs = [
            seg(0, 1000, " Alright, so here we are,"),
            seg(1000, 2000, "  one of the elephants. "),
        ];
        assert_eq!(
            paragraphs(&segs),
            vec!["Alright, so here we are, one of the elephants."]
        );
    }

    #[test]
    fn long_pause_starts_a_new_paragraph() {
        let segs = [
            seg(0, 1000, "First."),
            seg(1500, 2000, "Still first."),
            seg(4000, 5000, "Second."),
        ];
        assert_eq!(paragraphs(&segs), vec!["First. Still first.", "Second."]);
    }

    #[test]
    fn long_paragraphs_break_at_the_next_sentence_end() {
        let segs = [
            seg(0, 1, "one two three"),
            seg(1, 2, "four five."),
            seg(2, 3, "six seven"),
            seg(3, 4, "eight?"),
            seg(4, 5, "nine"),
        ];
        assert_eq!(
            paragraphs_with(&segs, 2_000, 10),
            vec!["one two three four five.", "six seven eight?", "nine"]
        );
    }

    #[test]
    fn devanagari_danda_ends_a_sentence() {
        let segs = [seg(0, 1, "यह मूवी आखिरी चैप्टर है।"), seg(1, 2, "इसीलिए अब")];
        assert_eq!(
            paragraphs_with(&segs, 2_000, 5),
            vec!["यह मूवी आखिरी चैप्टर है।", "इसीलिए अब"]
        );
    }

    #[test]
    fn empty_segments_are_ignored() {
        let segs = [seg(0, 1, "  "), seg(1, 2, "hi"), seg(9000, 9001, "")];
        assert_eq!(paragraphs(&segs), vec!["hi"]);
        assert!(paragraphs(&[]).is_empty());
    }

    #[test]
    fn text_and_is_empty() {
        let t = Transcript {
            source: TranscriptSource::Whisper,
            language: Some("en".into()),
            segments: vec![seg(0, 1, "A."), seg(5000, 6000, "B.")],
        };
        assert_eq!(t.text(), "A.\n\nB.");
        assert!(!t.is_empty());
        let empty = Transcript {
            segments: vec![seg(0, 1, " ")],
            ..t
        };
        assert!(empty.is_empty());
    }
}
