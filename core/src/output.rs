//! Markdown notes with YAML frontmatter (PDA §5, OUT-3, OUT-4, OUT-6).
//!
//! The body is the cleaned text when there is one, else the raw
//! transcript (the CLI's `--once` mode). Notes are never overwritten unless
//! their content is still exactly what Shruti wrote (a re-clean).

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use crate::cleanup::Cleaned;
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
/// use the opening words of the cleaned text, else the transcript (OUT-3).
pub fn note_title(meta: &VideoMeta, transcript: &Transcript, cleaned: Option<&Cleaned>) -> String {
    let title = meta.title.trim();
    let placeholder = meta.platform == Platform::Instagram
        && (title.is_empty() || title.starts_with("Video by "));
    if !placeholder {
        return title.to_string();
    }
    let words: Vec<String> = match cleaned.filter(|c| !c.text.trim().is_empty()) {
        Some(c) => c
            .text
            .split_whitespace()
            .take(TITLE_FROM_TEXT_WORDS)
            .map(str::to_string)
            .collect(),
        None => transcript
            .segments
            .iter()
            .flat_map(|s| s.text.split_whitespace())
            .take(TITLE_FROM_TEXT_WORDS)
            .map(str::to_string)
            .collect(),
    };
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
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    name = name
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string();
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
        name = name
            .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
            .to_string();
    }
    format!("{name}.md")
}

pub fn render_note(
    meta: &VideoMeta,
    transcript: &Transcript,
    cleaned: Option<&Cleaned>,
    today: &str,
) -> String {
    // JSON string literals are valid YAML double-quoted scalars, so
    // serde_json does the escaping for free-text fields.
    let quoted = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    let language = transcript
        .language
        .clone()
        .or_else(|| meta.spoken_language());

    let mut fm = vec![
        format!("title: {}", quoted(&note_title(meta, transcript, cleaned))),
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
    if let Some(c) = cleaned {
        fm.push(format!("translated: {}", c.translated));
    }
    fm.push(format!("source: {}", transcript.source.as_str()));
    if let Some(c) = cleaned {
        fm.push(format!("cleaned_by: {}", quoted(&c.model)));
        fm.push(format!(
            "cleanup: {}",
            if c.review { "review" } else { "ok" }
        ));
    }
    fm.push(format!("transcribed: {today}"));
    fm.push(format!("shruti_id: {}", quoted(&meta.key())));

    let body = neutralize_html(&match cleaned {
        Some(c) => c.text.clone(),
        None => transcript.text(),
    });
    format!("---\n{}\n---\n\n{}\n", fm.join("\n"), body)
}

/// Obsidian renders raw HTML in notes, and the body comes from untrusted
/// video speech via an LLM. `<` that starts a tag becomes `&lt;` (shown as
/// `<`), so nothing in a transcript can become live markup; "a < b" is left
/// as it is.
pub(crate) fn neutralize_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let starts_tag = c == '<'
            && chars
                .peek()
                .is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?'));
        if starts_tag {
            out.push_str("&lt;");
        } else {
            out.push(c);
        }
    }
    out
}

/// Hex SHA-256 of a note's content: the fingerprint of what Shruti wrote.
pub fn sha256_hex(content: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Rewrites a note in place if its content is still exactly one of the
/// versions Shruti wrote (`known_hashes`). Returns `false`, touching
/// nothing, if it's missing or was edited (OUT-6: never overwrite edits).
pub async fn update_note(path: &Path, known_hashes: &[String], content: &str) -> Result<bool> {
    let current = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(fs_err("read", path)(e)),
    };
    if !known_hashes.contains(&sha256_hex(&current)) {
        return Ok(false);
    }
    tokio::fs::write(path, content)
        .await
        .map_err(fs_err("write", path))?;
    Ok(true)
}

/// Notes under `root` whose frontmatter names this video, wherever they've
/// been moved to (FO-9). Hidden folders (`.obsidian`) are skipped.
pub fn find_notes_by_id(root: &Path, video_key: &str) -> Result<Vec<PathBuf>> {
    let needle = format!(
        "shruti_id: {}",
        serde_json::Value::String(video_key.to_string())
    );
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(fs_err("read folder", &dir)(e)),
        };
        for entry in entries {
            let entry = entry.map_err(fs_err("read folder", &dir))?;
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            // `file_type` doesn't follow symlinks out of the vault.
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "md")
                && frontmatter_contains(&path, &needle)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn frontmatter_contains(path: &Path, needle: &str) -> bool {
    use std::io::BufRead;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "unreadable note skipped while looking for it by id");
            }
            return false;
        }
    };
    std::io::BufReader::new(file)
        .lines()
        .take(40)
        .map_while(std::result::Result::ok)
        .any(|line| line.trim() == needle)
}

