//! Auto-sorting notes into topic folders (PDA §4.6, FO-1 … FO-9).
//!
//! The vault's `Shruti/` root holds exactly two levels, `Area/Topic`, plus
//! `Inbox/` for notes whose topic can't be told. The LLM picks a folder
//! from the current tree (the disk is the source of truth, FO-8) or
//! proposes a new one; its answer is checked, not trusted: names are
//! normalised to the naming rules (FO-5) and near-duplicates of existing
//! folders are reused instead (FO-3, FO-4). Each folder created gets a
//! one-line description in a hidden `.shruti` file (FO-6).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::fs_err;
use crate::llm::{Llm, LlmError};
use crate::{Error, Result};

pub const INBOX: &str = "Inbox";
const DESCRIPTION_FILE: &str = ".shruti";
const MAX_NAME_WORDS: usize = 3;
const MAX_NAME_CHARS: usize = 32;
/// How much of the note the sorter reads: enough to tell the topic.
const SAMPLE_CHARS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub area: String,
    pub topic: String,
    pub description: Option<String>,
}

impl Folder {
    pub fn path(&self) -> String {
        format!("{}/{}", self.area, self.topic)
    }
}

/// Where a note goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Existing(Folder),
    New(Folder),
    Inbox,
}

impl Placement {
    /// Relative to the root: `Area/Topic` or `Inbox`.
    pub fn relative_path(&self) -> String {
        match self {
            Placement::Existing(f) | Placement::New(f) => f.path(),
            Placement::Inbox => INBOX.to_string(),
        }
    }
}

/// The `Area/Topic` folders currently on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderTree {
    pub folders: Vec<Folder>,
}

impl FolderTree {
    /// Reads the tree under `root`. Hidden folders and `Inbox` are skipped;
    /// a missing root is an empty tree.
    pub fn scan(root: &Path) -> Result<Self> {
        let mut folders = Vec::new();
        for area in visible_dirs(root)? {
            if area == INBOX {
                continue;
            }
            for topic in visible_dirs(&root.join(&area))? {
                let description_file = root.join(&area).join(&topic).join(DESCRIPTION_FILE);
                let description = std::fs::read_to_string(description_file)
                    .ok()
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty());
                folders.push(Folder {
                    area: area.clone(),
                    topic,
                    description,
                });
            }
        }
        folders.sort_by_key(Folder::path);
        Ok(FolderTree { folders })
    }

    fn find(&self, path: &str) -> Option<&Folder> {
        self.folders
            .iter()
            .find(|f| f.path().eq_ignore_ascii_case(path.trim()))
    }

    fn areas(&self) -> Vec<&str> {
        let mut areas: Vec<&str> = self.folders.iter().map(|f| f.area.as_str()).collect();
        areas.dedup();
        areas
    }
}

