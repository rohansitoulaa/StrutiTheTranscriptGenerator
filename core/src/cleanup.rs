//! Fluff removal and translation (PDA §4.3, CL-1 … CL-6).
//!
//! The LLM edits by deletion: greetings, housekeeping, sponsor reads,
//! outros and filler go; every piece of information stays, in the
//! speaker's words and order. Long transcripts are cleaned in chunks, each
//! told where it sits, so only the first loses an intro and only the last
//! an outro. A word-count guardrail flags results that cut or added
//! suspiciously much (CL-6).

use crate::llm::{Llm, LlmError};
use crate::transcript::Transcript;

/// The default instructions (PDA §4.3). Editable in Settings later (CL-10).
pub const DEFAULT_PROMPT: &str = "\
You edit video transcripts into clean reading notes by deleting, never by rewriting.

Remove:
- greetings and openers (\"hey what's up guys\", \"welcome back to the channel\", \"before we start\")
- channel housekeeping: like/subscribe/bell requests, \"link in the description\", \"comment below\", Patreon and merch plugs
- sponsor reads and ad segments
- outros and sign-offs (\"that's it for today\", \"see you in the next one\")
- filler and repetition: um, uh, you know, basically, false starts, the same point said twice in a row

Keep every piece of actual information, in the order the speaker gave it, in the speaker's own words. \
Never summarize. Never add anything that isn't in the transcript. Never add commentary.

Output only the cleaned text as plain paragraphs separated by blank lines: \
no headings, bullet points, timestamps, quotation marks around the text, or notes about what you did.

If the transcript is not in English, translate it into natural English while following the same rules.";

#[derive(Debug, Clone, PartialEq)]
pub struct CleanupSettings {
    pub prompt: String,
    /// Estimated tokens of transcript per request. Leave room for the
    /// reply, which is about as long again (CL-5).
    pub chunk_tokens: usize,
    /// Cleaned/raw word ratio below this is flagged for review.
    pub min_ratio: f32,
    /// Above this is flagged (the model added text).
    pub max_ratio: f32,
    /// Translations run longer (Hindi → English), so they get more room.
    pub max_ratio_translated: f32,
}

impl Default for CleanupSettings {
    fn default() -> Self {
        CleanupSettings {
            prompt: DEFAULT_PROMPT.to_string(),
            // Fits LM Studio's default 4k context with the prompt and reply.
            chunk_tokens: 1_200,
            min_ratio: 0.25,
            max_ratio: 1.10,
            max_ratio_translated: 1.6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    pub text: String,
    pub translated: bool,
    /// Flagged by the guardrail (CL-6): the note is still written.
    pub review: bool,
    /// `provider/model`.
    pub model: String,
    pub raw_words: usize,
    pub clean_words: usize,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

pub async fn clean<L: Llm>(
    llm: &L,
    transcript: &Transcript,
    settings: &CleanupSettings,
) -> Result<Cleaned, LlmError> {
    let raw = transcript.text();
    let language = transcript.language.as_deref();
    let translated = language.is_some_and(|l| l != "en");
    let chunks = chunk(&raw, settings.chunk_tokens);

    let mut parts = Vec::with_capacity(chunks.len());
    let (mut tokens_in, mut tokens_out) = (0u64, 0u64);
    let mut dropped = 0;
    for (i, text) in chunks.iter().enumerate() {
        let user = user_message(text, i, chunks.len(), language);
        let reply = llm.chat(&settings.prompt, &user).await?;
        tokens_in += u64::from(reply.tokens_in.unwrap_or(0));
        tokens_out += u64::from(reply.tokens_out.unwrap_or(0));
        let part = tidy(&reply.text);
        if part.is_empty() {
            // One lost chunk barely moves the overall word ratio, so it
            // gets flagged on its own.
            tracing::warn!(
                chunk = i + 1,
                of = chunks.len(),
                "a cleaned chunk came back empty"
            );
            dropped += 1;
        } else {
            parts.push(part);
        }
    }

    let text = parts.join("\n\n");
    let raw_words = raw.split_whitespace().count();
    let clean_words = text.split_whitespace().count();
    let max = if translated {
        settings.max_ratio_translated
    } else {
        settings.max_ratio
    };
    let ratio = if raw_words == 0 {
        0.0
    } else {
        clean_words as f32 / raw_words as f32
    };
    let review = text.is_empty() || dropped > 0 || ratio < settings.min_ratio || ratio > max;
    if review {
        tracing::warn!(ratio, raw_words, clean_words, "cleanup flagged for review");
    }
    Ok(Cleaned {
        text,
        translated,
        review,
        model: llm.label(),
        raw_words,
        clean_words,
        tokens_in,
        tokens_out,
    })
}

fn user_message(text: &str, index: usize, total: usize, language: Option<&str>) -> String {
    let position = if total == 1 {
        "This is the whole transcript.".to_string()
    } else if index == 0 {
        format!(
            "This is the beginning of the transcript (part 1 of {total}). \
             Remove an opening greeting or intro if there is one. It continues in the next part, so don't end it."
        )
    } else if index + 1 == total {
        format!(
            "This is the end of the transcript (part {total} of {total}). \
             Remove an outro or sign-off if there is one. It continues from the previous part, so don't introduce it."
        )
    } else {
        format!(
            "This is a middle part of the transcript (part {} of {total}). \
             It continues before and after, so don't treat anything here as an intro or outro.",
            index + 1
        )
    };
    let language_line = match language {
        Some("en") | None => String::new(),
        Some(code) => format!(
            "\nThe transcript is in {}. Translate it into natural English.",
            language_name(code)
        ),
    };
    format!("{position}{language_line}\n\nTranscript:\n\n{text}")
}

fn language_name(code: &str) -> &str {
    match code {
        "hi" => "Hindi",
        "ne" => "Nepali",
        "ur" => "Urdu",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        other => other,
    }
}

/// Rough token count: English runs ~4 characters a token; Devanagari and
/// other scripts closer to 1.5.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    let (ascii, other) = text.chars().fold((0usize, 0usize), |(a, o), c| {
        if c.is_ascii() { (a + 1, o) } else { (a, o + 1) }
    });
    ascii / 4 + other * 2 / 3 + 1
}

/// Splits text into chunks of at most `max_tokens`, on paragraph
/// boundaries, then sentences, then words for a runaway sentence.
pub(crate) fn chunk(text: &str, max_tokens: usize) -> Vec<String> {
    let pieces: Vec<String> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .flat_map(|p| split_to_fit(p, max_tokens))
        .collect();
    pack(pieces, max_tokens, "\n\n")
}

/// Greedily joins pieces with `sep` while they fit in `max_tokens`.
fn pack(pieces: impl IntoIterator<Item = String>, max_tokens: usize, sep: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for piece in pieces {
        if !current.is_empty() && estimate_tokens(&current) + estimate_tokens(&piece) > max_tokens {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(sep);
        }
        current.push_str(&piece);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_to_fit(paragraph: &str, max_tokens: usize) -> Vec<String> {
    if estimate_tokens(paragraph) <= max_tokens {
        return vec![paragraph.to_string()];
    }
    let units = sentences(paragraph).into_iter().flat_map(|s| {
        if estimate_tokens(s) <= max_tokens {
            vec![s.to_string()]
        } else {
            pack(s.split_whitespace().map(str::to_string), max_tokens, " ")
        }
    });
    pack(units, max_tokens, " ")
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '?' | '!' | '।') {
            let end = i + c.len_utf8();
            let s = text[start..end].trim();
            if !s.is_empty() {
                out.push(s);
            }
            start = end;
        }
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// Strips what models wrap answers in despite instructions: code fences,
/// "Here is the cleaned transcript:" lead-ins, "Let me know…" sign-offs.
pub(crate) fn tidy(reply: &str) -> String {
    let mut lines: Vec<&str> = reply
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    while lines
        .first()
        .is_some_and(|l| l.trim().is_empty() || is_lead_in(l))
    {
        lines.remove(0);
    }
    while lines
        .last()
        .is_some_and(|l| l.trim().is_empty() || is_sign_off(l))
    {
        lines.pop();
    }
    let mut text = lines.join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

fn is_lead_in(line: &str) -> bool {
    let l = line.trim().to_ascii_lowercase();
    let opener = ["here is", "here's", "sure", "okay", "certainly", "below is"]
        .iter()
        .any(|p| l.starts_with(p));
    opener && (l.ends_with(':') || l.contains("cleaned") || l.contains("translat") || l.len() < 12)
}

/// Only lines that are clearly the model talking to the reader: a real
/// closing sentence like "Feel free to experiment with the settings." stays.
fn is_sign_off(line: &str) -> bool {
    let l = line.trim().to_ascii_lowercase();
    let opener = [
        "let me know",
        "i hope this",
        "hope this helps",
        "if you'd like",
        "feel free to",
    ]
    .iter()
    .any(|p| l.starts_with(p));
    let about_the_answer = [
        "help",
        "anything else",
        "question",
        "assist",
        "further",
        "adjust",
        "thought",
        "reach out",
        "feedback",
        "comment",
        "need",
        "want me",
    ]
    .iter()
    .any(|w| l.contains(w));
    opener && about_the_answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatReply;
    use crate::transcript::{Segment, TranscriptSource};
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;

    /// Replies with canned text per call, records each user message.
    struct FakeLlm {
        replies: Mutex<Vec<Result<String, LlmError>>>,
        seen: Mutex<Vec<String>>,
    }

    impl FakeLlm {
        fn new(replies: Vec<Result<&str, LlmError>>) -> Self {
            FakeLlm {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .rev()
                        .map(|r| r.map(str::to_string))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Llm for FakeLlm {
        async fn chat(&self, system: &str, user: &str) -> Result<ChatReply, LlmError> {
            assert!(system.contains("deleting, never by rewriting"));
            self.seen.lock().unwrap().push(user.to_string());
            let text = self
                .replies
                .lock()
                .unwrap()
                .pop()
                .expect("unexpected extra call")?;
            Ok(ChatReply {
                text,
                tokens_in: Some(100),
                tokens_out: Some(50),
            })
        }
        fn label(&self) -> String {
            "lmstudio/gemma-4-31b".into()
        }
    }

    fn transcript(language: &str, paragraphs: &[&str]) -> Transcript {
        Transcript {
            source: TranscriptSource::Whisper,
            language: Some(language.into()),
            // A 5 s gap between segments makes each one its own paragraph.
            segments: paragraphs
                .iter()
                .enumerate()
                .map(|(i, p)| Segment {
                    start_ms: i as u64 * 10_000,
                    end_ms: i as u64 * 10_000 + 5_000,
                    text: p.to_string(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn short_transcripts_go_in_one_request() {
        let t = transcript(
            "en",
            &["Hey what's up guys. Index funds track a whole market. Smash that like button."],
        );
        let llm = FakeLlm::new(vec![Ok("Index funds track a whole market.")]);
        let out = clean(&llm, &t, &CleanupSettings::default()).await.unwrap();
        assert_eq!(out.text, "Index funds track a whole market.");
        assert!(!out.translated);
        assert_eq!(out.model, "lmstudio/gemma-4-31b");
        assert_eq!((out.tokens_in, out.tokens_out), (100, 50));
        let seen = llm.seen();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("This is the whole transcript."));
        assert!(seen[0].contains("Smash that like button."));
        assert!(!seen[0].contains("Translate"));
    }

    #[tokio::test]
    async fn hindi_is_translated_in_the_same_pass() {
        let t = transcript("hi", &["तो दोस्तों, आज हम इंडेक्स फंड के बारे में बात करेंगे।"]);
        let llm = FakeLlm::new(vec![Ok("Today we'll talk about index funds, friends.")]);
        let out = clean(&llm, &t, &CleanupSettings::default()).await.unwrap();
        assert!(out.translated);
        assert!(!out.review, "a slightly longer English translation is fine");
        assert!(
            llm.seen()[0]
                .contains("The transcript is in Hindi. Translate it into natural English.")
        );
    }

    #[tokio::test]
    async fn long_transcripts_are_chunked_and_told_where_they_sit() {
        let paragraph = "This sentence carries real information about the topic. ".repeat(20);
        let t = transcript("en", &[&paragraph, &paragraph, &paragraph]);
        let settings = CleanupSettings {
            chunk_tokens: estimate_tokens(&paragraph) + 5,
            ..CleanupSettings::default()
        };
        let llm = FakeLlm::new(vec![Ok("First."), Ok("Second."), Ok("Third.")]);
        let out = clean(&llm, &t, &settings).await.unwrap();
        assert_eq!(out.text, "First.\n\nSecond.\n\nThird.");
        let seen = llm.seen();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].starts_with("This is the beginning of the transcript (part 1 of 3)."));
        assert!(seen[1].starts_with("This is a middle part of the transcript (part 2 of 3)."));
        assert!(seen[2].starts_with("This is the end of the transcript (part 3 of 3)."));
    }

    #[test]
    fn chunking_never_loses_or_reorders_text() {
        let text =
            "One. Two two. Three three three.\n\nFour. Five.\n\n".repeat(30) + &"word ".repeat(400);
        for max in [5, 20, 100, 10_000] {
            let chunks = chunk(&text, max);
            let rejoined: Vec<&str> = chunks.iter().flat_map(|c| c.split_whitespace()).collect();
            let original: Vec<&str> = text.split_whitespace().collect();
            assert_eq!(rejoined, original, "max {max}");
            if max >= 20 {
                assert!(
                    chunks.iter().all(|c| estimate_tokens(c) <= max + 2),
                    "max {max}"
                );
            }
        }
    }

    #[test]
    fn devanagari_counts_as_more_tokens_per_character() {
        let hindi = "दृश्यम द कंक्लूजन"; // 17 characters
        let english = "Drishyam conclusio"; // 18 characters
        assert!(estimate_tokens(hindi) > 2 * estimate_tokens(english));
        assert_eq!(estimate_tokens(""), 1);
    }

    #[tokio::test]
    async fn guardrail_flags_too_short_too_long_and_empty() {
        let raw = "real words here ".repeat(25);
        for (reply, review) in [
            ("real words here ".repeat(15), false),
            ("real".to_string(), true),
            ("real words here ".repeat(40), true),
            (String::new(), true),
        ] {
            let llm = FakeLlm::new(vec![Ok(reply.as_str())]);
            let out = clean(
                &llm,
                &transcript("en", &[&raw]),
                &CleanupSettings::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                out.review,
                review,
                "reply of {} words",
                reply.split_whitespace().count()
            );
        }
    }

    #[tokio::test]
    async fn provider_errors_reach_the_caller_unchanged() {
        let llm = FakeLlm::new(vec![Err(LlmError::QuotaExhausted)]);
        let err = clean(&llm, &transcript("en", &["x"]), &CleanupSettings::default())
            .await
            .unwrap_err();
        assert_eq!(err, LlmError::QuotaExhausted);
    }

    #[tokio::test]
    async fn every_chunk_of_a_long_hindi_transcript_is_told_to_translate() {
        let paragraph = "यह वाक्य विषय के बारे में असली जानकारी देता है। ".repeat(15);
        let t = transcript("hi", &[&paragraph, &paragraph, &paragraph]);
        let settings = CleanupSettings {
            chunk_tokens: estimate_tokens(&paragraph) + 5,
            ..CleanupSettings::default()
        };
        let llm = FakeLlm::new(vec![Ok("One."), Ok("Two."), Ok("Three.")]);
        clean(&llm, &t, &settings).await.unwrap();
        let seen = llm.seen();
        assert_eq!(seen.len(), 3);
        for message in seen {
            assert!(
                message.contains("Translate it into natural English."),
                "{message}"
            );
        }
    }

    #[tokio::test]
    async fn guardrail_boundaries_are_inclusive() {
        // 100 raw words: exactly 25 and exactly 110 cleaned words pass;
        // 24 and 111 are flagged.
        let raw = "word ".repeat(100);
        for (words, review) in [(25, false), (24, true), (110, false), (111, true)] {
            let reply = "kept ".repeat(words);
            let llm = FakeLlm::new(vec![Ok(reply.as_str())]);
            let out = clean(
                &llm,
                &transcript("en", &[&raw]),
                &CleanupSettings::default(),
            )
            .await
            .unwrap();
            assert_eq!(out.review, review, "{words} words");
        }
        // Translations get up to 1.6×.
        let llm = FakeLlm::new(vec![Ok("kept ".repeat(160).as_str())]);
        let out = clean(
            &llm,
            &transcript("hi", &[&raw]),
            &CleanupSettings::default(),
        )
        .await
        .unwrap();
        assert!(!out.review);
    }

    #[tokio::test]
    async fn a_chunk_that_comes_back_empty_flags_the_note() {
        let paragraph = "This sentence carries real information about the topic. ".repeat(20);
        let t = transcript("en", &[&paragraph, &paragraph, &paragraph]);
        let settings = CleanupSettings {
            chunk_tokens: estimate_tokens(&paragraph) + 5,
            ..CleanupSettings::default()
        };
        let full = paragraph.trim().to_string();
        let llm = FakeLlm::new(vec![
            Ok(full.as_str()),
            Ok("Here is the cleaned transcript:"),
            Ok(full.as_str()),
        ]);
        let out = clean(&llm, &t, &settings).await.unwrap();
        assert!(out.review, "a whole chunk vanished");
    }

    #[test]
    fn tidy_keeps_real_closing_sentences() {
        assert_eq!(
            tidy("Set the gain first.\n\nFeel free to experiment with the settings."),
            "Set the gain first.\n\nFeel free to experiment with the settings."
        );
        for sign_off in [
            "Feel free to ask if you have any questions!",
            "Let me know your thoughts.",
            "Feel free to reach out.",
            "Let me know if you need anything changed.",
            "If you'd like, I can shorten it further.",
        ] {
            assert_eq!(
                tidy(&format!("Set the gain first.\n\n{sign_off}")),
                "Set the gain first.",
                "{sign_off}"
            );
        }
    }

    #[test]
    fn tidy_strips_model_chatter() {
        assert_eq!(
            tidy(
                "Here is the cleaned transcript:\n\nIndex funds are cheap.\n\nLet me know if you need anything else!"
            ),
            "Index funds are cheap."
        );
        assert_eq!(tidy("```\nA.\n\n\n\nB.\n```"), "A.\n\nB.");
        assert_eq!(tidy("Sure!\nA."), "A.");
        // Real content that merely starts with "Okay" stays.
        assert_eq!(
            tidy("Okay so the first thing you need to understand about compound interest is time."),
            "Okay so the first thing you need to understand about compound interest is time."
        );
    }
}
