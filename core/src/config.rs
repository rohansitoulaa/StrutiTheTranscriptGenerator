//! Where the tools and models live, and how the queue behaves: built-in
//! defaults, then the saved settings (PDA UI-5), then `SHRUTI_*` variables.

use std::path::{Path, PathBuf};

use crate::cleanup::CleanupSettings;
use crate::llm::{LlmClient, Provider, ProviderKind};
use crate::pipeline::Engine;
use crate::queue::Limits;
use crate::settings::Settings;
use crate::whisper::WhisperCli;
use crate::ytdlp::YtDlp;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuChoice {
    /// Probe for the first discrete GPU.
    Auto,
    Device(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub ytdlp_bin: PathBuf,
    pub js_runtime: Option<String>,
    pub cookies_from_browser: Option<String>,
    pub whisper_bin: PathBuf,
    pub whisper_model: PathBuf,
    pub vad_model: Option<PathBuf>,
    pub gpu: GpuChoice,
    pub threads: Option<u32>,
    pub database_url: String,
    /// Audio waiting between the download and Whisper stages.
    pub work_dir: PathBuf,
    pub limits: Limits,
    pub llm: LlmSettings,
    /// Transcript tokens per cleanup request; `None` picks one for the
    /// provider (small for a local model's 4k context, large for cloud).
    pub chunk_tokens: Option<usize>,
    /// Replaces the built-in cleanup prompt.
    pub cleanup_prompt: Option<String>,
    pub guardrail: Guardrail,
}

/// Cleaned/raw word ratios outside these flag a note for review (CL-6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guardrail {
    pub min_ratio: f32,
    pub max_ratio: f32,
    pub max_ratio_translated: f32,
}

impl Default for Guardrail {
    fn default() -> Self {
        let d = CleanupSettings::default();
        Guardrail {
            min_ratio: d.min_ratio,
            max_ratio: d.max_ratio,
            max_ratio_translated: d.max_ratio_translated,
        }
    }
}

/// Which model does cleanup and sorting (PDA CL-7, LLM-1). The API key is
/// not here: it lives in the keyring (LLM-5) and is read when needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmSettings {
    pub kind: ProviderKind,
    pub model: String,
    /// Overrides the provider's default; required for `custom`.
    pub base_url: Option<String>,
}

impl Default for LlmSettings {
    fn default() -> Self {
        LlmSettings {
            kind: ProviderKind::LmStudio,
            // The model already installed in LM Studio (PDA CL-7).
            model: "google/gemma-4-31b".into(),
            base_url: None,
        }
    }
}

impl Config {
    /// Defaults for a checkout of this repo: whisper-cli built under
    /// `vendor/`, models under `models/`, yt-dlp and node from `PATH`, the
    /// local Postgres `shruti` database over its Unix socket as `user`.
    pub fn defaults(repo_root: &Path, home: &Path, user: &str) -> Self {
        Config {
            ytdlp_bin: "yt-dlp".into(),
            js_runtime: Some("node".into()),
            cookies_from_browser: None,
            whisper_bin: repo_root.join("vendor/whisper.cpp/build/bin/whisper-cli"),
            whisper_model: repo_root.join("models/ggml-large-v3-turbo-q5_0.bin"),
            vad_model: Some(repo_root.join("models/ggml-silero-v5.1.2.bin")),
            gpu: GpuChoice::Auto,
            threads: None,
            database_url: local_database_url(user, "shruti"),
            work_dir: home.join(".cache/shruti/work"),
            limits: Limits::default(),
            llm: LlmSettings::default(),
            chunk_tokens: None,
            cleanup_prompt: None,
            guardrail: Guardrail::default(),
        }
    }

    /// Defaults for the installed app: the bundled tools sit next to its
    /// executable (`shruti-whisper-cli`, `shruti-yt-dlp`), and the models
    /// are downloaded into `data_dir` on first launch (PDA UI-7).
    pub fn packaged(exe_dir: &Path, data_dir: &Path, home: &Path, user: &str) -> Self {
        let models = data_dir.join("models");
        Config {
            ytdlp_bin: exe_dir.join("shruti-yt-dlp"),
            whisper_bin: exe_dir.join("shruti-whisper-cli"),
            whisper_model: models.join("ggml-large-v3-turbo-q5_0.bin"),
            vad_model: Some(models.join("ggml-silero-v5.1.2.bin")),
            ..Config::defaults(exe_dir, home, user)
        }
    }

