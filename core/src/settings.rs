//! The app's saved settings (PDA UI-5), in `~/.config/shruti/settings.json`.
//!
//! Every field is optional: unset means the built-in default, so a new
//! default reaches anyone who never changed that setting. API keys are never
//! here; they live in the keyring (LLM-5). `SHRUTI_*` environment variables
//! still win over the file, for development.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::fs_err;
use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The `Shruti/` root inside the vault.
    pub out_dir: Option<PathBuf>,
    pub database_url: Option<String>,
    /// `lmstudio`, `openai`, `gemini` or `custom`.
    pub llm_provider: Option<String>,
    pub llm_model: Option<String>,
    pub llm_base_url: Option<String>,
    /// Cloud providers only; local is always 1 (LLM-7).
    pub llm_concurrency: Option<u32>,
    pub chunk_tokens: Option<usize>,
    pub cleanup_prompt: Option<String>,
    /// Guardrail thresholds (CL-6), as cleaned/raw word ratios.
    pub min_ratio: Option<f32>,
    pub max_ratio: Option<f32>,
    pub max_ratio_translated: Option<f32>,
    pub whisper_model: Option<PathBuf>,
    /// `auto`, or a Vulkan device number.
    pub gpu: Option<String>,
    /// Browser to read Instagram cookies from (IG-2).
    pub cookies_browser: Option<String>,
    pub ig_delay_secs: Option<u32>,
    pub ig_batch: Option<u32>,
    pub ig_pause_secs: Option<u32>,
}

impl Settings {
    pub fn path(home: &Path) -> PathBuf {
        home.join(".config/shruti/settings.json")
    }

    /// A missing file is the defaults; an unreadable one is an error, so a
    /// typo never silently resets everything.
    pub fn load(path: &Path) -> Result<Settings> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Parse {
                what: "settings file",
                reason: format!("{}: {e}", path.display()),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(fs_err("read", path)(e)),
        }
    }

    /// Written to a temporary file and renamed, so a crash mid-write never
    /// leaves half a settings file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(fs_err("create", dir))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::Parse {
            what: "settings",
            reason: e.to_string(),
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text + "\n").map_err(fs_err("write", &tmp))?;
        std::fs::rename(&tmp, path).map_err(fs_err("replace", path))
    }

    /// Blank strings mean "back to the default", as cleared form fields.
    pub fn normalized(mut self) -> Settings {
        fn blank(s: &mut Option<String>) {
            if s.as_deref().is_some_and(|v| v.trim().is_empty()) {
                *s = None;
            }
        }
        fn blank_path(p: &mut Option<PathBuf>) {
            if p.as_deref().is_some_and(|v| v.as_os_str().is_empty()) {
                *p = None;
            }
        }
        blank_path(&mut self.out_dir);
        blank(&mut self.database_url);
        blank(&mut self.llm_provider);
        blank(&mut self.llm_model);
        blank(&mut self.llm_base_url);
        blank(&mut self.cleanup_prompt);
        blank_path(&mut self.whisper_model);
        blank(&mut self.gpu);
        blank(&mut self.cookies_browser);
        self
    }
}

/// Where Obsidian (native or Flatpak) keeps its list of vaults.
fn obsidian_configs(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".config/obsidian/obsidian.json"),
        home.join(".var/app/md.obsidian.Obsidian/config/obsidian/obsidian.json"),
    ]
}

/// `Shruti/` inside the vault Obsidian has open, else the one opened most
/// recently (PDA UI-7). `None` when Obsidian lists no vault.
pub fn default_out_dir(home: &Path) -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Config {
        #[serde(default)]
        vaults: std::collections::HashMap<String, Vault>,
    }
    #[derive(Deserialize)]
    struct Vault {
        path: PathBuf,
        #[serde(default)]
        ts: u64,
        #[serde(default)]
        open: bool,
    }
    obsidian_configs(home)
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .filter_map(|text| serde_json::from_str::<Config>(&text).ok())
        .flat_map(|c| c.vaults.into_values())
        .filter(|v| v.path.is_dir())
        .max_by_key(|v| (v.open, v.ts))
        .map(|v| v.path.join("Shruti"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn missing_file_is_defaults_and_saving_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = Settings::path(dir.path());
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());

        let s = Settings {
            llm_provider: Some("gemini".into()),
            min_ratio: Some(0.3),
            ig_batch: Some(10),
            ..Settings::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = Settings::load(&path).unwrap_err().to_string();
        assert!(err.contains("settings file"), "{err}");
    }

    #[test]
    fn the_default_folder_is_in_the_open_obsidian_vault() {
        let home = tempfile::tempdir().unwrap();
        let (a, b) = (home.path().join("Old Vault"), home.path().join("Notes"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(default_out_dir(home.path()), None);

        let flatpak = &obsidian_configs(home.path())[1];
        std::fs::create_dir_all(flatpak.parent().unwrap()).unwrap();
        let config = serde_json::json!({"vaults": {
            "1": {"path": a, "ts": 900},
            "2": {"path": b, "ts": 100, "open": true},
            "3": {"path": home.path().join("deleted"), "ts": 999, "open": true},
        }});
        std::fs::write(flatpak, config.to_string()).unwrap();
        assert_eq!(default_out_dir(home.path()), Some(b.join("Shruti")));
    }

    #[test]
    fn unknown_fields_are_ignored_and_blanks_mean_default() {
        let s: Settings =
            serde_json::from_str(r#"{"llm_model": "  ", "gone_setting": 1, "ig_batch": 5}"#)
                .unwrap();
        let s = s.normalized();
        assert_eq!(s.llm_model, None);
        assert_eq!(s.ig_batch, Some(5));
    }
}