/// Writes the note into `dir` (created if missing) and returns its path.
///
/// Never overwrites (PDA OUT-6): if a note with that name exists and differs,
/// the new one gets ` (2)`, ` (3)`, …. If an existing note already has the
/// same content (ignoring the `transcribed:` date), it's reused, so running
/// the same link twice doesn't pile up copies.
pub async fn write_note(
    dir: &Path,
    meta: &VideoMeta,
    transcript: &Transcript,
    cleaned: Option<&Cleaned>,
    today: &str,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(fs_err("create folder", dir))?;
    let file_name = note_filename(
        meta.upload_date.as_deref(),
        &note_title(meta, transcript, cleaned),
    );
    let stem = file_name.trim_end_matches(".md");
    let body = render_note(meta, transcript, cleaned, today);

    for n in 1..=MAX_COPIES {
        let candidate = if n == 1 {
            dir.join(&file_name)
        } else {
            dir.join(format!("{stem} ({n}).md"))
        };
        // `create_new` makes "exists?" and "create" one atomic step, so a
        // concurrent writer can't slip in between and get overwritten.
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(mut file) => {
                file.write_all(body.as_bytes())
                    .await
                    .map_err(fs_err("write", &candidate))?;
                file.flush().await.map_err(fs_err("write", &candidate))?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match tokio::fs::read_to_string(&candidate).await {
                    Ok(existing) if same_note(&existing, &body) => return Ok(candidate),
                    Ok(_) => {}
                    // Unreadable (permissions, not UTF-8): it can't be the
                    // same note, so move on, but leave a trace.
                    Err(e) => {
                        tracing::warn!(path = %candidate.display(), error = %e, "existing note unreadable")
                    }
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
        s.lines()
            .filter(|l| !l.starts_with("transcribed: "))
            .collect::<Vec<_>>()
            .join("\n")
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
                .map(|(i, t)| Segment {
                    start_ms: i as u64 * 1000,
                    end_ms: i as u64 * 1000 + 900,
                    text: t.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn filenames() {
        assert_eq!(
            note_filename(Some("2005-04-24"), "Me at the zoo"),
            "2005-04-24 Me at the zoo.md"
        );
        assert_eq!(note_filename(None, "Me at the zoo"), "Me at the zoo.md");
        assert_eq!(note_filename(None, "What is UI/UX?"), "What is UI UX.md");
        assert_eq!(note_filename(None, r#"a\b: "c" <d>|e*"#), "a b c d e.md");
        assert_eq!(
            note_filename(None, "  ..dots and spaces..  "),
            "dots and spaces.md"
        );
        assert_eq!(
            note_filename(None, "tab\tand\nnewline"),
            "tab and newline.md"
        );
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
        assert_eq!(
            note_title(&zoo(), &transcript(&["anything"]), None),
            "Me at the zoo"
        );
    }

    #[test]
    fn reel_title_comes_from_the_transcript() {
        let t = transcript(&[
            "Three things nobody tells you about",
            "designing onboarding flows for apps",
        ]);
        assert_eq!(
            note_title(&reel(), &t, None),
            "Three things nobody tells you about designing onboarding"
        );
        assert_eq!(
            note_title(&reel(), &transcript(&[]), None),
            "Instagram reel DPx0yXkkS5M"
        );
    }

    #[test]
    fn renders_frontmatter_and_body() {
        let note = render_note(
            &zoo(),
            &transcript(&["Alright, so here we are.", "That's cool."]),
            None,
            "2026-10-11",
        );
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
        let note = render_note(&meta, &t, None, "2026-10-11");
        assert!(
            note.contains(r#"title: "He said \"hi\"\nthen left""#),
            "{note}"
        );
        assert!(!note.contains("channel:"));
        assert!(!note.contains("upload_date:"));
        assert!(!note.contains("duration:"));
        assert!(!note.contains("language:"));
    }

    #[tokio::test]
    async fn write_note_never_overwrites_a_different_note() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested/Inbox");
        let p1 = write_note(&out, &zoo(), &transcript(&["first"]), None, "2026-10-11")
            .await
            .unwrap();
        assert_eq!(p1, out.join("2005-04-24 Me at the zoo.md"));
        let p2 = write_note(&out, &zoo(), &transcript(&["second"]), None, "2026-10-11")
            .await
            .unwrap();
        assert_eq!(p2, out.join("2005-04-24 Me at the zoo (2).md"));
        let p3 = write_note(&out, &zoo(), &transcript(&["third"]), None, "2026-10-11")
            .await
            .unwrap();
        assert_eq!(p3, out.join("2005-04-24 Me at the zoo (3).md"));
        assert!(std::fs::read_to_string(&p1).unwrap().ends_with("first\n"));
        assert!(std::fs::read_to_string(&p2).unwrap().ends_with("second\n"));
    }

    #[tokio::test]
    async fn rerunning_the_same_link_reuses_the_note_even_on_another_day() {
        let dir = tempfile::tempdir().unwrap();
        let t = transcript(&["same words"]);
        let p1 = write_note(dir.path(), &zoo(), &t, None, "2026-10-11")
            .await
            .unwrap();
        let p2 = write_note(dir.path(), &zoo(), &t, None, "2026-10-12")
            .await
            .unwrap();
        assert_eq!(p1, p2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(
            std::fs::read_to_string(&p1)
                .unwrap()
                .contains("transcribed: 2026-10-11")
        );
    }

    #[tokio::test]
    async fn an_edited_note_is_not_reused_or_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let t = transcript(&["same words"]);
        let p1 = write_note(dir.path(), &zoo(), &t, None, "2026-10-11")
            .await
            .unwrap();
        std::fs::write(&p1, "my own edits").unwrap();
        let p2 = write_note(dir.path(), &zoo(), &t, None, "2026-10-11")
            .await
            .unwrap();
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
                    let path = write_note(&dir, &zoo(), &transcript(&[&words]), None, "2026-10-11")
                        .await
                        .unwrap();
                    (path, words)
                })
            })
            .collect();
        let mut paths = std::collections::HashSet::new();
        for task in tasks {
            let (path, words) = task.await.unwrap();
            assert!(
                std::fs::read_to_string(&path)
                    .unwrap()
                    .ends_with(&format!("{words}\n"))
            );
            assert!(paths.insert(path), "two writers got the same file");
        }
        assert_eq!(paths.len(), 8);
    }

    #[test]
    fn devanagari_titles_fit_the_filesystem_byte_limit() {
        let name = note_filename(Some("2026-10-07"), &"दृश्यम ".repeat(40));
        assert!(
            name.len() <= MAX_FILENAME_BYTES + ".md".len(),
            "{} bytes",
            name.len()
        );
    }

    fn cleaned_text(text: &str, review: bool) -> Cleaned {
        Cleaned {
            text: text.into(),
            translated: true,
            review,
            model: "lmstudio/google/gemma-4-31b".into(),
            raw_words: 10,
            clean_words: 5,
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    #[test]
    fn cleaned_notes_carry_the_cleanup_fields_and_body() {
        let meta = VideoMeta {
            language: Some("hi".into()),
            ..zoo()
        };
        let mut t = transcript(&["दोस्तों नमस्ते"]);
        t.language = Some("hi".into());
        let note = render_note(
            &meta,
            &t,
            Some(&cleaned_text("Hello friends.", true)),
            "2026-10-11",
        );
        assert!(note.contains("language: hi\ntranslated: true\nsource: whisper\ncleaned_by: \"lmstudio/google/gemma-4-31b\"\ncleanup: review\ntranscribed: 2026-10-11"), "{note}");
        assert!(note.ends_with("---\n\nHello friends.\n"));
        assert!(
            !note.contains("दोस्तों"),
            "the raw transcript stays in the database"
        );
    }

    #[test]
    fn reel_titles_prefer_the_cleaned_english_text() {
        let t = transcript(&["तो दोस्तों आज हम बात करेंगे"]);
        let c = cleaned_text(
            "Today we talk about cancel flows and dark patterns in apps",
            false,
        );
        assert_eq!(
            note_title(&reel(), &t, Some(&c)),
            "Today we talk about cancel flows and dark"
        );
    }

    #[tokio::test]
    async fn update_note_rewrites_only_unedited_notes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "version one").unwrap();
        let known = vec![sha256_hex("version one")];
        assert!(update_note(&path, &known, "version two").await.unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version two");
        // Now "version two" is on disk, which isn't a known hash: an edit.
        assert!(!update_note(&path, &known, "version three").await.unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version two");
        assert!(
            !update_note(&dir.path().join("gone.md"), &known, "x")
                .await
                .unwrap()
        );
    }

    #[test]
    fn notes_are_found_by_id_wherever_they_were_moved() {
        let dir = tempfile::tempdir().unwrap();
        let moved = dir.path().join("Design/UX/My name for it.md");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(
            &moved,
            "---\ntitle: \"x\"\nshruti_id: \"youtube:jNQXAC9IVRw\"\n---\n\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("other.md"),
            "---\nshruti_id: \"youtube:other\"\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".obsidian")).unwrap();
        std::fs::write(
            dir.path().join(".obsidian/cache.md"),
            "shruti_id: \"youtube:jNQXAC9IVRw\"",
        )
        .unwrap();
        assert_eq!(
            find_notes_by_id(dir.path(), "youtube:jNQXAC9IVRw").unwrap(),
            vec![moved]
        );
        assert!(
            find_notes_by_id(&dir.path().join("missing"), "x")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn markup_in_transcripts_is_never_live_in_the_note() {
        assert_eq!(
            neutralize_html("<script>alert(1)</script> and a < b, x<3"),
            "&lt;script>alert(1)&lt;/script> and a < b, x<3"
        );
        assert_eq!(
            neutralize_html("<img src=x onerror=y> <!-- c -->"),
            "&lt;img src=x onerror=y> &lt;!-- c -->"
        );
        let mut t = transcript(&["<iframe src=evil>"]);
        t.language = Some("en".into());
        assert!(render_note(&zoo(), &t, None, "2026-10-11").ends_with("&lt;iframe src=evil>\n"));
    }

    #[test]
    fn fingerprints_are_stable_hex() {
        assert_eq!(sha256_hex("").len(), 64);
        assert_eq!(sha256_hex("a"), sha256_hex("a"));
        assert_ne!(sha256_hex("a"), sha256_hex("b"));
    }
}