    /// Repo defaults, then `settings`, then `SHRUTI_*` variables.
    pub fn load(
        repo_root: &Path,
        settings: &Settings,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let home = PathBuf::from(var("HOME").unwrap_or_default());
        let user = var("USER").unwrap_or_default();
        Config::load_over(Config::defaults(repo_root, &home, &user), settings, var)
    }

    /// `defaults`, then `settings`, then `SHRUTI_*` variables.
    pub fn load_over(
        mut defaults: Config,
        settings: &Settings,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        defaults.apply_settings(settings)?;
        defaults.apply_env(&var)?;
        Ok(defaults)
    }

    fn apply_settings(&mut self, s: &Settings) -> Result<()> {
        if let Some(url) = &s.database_url {
            self.database_url = url.clone();
        }
        if let Some(name) = &s.llm_provider {
            self.set_provider(name)?;
        }
        if let Some(model) = &s.llm_model {
            self.llm.model = model.clone();
        }
        if s.llm_base_url.is_some() {
            self.llm.base_url = s.llm_base_url.clone();
        }
        if let Some(n) = s.llm_concurrency
            && !self.llm.kind.is_local()
        {
            self.limits.llm = n.max(1) as usize;
        }
        if s.chunk_tokens.is_some() {
            self.chunk_tokens = s.chunk_tokens.map(|n| n.max(100));
        }
        if s.cleanup_prompt.is_some() {
            self.cleanup_prompt = s.cleanup_prompt.clone();
        }
        let g = &mut self.guardrail;
        g.min_ratio = s.min_ratio.unwrap_or(g.min_ratio);
        g.max_ratio = s.max_ratio.unwrap_or(g.max_ratio);
        g.max_ratio_translated = s.max_ratio_translated.unwrap_or(g.max_ratio_translated);
        if let Some(model) = &s.whisper_model {
            self.whisper_model = model.clone();
        }
        if let Some(gpu) = &s.gpu {
            self.gpu = parse_gpu("gpu", gpu)?;
        }
        if s.cookies_browser.is_some() {
            self.cookies_from_browser = s.cookies_browser.clone();
        }
        let ig = &mut self.limits.instagram;
        if let Some(secs) = s.ig_delay_secs {
            ig.gap = std::time::Duration::from_secs(secs.into());
        }
        if let Some(n) = s.ig_batch {
            ig.batch = n;
        }
        if let Some(secs) = s.ig_pause_secs {
            ig.pause = std::time::Duration::from_secs(secs.into());
        }
        Ok(())
    }

    /// Cloud providers run a few requests at once (LLM-7); local is 1.
    fn set_provider(&mut self, name: &str) -> Result<()> {
        self.llm.kind = ProviderKind::parse(name).ok_or_else(|| Error::Parse {
            what: "settings",
            reason: format!("LLM provider {name:?} is not one of lmstudio, openai, gemini, custom"),
        })?;
        self.limits.llm = if self.llm.kind.is_local() { 1 } else { 2 };
        Ok(())
    }

    /// [`Config::defaults`] (with `HOME` and `USER`) overridden by
    /// `SHRUTI_*` variables, ignoring the settings file.
    pub fn from_env(repo_root: &Path, var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        Config::load(repo_root, &Settings::default(), var)
    }

    /// `SHRUTI_*` variables. An empty value turns an optional setting off.
    fn apply_env(&mut self, var: &impl Fn(&str) -> Option<String>) -> Result<()> {
        let c = self;
        let optional = |v: String| (!v.is_empty()).then_some(v);
        let number = |name: &'static str, v: &str| {
            v.parse::<u32>().map_err(|_| Error::Parse {
                what: "settings",
                reason: format!("{name}={v:?} is not a number"),
            })
        };