fn visible_dirs(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(fs_err("read folder", dir)(e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(fs_err("read folder", dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && entry.path().is_dir() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

const SORTER_PROMPT: &str = "\
You file notes made from videos into a two-level folder tree: Area/Topic (for example Design/UX, Engineering/Backend, Travel/Vlogs).

Rules:
- Choose an existing folder whenever one reasonably fits. Create a new one only when none does.
- Never create a near-duplicate of an existing folder (\"UX\", \"UX Design\" and \"User Experience\" are the same folder).
- New names: English, Title Case, 1 to 3 words each, no dates, channel names or emoji. Reuse an existing Area when the topic belongs to it.
- Use the inbox only when the topic genuinely can't be told (music only, empty, a few words).

Reply with only one JSON object, one of:
{\"folder\": \"Area/Topic\"}
{\"new\": {\"area\": \"Area\", \"topic\": \"Topic\", \"description\": \"one line on what belongs here\"}}
{\"inbox\": true}";

#[derive(Deserialize)]
struct SortReply {
    folder: Option<String>,
    new: Option<NewFolder>,
    #[serde(default)]
    inbox: bool,
}

#[derive(Deserialize)]
struct NewFolder {
    area: String,
    topic: String,
    description: Option<String>,
}

/// Picks where a note goes. A reply that can't be used goes to the inbox
/// rather than failing the job; provider errors are returned so the queue
/// can wait and retry.
pub async fn sort<L: Llm>(
    llm: &L,
    tree: &FolderTree,
    title: &str,
    text: &str,
) -> std::result::Result<Placement, LlmError> {
    let listing = if tree.folders.is_empty() {
        "(no folders yet)".to_string()
    } else {
        tree.folders
            .iter()
            .map(|f| match &f.description {
                Some(d) => format!("- {}: {d}", f.path()),
                None => format!("- {}", f.path()),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let sample: String = text.chars().take(SAMPLE_CHARS).collect();
    let user = format!("Existing folders:\n{listing}\n\nNote title: {title}\n\nNote:\n{sample}");
    let reply = llm.chat(SORTER_PROMPT, &user).await?;
    Ok(interpret(tree, &reply.text))
}

/// Turns the model's reply into a checked placement.
pub(crate) fn interpret(tree: &FolderTree, reply: &str) -> Placement {
    let parsed = json_object(reply).and_then(|j| serde_json::from_str::<SortReply>(j).ok());
    let Some(parsed) = parsed else {
        tracing::warn!(
            reply,
            "the folder sorter's reply wasn't usable; filing to Inbox"
        );
        return Placement::Inbox;
    };
    if parsed.inbox {
        return Placement::Inbox;
    }
    if let Some(path) = parsed.folder {
        if let Some(existing) = tree.find(&path) {
            return Placement::Existing(existing.clone());
        }
        // It named a folder that doesn't exist: treat it as a proposal.
        if let Some((area, topic)) = path.split_once('/') {
            return propose(tree, area, topic, None);
        }
    }
    match parsed.new {
        Some(new) => propose(tree, &new.area, &new.topic, new.description),
        None => Placement::Inbox,
    }
}

/// Applies the naming rules and the near-duplicate check to a proposal.
fn propose(tree: &FolderTree, area: &str, topic: &str, description: Option<String>) -> Placement {
    let (Some(area), Some(topic)) = (clean_name(area), clean_name(topic)) else {
        return Placement::Inbox;
    };
    // A known area under a different spelling: use its exact name.
    let area = tree
        .areas()
        .into_iter()
        .find(|a| similar(a, &area))
        .map_or(area, str::to_string);
    if let Some(existing) = tree
        .folders
        .iter()
        .find(|f| f.area == area && similar(&f.topic, &topic))
    {
        return Placement::Existing(existing.clone());
    }
    Placement::New(Folder {
        area,
        topic,
        description: description
            .map(|d| d.trim().chars().take(200).collect::<String>())
            .filter(|d| !d.is_empty()),
    })
}

/// FO-5: 1–3 English words, Title Case, no dates or symbols. `None` if
/// nothing usable is left.
pub(crate) fn clean_name(raw: &str) -> Option<String> {
    let words: Vec<String> = raw
        .split(|c: char| c.is_whitespace() || c == '_' || c == '/')
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '&' || *c == '-')
                .collect::<String>()
        })
        .filter(|w| !w.is_empty() && !w.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .take(MAX_NAME_WORDS)
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                // Keep the rest as written so acronyms like UX and AI stay.
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => w,
            }
        })
        .collect();
    let name: String = words.join(" ").chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// FO-4: the same folder under different spelling, case, plural or
/// wording ("UX" / "UX Design" / "Ux designs").
pub(crate) fn similar(a: &str, b: &str) -> bool {
    let (a, b) = (normalise(a), normalise(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    // "ux" inside "uxdesign": a short name extended with a generic word.
    if let Some(rest) = long.strip_prefix(short.as_str())
        && GENERIC_SUFFIXES.contains(&rest)
    {
        return true;
    }
    short.len() >= 5 && edit_distance(short, long) <= 1
}

const GENERIC_SUFFIXES: &[&str] = &[
    "design", "designs", "stuff", "general", "basics", "tips", "notes", "videos", "content",
];

fn normalise(name: &str) -> String {
    let lower: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match lower.strip_suffix('s') {
        Some(stem) if stem.len() >= 2 => stem.to_string(),
        _ => lower,
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The JSON object inside a reply that may wrap it in prose or fences.
fn json_object(reply: &str) -> Option<&str> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    (end > start).then(|| &reply[start..=end])
}

/// Makes sure the placement's folder exists (writing `.shruti` for a new
/// one) and returns the directory the note goes in. `Inbox` is recreated if
/// it was deleted (FO-7).
pub fn place(root: &Path, placement: &Placement) -> Result<PathBuf> {
    let dir = root.join(placement.relative_path());
    std::fs::create_dir_all(&dir).map_err(fs_err("create folder", &dir))?;
    if let Placement::New(folder) = placement
        && let Some(description) = &folder.description
    {
        let file = dir.join(DESCRIPTION_FILE);
        if !file.exists() {
            std::fs::write(&file, format!("{description}\n")).map_err(fs_err("write", &file))?;
        }
    }
    Ok(dir)
}

/// Parses a stored `Area/Topic` or `Inbox` back into a placement.
pub fn from_relative_path(tree: &FolderTree, path: &str) -> Result<Placement> {
    if path == INBOX {
        return Ok(Placement::Inbox);
    }
    if let Some(existing) = tree.find(path) {
        return Ok(Placement::Existing(existing.clone()));
    }
    let (area, topic) = path
        .split_once('/')
        .ok_or_else(|| Error::State(format!("stored folder {path:?} isn't Area/Topic")))?;
    Ok(Placement::New(Folder {
        area: area.to_string(),
        topic: topic.to_string(),
        description: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatReply;
    use pretty_assertions::assert_eq;

    fn folder(area: &str, topic: &str, description: Option<&str>) -> Folder {
        Folder {
            area: area.into(),
            topic: topic.into(),
            description: description.map(Into::into),
        }
    }

    fn tree() -> FolderTree {
        FolderTree {
            folders: vec![
                folder("Design", "UX", Some("UX design teardowns and critiques")),
                folder("Engineering", "Backend", None),
            ],
        }
    }

    #[test]
    fn scans_two_levels_with_descriptions_and_skips_inbox_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Design/UX")).unwrap();
        std::fs::write(root.join("Design/UX/.shruti"), "UX design teardowns\n").unwrap();
        std::fs::create_dir_all(root.join("Engineering/Backend")).unwrap();
        std::fs::create_dir_all(root.join("Inbox")).unwrap();
        std::fs::create_dir_all(root.join(".obsidian/plugins")).unwrap();
        std::fs::write(root.join("Design/loose note.md"), "x").unwrap();
        let tree = FolderTree::scan(root).unwrap();
        assert_eq!(
            tree.folders,
            vec![
                folder("Design", "UX", Some("UX design teardowns")),
                folder("Engineering", "Backend", None),
            ]
        );
        assert_eq!(
            FolderTree::scan(&root.join("missing")).unwrap(),
            FolderTree::default()
        );
    }

    #[test]
    fn existing_folders_are_matched_case_insensitively() {
        assert_eq!(
            interpret(&tree(), r#"{"folder": "design/ux"}"#),
            Placement::Existing(folder(
                "Design",
                "UX",
                Some("UX design teardowns and critiques")
            ))
        );
    }

    #[test]
    fn near_duplicates_reuse_the_existing_folder() {
        for proposal in [
            r#"{"new": {"area": "Design", "topic": "UX Design", "description": "x"}}"#,
            r#"{"new": {"area": "design", "topic": "ux", "description": "x"}}"#,
            r#"{"new": {"area": "Designs", "topic": "UXs"}}"#,
            r#"{"folder": "Design/UX Designs"}"#,
            r#"{"new": {"area": "Engineering", "topic": "Backends"}}"#,
            r#"{"new": {"area": "Engineering", "topic": "Bakend"}}"#,
        ] {
            assert!(
                matches!(interpret(&tree(), proposal), Placement::Existing(_)),
                "{proposal}"
            );
        }
    }

    #[test]
    fn genuinely_new_topics_are_created_under_a_known_area_spelling() {
        assert_eq!(
            interpret(
                &tree(),
                r#"{"new": {"area": "design", "topic": "typography", "description": "Type and fonts"}}"#
            ),
            Placement::New(folder("Design", "Typography", Some("Type and fonts")))
        );
        assert_eq!(
            interpret(
                &tree(),
                r#"Sure! {"new": {"area": "Travel", "topic": "Vlogs"}}"#
            ),
            Placement::New(folder("Travel", "Vlogs", None))
        );
    }

    #[test]
    fn names_follow_the_rules() {
        assert_eq!(
            clean_name("user experience research methods").as_deref(),
            Some("User Experience Research")
        );
        assert_eq!(clean_name("UX").as_deref(), Some("UX"));
        assert_eq!(clean_name("  ai/ml  ").as_deref(), Some("Ai Ml"));
        assert_eq!(clean_name("2026 Vlogs 🎥").as_deref(), Some("Vlogs"));
        assert_eq!(clean_name("2026-10-11 Vlogs").as_deref(), Some("Vlogs"));
        assert_eq!(clean_name("../../etc").as_deref(), Some("Etc"));
        assert_eq!(clean_name("🎥 2026"), None);
    }

    #[test]
    fn unusable_replies_go_to_the_inbox() {
        for reply in [
            "",
            "I think Design/UX",
            "{not json}",
            r#"{"new": {"area": "!!!", "topic": "???"}}"#,
        ] {
            assert_eq!(interpret(&tree(), reply), Placement::Inbox, "{reply:?}");
        }
        assert_eq!(interpret(&tree(), r#"{"inbox": true}"#), Placement::Inbox);
    }

    #[test]
    fn similarity_is_not_too_eager() {
        assert!(!similar("UX", "UI"));
        assert!(!similar("Backend", "Frontend"));
        // Short names need an exact match: one letter apart is a different topic.
        assert!(!similar("Vlogs", "Blogs"));
        assert!(similar("Typography", "Typograpy"));
        assert!(similar("UX", "Ux Designs"));
    }

    #[test]
    fn placing_creates_folders_descriptions_and_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let new = Placement::New(folder("Travel", "Vlogs", Some("Travel vlogs")));
        let path = place(dir.path(), &new).unwrap();
        assert_eq!(path, dir.path().join("Travel/Vlogs"));
        assert_eq!(
            std::fs::read_to_string(path.join(".shruti")).unwrap(),
            "Travel vlogs\n"
        );
        assert_eq!(
            place(dir.path(), &Placement::Inbox).unwrap(),
            dir.path().join("Inbox")
        );
        // An existing description (maybe edited by hand) is never replaced.
        std::fs::write(path.join(".shruti"), "my words\n").unwrap();
        place(dir.path(), &new).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join(".shruti")).unwrap(),
            "my words\n"
        );
    }

    #[test]
    fn stored_placements_round_trip() {
        let t = tree();
        assert_eq!(from_relative_path(&t, "Inbox").unwrap(), Placement::Inbox);
        assert!(matches!(
            from_relative_path(&t, "Design/UX").unwrap(),
            Placement::Existing(_)
        ));
        assert!(matches!(
            from_relative_path(&t, "Travel/Vlogs").unwrap(),
            Placement::New(_)
        ));
        assert!(from_relative_path(&t, "nonsense").is_err());
    }

    struct OneReply(&'static str);

    impl Llm for OneReply {
        async fn chat(&self, system: &str, user: &str) -> std::result::Result<ChatReply, LlmError> {
            assert!(system.contains("Area/Topic"));
            assert!(user.contains("- Design/UX: UX design teardowns and critiques"));
            assert!(user.contains("Note title: Cancel flows"));
            Ok(ChatReply {
                text: self.0.into(),
                tokens_in: None,
                tokens_out: None,
            })
        }
        fn label(&self) -> String {
            "fake".into()
        }
    }

    #[tokio::test]
    async fn sort_shows_the_tree_and_checks_the_answer() {
        let placement = sort(
            &OneReply(r#"{"folder": "Design/UX"}"#),
            &tree(),
            "Cancel flows",
            "Dark patterns…",
        )
        .await
        .unwrap();
        assert!(matches!(placement, Placement::Existing(f) if f.topic == "UX"));
    }
}
