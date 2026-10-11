//! Markdown notes with YAML frontmatter (PDA §5, OUT-3, OUT-4, OUT-6).
//!
//! Step 1 writes the raw transcript; the cleanup fields (`translated`,
//! `cleaned_by`, `cleanup`) arrive with the LLM stage.

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use crate::error::fs_err;
use crate::metadata::{Platform, VideoMeta};
use crate::transcript::Transcript;
use crate::{Error, Result};

const MAX_FILENAME_CHARS: usize = 120;
/// Linux file names max out at 255 bytes; Devanagari is 3 bytes a character,
/// so a character cap alone isn't enough. Leaves room for " (n).md".
const MAX_FILENAME_BYTES: usize = 200;
const TITLE_FROM_TEXT_WORDS: usize = 8;

/// The note's title. Reels have no real title ("Video by <handle>"), so they
/// use the opening words of the transcript instead (PDA OUT-3).
pub fn note_title(meta: &VideoMeta, transcript: &Transcript) -> String {
    let title = meta.title.trim();
    let placeholder = meta.platform == Platform::Instagram && (title.is_empty() || title.starts_with("Video by "));
    if !placeholder {
        return title.to_string();
    }
    let words: Vec<String> = transcript
        .segments
        .iter()
        .flat_map(|s| s.text.split_whitespace())
        .take(TITLE_FROM_TEXT_WORDS)
        .map(str::to_string)
        .collect();
    if words.is_empty() {
        format!("Instagram reel {}", meta.id)
    } else {
        words.join(" ")
    }
}

/// `<YYYY-MM-DD> <Title>.md`, with characters that aren't safe in file names
/// replaced and the length capped.
pub fn note_filename(date: Option<&str>, title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { ' ' } else { c })
        .collect();
    let mut name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    name = name.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if name.is_empty() {
        name = "Untitled".into();
    }
    if let Some(date) = date {
        name = format!("{date} {name}");
    }
    if name.chars().count() > MAX_FILENAME_CHARS || name.len() > MAX_FILENAME_BYTES {
        let mut bytes = 0;
        name = name
            .chars()
            .take(MAX_FILENAME_CHARS)
            .take_while(|c| {
                bytes += c.len_utf8();
                bytes <= MAX_FILENAME_BYTES
            })
            .collect::<String>();
        name = name.trim_end_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    }
    format!("{name}.md")
}

pub fn render_note(meta: &VideoMeta, transcript: &Transcript, today: &str) -> String {
    // JSON string literals are valid YAML double-quoted scalars, so
    // serde_json does the escaping for free-text fields.
    let quoted = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    let language = transcript.language.clone().or_else(|| meta.spoken_language());

    let mut fm = vec![
        format!("title: {}", quoted(&note_title(meta, transcript))),
        format!("url: {}", quoted(&meta.url)),
        format!("platform: {}", meta.platform.as_str()),
    ];
    if let Some(channel) = &meta.channel {
        fm.push(format!("channel: {}", quoted(channel)));
    }
    if let Some(date) = &meta.upload_date {
        fm.push(format!("upload_date: {date}"));
    }
    if let Some(d) = meta.duration_secs {
        fm.push(format!("duration: {d}"));
    }
    if let Some(lang) = language {
        fm.push(format!("language: {lang}"));
    }
    fm.push(format!("source: {}", transcript.source.as_str()));
    fm.push(format!("transcribed: {today}"));
    fm.push(format!("shruti_id: {}", quoted(&meta.key())));

    format!("---\n{}\n---\n\n{}\n", fm.join("\n"), transcript.text())
}