        if let Some(v) = var("SHRUTI_YTDLP") {
            c.ytdlp_bin = v.into();
        }
        if let Some(v) = var("SHRUTI_JS_RUNTIME") {
            c.js_runtime = optional(v);
        }
        if let Some(v) = var("SHRUTI_COOKIES_BROWSER") {
            c.cookies_from_browser = optional(v);
        }
        if let Some(v) = var("SHRUTI_WHISPER_BIN") {
            c.whisper_bin = v.into();
        }
        if let Some(v) = var("SHRUTI_WHISPER_MODEL") {
            c.whisper_model = v.into();
        }
        if let Some(v) = var("SHRUTI_VAD_MODEL") {
            c.vad_model = optional(v).map(PathBuf::from);
        }
        if let Some(v) = var("SHRUTI_GPU") {
            c.gpu = match v.as_str() {
                "" | "auto" => GpuChoice::Auto,
                n => GpuChoice::Device(number("SHRUTI_GPU", n)?),
            };
        }
        if let Some(v) = var("SHRUTI_DATABASE_URL").filter(|v| !v.is_empty()) {
            c.database_url = v;
        }
        if let Some(v) = var("SHRUTI_WORK_DIR").filter(|v| !v.is_empty()) {
            c.work_dir = v.into();
        }
        if let Some(v) = var("SHRUTI_DOWNLOADS").filter(|v| !v.is_empty()) {
            c.limits.downloads = number("SHRUTI_DOWNLOADS", &v)?.max(1) as usize;
        }
        // Instagram pacing (PDA IG-3).
        let ig = &mut c.limits.instagram;
        if let Some(v) = var("SHRUTI_IG_DELAY_SECS").filter(|v| !v.is_empty()) {
            ig.gap = std::time::Duration::from_secs(number("SHRUTI_IG_DELAY_SECS", &v)?.into());
        }
        if let Some(v) = var("SHRUTI_IG_BATCH").filter(|v| !v.is_empty()) {
            ig.batch = number("SHRUTI_IG_BATCH", &v)?;
        }
        if let Some(v) = var("SHRUTI_IG_PAUSE_SECS").filter(|v| !v.is_empty()) {
            ig.pause = std::time::Duration::from_secs(number("SHRUTI_IG_PAUSE_SECS", &v)?.into());
        }
        if let Some(v) = var("SHRUTI_LLM_PROVIDER").filter(|v| !v.is_empty()) {
            c.llm.kind = ProviderKind::parse(&v).ok_or_else(|| Error::Parse {
                what: "settings",
                reason: format!(
                    "SHRUTI_LLM_PROVIDER={v:?} is not one of lmstudio, openai, gemini, custom"
                ),
            })?;
            // Cloud providers can take a few requests at once (LLM-7).
            if !c.llm.kind.is_local() {
                c.limits.llm = 2;
            }
        }
        if let Some(v) = var("SHRUTI_LLM_MODEL").filter(|v| !v.is_empty()) {
            c.llm.model = v;
        }
        if let Some(v) = var("SHRUTI_LLM_BASE_URL") {
            c.llm.base_url = optional(v);
        }
        if let Some(v) = var("SHRUTI_LLM_CONCURRENCY").filter(|v| !v.is_empty()) {
            c.limits.llm = number("SHRUTI_LLM_CONCURRENCY", &v)?.max(1) as usize;
        }
        if let Some(v) = var("SHRUTI_CHUNK_TOKENS").filter(|v| !v.is_empty()) {
            c.chunk_tokens = Some(number("SHRUTI_CHUNK_TOKENS", &v)?.max(100) as usize);
        }
        if let Some(v) = var("SHRUTI_THREADS") {
            c.threads = if v.is_empty() {
                None
            } else {
                Some(number("SHRUTI_THREADS", &v)?)
            };
        }
        Ok(())
    }

    /// Missing tools and files, as readable lines. Empty means ready.
    /// Programs given as a bare name (`yt-dlp`) are looked up on `PATH`, the
    /// same way they'll be found when spawned.
    pub fn problems(&self) -> Vec<String> {
        let mut problems: Vec<String> = [
            ("yt-dlp", &self.ytdlp_bin),
            ("whisper-cli", &self.whisper_bin),
        ]
        .into_iter()
        .filter(|(_, bin)| !program_exists(bin))
        .map(|(what, bin)| format!("{what} not found at {}", bin.display()))
        .collect();
        let mut files = vec![("Whisper model", &self.whisper_model)];
        if let Some(vad) = &self.vad_model {
            files.push(("VAD model", vad));
        }
        problems.extend(
            files
                .into_iter()
                .filter(|(_, path)| !path.is_file())
                .map(|(what, path)| format!("{what} not found at {}", path.display())),
        );
        problems
    }

    pub fn llm_base_url(&self) -> Result<String> {
        match (&self.llm.base_url, self.llm.kind.default_base_url()) {
            (Some(url), _) => Ok(url.clone()),
            (None, Some(url)) => Ok(url.to_string()),
            (None, None) => Err(Error::Parse {
                what: "settings",
                reason: "a custom LLM provider needs SHRUTI_LLM_BASE_URL".into(),
            }),
        }
    }

    /// The LLM client, with its API key read from the keyring for cloud
    /// providers. A missing key isn't an error here: the provider rejects
    /// the request and jobs wait with that reason (CL-8).
    pub async fn build_llm(&self) -> Result<LlmClient> {
        let kind = self.llm.kind;
        let api_key = if kind.is_local() {
            None
        } else {
            tokio::task::spawn_blocking(move || crate::secrets::api_key(kind))
                .await
                .map_err(|e| Error::Keyring(e.to_string()))??
        };
        let provider = Provider {
            kind,
            base_url: self.llm_base_url()?,
            api_key,
            model: self.llm.model.clone(),
        };
        Ok(LlmClient::new(provider)?)
    }

    pub fn cleanup_settings(&self) -> CleanupSettings {
        let defaults = CleanupSettings::default();
        let chunk_tokens = self.chunk_tokens.unwrap_or(if self.llm.kind.is_local() {
            defaults.chunk_tokens
        } else {
            8_000
        });
        CleanupSettings {
            prompt: self.cleanup_prompt.clone().unwrap_or(defaults.prompt),
            chunk_tokens,
            min_ratio: self.guardrail.min_ratio,
            max_ratio: self.guardrail.max_ratio,
            max_ratio_translated: self.guardrail.max_ratio_translated,
        }
    }

    /// Builds the engine, probing for the discrete GPU when set to `Auto`.
    pub async fn build_engine(&self) -> Result<Engine<YtDlp, WhisperCli>> {
        let mut whisper = WhisperCli {
            bin: self.whisper_bin.clone(),
            model: self.whisper_model.clone(),
            vad_model: self.vad_model.clone(),
            gpu_device: None,
            threads: self.threads,
        };
        whisper.gpu_device = match self.gpu {
            GpuChoice::Device(n) => Some(n),
            GpuChoice::Auto => whisper.detect_discrete_gpu().await?,
        };
        tracing::info!(gpu = ?whisper.gpu_device, "whisper device");

        let ytdlp = YtDlp {
            bin: self.ytdlp_bin.clone(),
            js_runtime: self.js_runtime.clone(),
            cookies_from_browser: self.cookies_from_browser.clone(),
        };
        Ok(Engine {
            fetcher: ytdlp,
            transcriber: whisper,
        })
    }
}

fn parse_gpu(name: &str, v: &str) -> Result<GpuChoice> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok(GpuChoice::Auto);
    }
    v.parse().map(GpuChoice::Device).map_err(|_| Error::Parse {
        what: "settings",
        reason: format!("{name}={v:?} is neither auto nor a device number"),
    })
}

/// The local server over its Unix socket, as `user` (peer authentication,
/// no password). sqlx needs the socket path percent-encoded as the host.
pub fn local_database_url(user: &str, database: &str) -> String {
    format!("postgres://{user}@%2Fvar%2Frun%2Fpostgresql/{database}")
}

fn program_exists(bin: &Path) -> bool {
    if bin.components().count() > 1 {
        return bin.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn defaults() -> Config {
        Config::defaults(Path::new("/repo"), Path::new("/home/me"), "me")
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn repo_defaults_point_into_the_checkout() {
        let c = defaults();
        assert_eq!(c.ytdlp_bin, PathBuf::from("yt-dlp"));
        assert_eq!(c.js_runtime.as_deref(), Some("node"));
        assert_eq!(c.cookies_from_browser, None);
        assert_eq!(
            c.whisper_bin,
            PathBuf::from("/repo/vendor/whisper.cpp/build/bin/whisper-cli")
        );
        assert_eq!(
            c.whisper_model,
            PathBuf::from("/repo/models/ggml-large-v3-turbo-q5_0.bin")
        );
        assert_eq!(
            c.vad_model,
            Some(PathBuf::from("/repo/models/ggml-silero-v5.1.2.bin"))
        );
        assert_eq!(c.gpu, GpuChoice::Auto);
        assert_eq!(c.threads, None);
        assert_eq!(
            c.database_url,
            "postgres://me@%2Fvar%2Frun%2Fpostgresql/shruti"
        );
        assert_eq!(c.work_dir, PathBuf::from("/home/me/.cache/shruti/work"));
        assert_eq!(c.limits.downloads, 3);
        assert_eq!(c.llm, LlmSettings::default());
        assert_eq!(c.limits.llm, 1);
        assert_eq!(c.cleanup_settings().chunk_tokens, 1_200);
        assert_eq!(c.llm_base_url().unwrap(), "http://localhost:1234/v1");
    }

    #[test]
    fn settings_override_defaults_and_env_overrides_settings() {
        let settings = Settings {
            llm_provider: Some("openai".into()),
            llm_model: Some("gpt-x".into()),
            llm_concurrency: Some(3),
            min_ratio: Some(0.4),
            cleanup_prompt: Some("Keep it all.".into()),
            gpu: Some("0".into()),
            ig_batch: Some(5),
            database_url: Some("postgres://a@b/c".into()),
            ..Settings::default()
        };
        let c = Config::load(
            Path::new("/repo"),
            &settings,
            env(&[("SHRUTI_LLM_MODEL", "from-env")]),
        )
        .unwrap();
        assert_eq!(c.llm.kind, ProviderKind::OpenAi);
        assert_eq!(c.llm.model, "from-env", "env wins over the file");
        assert_eq!(c.limits.llm, 3);
        assert_eq!(c.gpu, GpuChoice::Device(0));
        assert_eq!(c.limits.instagram.batch, 5);
        assert_eq!(c.database_url, "postgres://a@b/c");
        let cleanup = c.cleanup_settings();
        assert_eq!(cleanup.min_ratio, 0.4);
        assert_eq!(cleanup.max_ratio, 1.10, "unset keeps the default");
        assert_eq!(cleanup.prompt, "Keep it all.");

        let local = Settings {
            llm_provider: Some("lmstudio".into()),
            llm_concurrency: Some(4),
            ..Settings::default()
        };
        let c = Config::load(Path::new("/repo"), &local, env(&[])).unwrap();
        assert_eq!(c.limits.llm, 1, "local is always one at a time");

        let bad = Settings {
            gpu: Some("fast".into()),
            ..Settings::default()
        };
        assert!(Config::load(Path::new("/repo"), &bad, env(&[])).is_err());
    }

    #[test]
    fn the_installed_app_uses_its_bundled_tools_and_downloaded_models() {
        let c = Config::packaged(
            Path::new("/usr/bin"),
            Path::new("/home/me/.local/share/shruti"),
            Path::new("/home/me"),
            "me",
        );
        assert_eq!(c.whisper_bin, PathBuf::from("/usr/bin/shruti-whisper-cli"));
        assert_eq!(c.ytdlp_bin, PathBuf::from("/usr/bin/shruti-yt-dlp"));
        assert_eq!(
            c.whisper_model,
            PathBuf::from("/home/me/.local/share/shruti/models/ggml-large-v3-turbo-q5_0.bin")
        );
        assert_eq!(c.work_dir, PathBuf::from("/home/me/.cache/shruti/work"));
        let settings = Settings {
            whisper_model: Some("/big/model.bin".into()),
            ..Settings::default()
        };
        let c = Config::load_over(c, &settings, env(&[("SHRUTI_YTDLP", "/opt/yt-dlp")])).unwrap();
        assert_eq!(c.whisper_model, PathBuf::from("/big/model.bin"));
        assert_eq!(c.ytdlp_bin, PathBuf::from("/opt/yt-dlp"));
    }

    #[test]
    fn llm_settings_from_env() {
        let c = Config::from_env(
            Path::new("/repo"),
            env(&[
                ("SHRUTI_LLM_PROVIDER", "gemini"),
                ("SHRUTI_LLM_MODEL", "gemini-flash-lite"),
            ]),
        )
        .unwrap();
        assert_eq!(c.llm.kind, ProviderKind::Gemini);
        assert_eq!(c.llm.model, "gemini-flash-lite");
        assert_eq!(c.limits.llm, 2, "cloud providers run a few at once");
        assert_eq!(c.cleanup_settings().chunk_tokens, 8_000);
        assert_eq!(
            c.llm_base_url().unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );

        let c = Config::from_env(
            Path::new("/repo"),
            env(&[
                ("SHRUTI_LLM_PROVIDER", "custom"),
                ("SHRUTI_LLM_CONCURRENCY", "4"),
                ("SHRUTI_CHUNK_TOKENS", "3000"),
            ]),
        )
        .unwrap();
        assert!(
            c.llm_base_url()
                .unwrap_err()
                .to_string()
                .contains("SHRUTI_LLM_BASE_URL")
        );
        assert_eq!(c.limits.llm, 4);
        assert_eq!(c.cleanup_settings().chunk_tokens, 3_000);

        let err = Config::from_env(
            Path::new("/repo"),
            env(&[("SHRUTI_LLM_PROVIDER", "claude")]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("SHRUTI_LLM_PROVIDER"), "{err}");
    }

    #[test]
    fn env_overrides() {
        let c = Config::from_env(
            Path::new("/repo"),
            env(&[
                ("SHRUTI_YTDLP", "/opt/yt-dlp"),
                ("SHRUTI_JS_RUNTIME", ""),
                ("SHRUTI_COOKIES_BROWSER", "brave"),
                ("SHRUTI_WHISPER_BIN", "/opt/whisper-cli"),
                ("SHRUTI_WHISPER_MODEL", "/m/small.bin"),
                ("SHRUTI_VAD_MODEL", ""),
                ("SHRUTI_GPU", "1"),
                ("SHRUTI_THREADS", "8"),
                ("SHRUTI_DATABASE_URL", "postgres://x@db/other"),
                ("SHRUTI_WORK_DIR", "/fast/work"),
                ("SHRUTI_DOWNLOADS", "2"),
                ("SHRUTI_IG_DELAY_SECS", "5"),
                ("SHRUTI_IG_BATCH", "10"),
                ("SHRUTI_IG_PAUSE_SECS", "120"),
            ]),
        )
        .unwrap();
        assert_eq!(c.ytdlp_bin, PathBuf::from("/opt/yt-dlp"));
        assert_eq!(c.js_runtime, None);
        assert_eq!(c.cookies_from_browser.as_deref(), Some("brave"));
        assert_eq!(c.whisper_bin, PathBuf::from("/opt/whisper-cli"));
        assert_eq!(c.whisper_model, PathBuf::from("/m/small.bin"));
        assert_eq!(c.vad_model, None);
        assert_eq!(c.gpu, GpuChoice::Device(1));
        assert_eq!(c.threads, Some(8));
        assert_eq!(c.database_url, "postgres://x@db/other");
        assert_eq!(c.work_dir, PathBuf::from("/fast/work"));
        assert_eq!(c.limits.downloads, 2);
        let ig = c.limits.instagram;
        assert_eq!(
            (ig.gap.as_secs(), ig.batch, ig.pause.as_secs()),
            (5, 10, 120)
        );
    }

    #[test]
    fn no_env_is_repo_defaults() {
        assert_eq!(
            Config::from_env(
                Path::new("/repo"),
                env(&[("HOME", "/home/me"), ("USER", "me")])
            )
            .unwrap(),
            defaults()
        );
        let c = Config::from_env(Path::new("/repo"), env(&[("SHRUTI_GPU", "auto")])).unwrap();
        assert_eq!(c.gpu, GpuChoice::Auto);
    }

    #[test]
    fn bad_numbers_name_the_variable() {
        for (k, v) in [
            ("SHRUTI_GPU", "nvidia"),
            ("SHRUTI_THREADS", "-2"),
            ("SHRUTI_DOWNLOADS", "many"),
        ] {
            let err = Config::from_env(Path::new("/repo"), env(&[(k, v)])).unwrap_err();
            assert!(err.to_string().contains(k), "{err}");
        }
    }

    #[test]
    fn problems_lists_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config {
            ytdlp_bin: "sh".into(),
            ..Config::defaults(dir.path(), dir.path(), "me")
        };
        let problems = c.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("whisper-cli")));
        assert!(problems.iter().any(|p| p.contains("ggml-large-v3-turbo")));
        assert!(problems.iter().any(|p| p.contains("silero")));

        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        std::fs::write(dir.path().join("models/ggml-large-v3-turbo-q5_0.bin"), "").unwrap();
        let c = Config {
            vad_model: None,
            ytdlp_bin: "sh".into(),
            ..Config::defaults(dir.path(), dir.path(), "me")
        };
        assert_eq!(c.problems().len(), 1);
    }

    #[test]
    fn programs_are_found_on_path_or_reported() {
        let dir = tempfile::tempdir().unwrap();
        let base = Config::defaults(dir.path(), dir.path(), "me");
        let on_path = Config {
            whisper_bin: "sh".into(),
            ytdlp_bin: "sh".into(),
            ..base.clone()
        };
        assert!(
            !on_path
                .problems()
                .iter()
                .any(|p| p.contains("whisper-cli") || p.contains("yt-dlp"))
        );
        let missing = Config {
            ytdlp_bin: "no-such-program-shruti".into(),
            ..base
        };
        assert!(
            missing
                .problems()
                .iter()
                .any(|p| p == "yt-dlp not found at no-such-program-shruti")
        );
    }
}