/// Writes the note into `dir` (created if missing) and returns its path.
///
/// Never overwrites (PDA OUT-6): if a note with that name exists and differs,
/// the new one gets ` (2)`, ` (3)`, …. If an existing note already has the
/// same content (ignoring the `transcribed:` date), it's reused, so running
/// the same link twice doesn't pile up copies.
pub async fn write_note(dir: &Path, meta: &VideoMeta, transcript: &Transcript, today: &str) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await.map_err(fs_err("create folder", dir))?;
    let file_name = note_filename(meta.upload_date.as_deref(), &note_title(meta, transcript));
    let stem = file_name.trim_end_matches(".md");
    let body = render_note(meta, transcript, today);

    for n in 1..=MAX_COPIES {
        let candidate = if n == 1 { dir.join(&file_name) } else { dir.join(format!("{stem} ({n}).md")) };
        // `create_new` makes "exists?" and "create" one atomic step, so a
        // concurrent writer can't slip in between and get overwritten.
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&candidate).await {
            Ok(mut file) => {
                file.write_all(body.as_bytes()).await.map_err(fs_err("write", &candidate))?;
                file.flush().await.map_err(fs_err("write", &candidate))?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match tokio::fs::read_to_string(&candidate).await {
                    Ok(existing) if same_note(&existing, &body) => return Ok(candidate),
                    Ok(_) => {}
                    // Unreadable (permissions, not UTF-8): it can't be the
                    // same note, so move on, but leave a trace.
                    Err(e) => tracing::warn!(path = %candidate.display(), error = %e, "existing note unreadable"),
                }
            }
            Err(e) => return Err(fs_err("create", &candidate)(e)),
        }
    }
    Err(Error::Fs {
        action: "find a free name for",
        path: dir.join(&file_name),
        source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
    })
}

/// Enough for any real re-run pattern; beyond this something is looping.
const MAX_COPIES: u32 = 1_000;

fn same_note(a: &str, b: &str) -> bool {
    let without_date = |s: &str| {
        s.lines().filter(|l| !l.starts_with("transcribed: ")).collect::<Vec<_>>().join("\n")
    };
    without_date(a) == without_date(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Segment, TranscriptSource};
    use pretty_assertions::assert_eq;

    fn zoo() -> VideoMeta {
        VideoMeta {
            platform: Platform::YouTube,
            id: "jNQXAC9IVRw".into(),
            url: "https://www.youtube.com/watch?v=jNQXAC9IVRw".into(),
            title: "Me at the zoo".into(),
            channel: Some("jawed".into()),
            upload_date: Some("2005-04-24".into()),
            duration_secs: Some(19),
            language: None,
            subtitles: vec![],
            auto_captions: vec![],
        }
    }

    fn reel() -> VideoMeta {
        VideoMeta {
            platform: Platform::Instagram,
            id: "DPx0yXkkS5M".into(),
            url: "https://www.instagram.com/reel/DPx0yXkkS5M/".into(),
            title: "Video by natgeo".into(),
            channel: Some("natgeo".into()),
            upload_date: None,
            ..zoo()
        }
    }

    fn transcript(texts: &[&str]) -> Transcript {
        Transcript {
            source: TranscriptSource::Whisper,
            language: Some("en".into()),
            segments: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Segment { start_ms: i as u64 * 1000, end_ms: i as u64 * 1000 + 900, text: t.to_string() })
                .collect(),
        }
    }

    #[test]
    fn filenames() {
        assert_eq!(note_filename(Some("2005-04-24"), "Me at the zoo"), "2005-04-24 Me at the zoo.md");
        assert_eq!(note_filename(None, "Me at the zoo"), "Me at the zoo.md");
        assert_eq!(note_filename(None, "What is UI/UX?"), "What is UI UX.md");
        assert_eq!(note_filename(None, r#"a\b: "c" <d>|e*"#), "a b c d e.md");
        assert_eq!(note_filename(None, "  ..dots and spaces..  "), "dots and spaces.md");
        assert_eq!(note_filename(None, "tab\tand\nnewline"), "tab and newline.md");
        assert_eq!(note_filename(None, " / "), "Untitled.md");
    }

    #[test]
    fn long_titles_are_cut_on_a_character_boundary() {
        let title = "दृश्यम ".repeat(40);
        let name = note_filename(Some("2026-10-07"), &title);
        assert!(name.ends_with(".md"));
        assert!(name.trim_end_matches(".md").chars().count() <= MAX_FILENAME_CHARS);
        assert!(!name.trim_end_matches(".md").ends_with(' '));
    }

    #[test]
    fn youtube_title_is_the_video_title() {
        assert_eq!(note_title(&zoo(), &transcript(&["anything"])), "Me at the zoo");
    }

    #[test]
    fn reel_title_comes_from_the_transcript() {
        let t = transcript(&["Three things nobody tells you about", "designing onboarding flows for apps"]);
        assert_eq!(note_title(&reel(), &t), "Three things nobody tells you about designing onboarding");
        assert_eq!(note_title(&reel(), &transcript(&[])), "Instagram reel DPx0yXkkS5M");
    }

    #[test]
    fn renders_frontmatter_and_body() {
        let note = render_note(&zoo(), &transcript(&["Alright, so here we are.", "That's cool."]), "2026-10-11");
        assert_eq!(
            note,
            "---\n\
             title: \"Me at the zoo\"\n\
             url: \"https://www.youtube.com/watch?v=jNQXAC9IVRw\"\n\
             platform: youtube\n\
             channel: \"jawed\"\n\
             upload_date: 2005-04-24\n\
             duration: 19\n\
             language: en\n\
             source: whisper\n\
             transcribed: 2026-10-11\n\
             shruti_id: \"youtube:jNQXAC9IVRw\"\n\
             ---\n\
             \n\
             Alright, so here we are. That's cool.\n"
        );
    }

    #[test]
    fn frontmatter_escapes_quotes_and_omits_unknowns() {
        let meta = VideoMeta {
            title: "He said \"hi\"\nthen left".into(),
            channel: None,
            upload_date: None,
            duration_secs: None,
            ..zoo()
        };
        let mut t = transcript(&["x"]);
        t.language = None;
        let note = render_note(&meta, &t, "2026-10-11");
        assert!(note.contains(r#"title: "He said \"hi\"\nthen left""#), "{note}");
        assert!(!note.contains("channel:"));
        assert!(!note.contains("upload_date:"));
        assert!(!note.contains("duration:"));
        assert!(!note.contains("language:"));
    }

    #[tokio::test]
    async fn write_note_never_overwrites_a_different_note() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested/Inbox");
        let p1 = write_note(&out, &zoo(), &transcript(&["first"]), "2026-10-11").await.unwrap();
        assert_eq!(p1, out.join("2005-04-24 Me at the zoo.md"));
        let p2 = write_note(&out, &zoo(), &transcript(&["second"]), "2026-10-11").await.unwrap();
        assert_eq!(p2, out.join("2005-04-24 Me at the zoo (2).md"));
        let p3 = write_note(&out, &zoo(), &transcript(&["third"]), "2026-10-11").await.unwrap();
        assert_eq!(p3, out.join("2005-04-24 Me at the zoo (3).md"));
        assert!(std::fs::read_to_string(&p1).unwrap().ends_with("first\n"));
        assert!(std::fs::read_to_string(&p2).unwrap().ends_with("second\n"));
    }

    #[tokio::test]
    async fn rerunning_the_same_link_reuses_the_note_even_on_another_day() {
        let dir = tempfile::tempdir().unwrap();
        let t = transcript(&["same words"]);
        let p1 = write_note(dir.path(), &zoo(), &t, "2026-10-11").await.unwrap();
        let p2 = write_note(dir.path(), &zoo(), &t, "2026-10-12").await.unwrap();
        assert_eq!(p1, p2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(std::fs::read_to_string(&p1).unwrap().contains("transcribed: 2026-10-11"));
    }

    #[tokio::test]
    async fn an_edited_note_is_not_reused_or_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let t = transcript(&["same words"]);
        let p1 = write_note(dir.path(), &zoo(), &t, "2026-10-11").await.unwrap();
        std::fs::write(&p1, "my own edits").unwrap();
        let p2 = write_note(dir.path(), &zoo(), &t, "2026-10-11").await.unwrap();
        assert_ne!(p1, p2);
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "my own edits");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_never_clobber_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.path().to_path_buf();
                tokio::spawn(async move {
                    let words = format!("writer {i}");
                    let path = write_note(&dir, &zoo(), &transcript(&[&words]), "2026-10-11").await.unwrap();
                    (path, words)
                })
            })
            .collect();
        let mut paths = std::collections::HashSet::new();
        for task in tasks {
            let (path, words) = task.await.unwrap();
            assert!(std::fs::read_to_string(&path).unwrap().ends_with(&format!("{words}\n")));
            assert!(paths.insert(path), "two writers got the same file");
        }
        assert_eq!(paths.len(), 8);
    }

    #[test]
    fn devanagari_titles_fit_the_filesystem_byte_limit() {
        let name = note_filename(Some("2026-10-07"), &"दृश्यम ".repeat(40));
        assert!(name.len() <= MAX_FILENAME_BYTES + ".md".len(), "{} bytes", name.len());
    }
}
