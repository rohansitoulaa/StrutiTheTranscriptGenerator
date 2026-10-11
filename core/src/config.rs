//! Where the tools and models live. Step 1 reads environment variables with
//! repo-relative defaults; the app's Settings (PDA UI-5) replace this later.

use std::path::{Path, PathBuf};

use crate::pipeline::Engine;
use crate::queue::Limits;
use crate::whisper::WhisperCli;
use crate::ytdlp::YtDlp;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuChoice {
    /// Probe for the first discrete GPU.
    Auto,
    Device(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        }
    }

    /// [`Config::defaults`] (with `HOME` and `USER`) overridden by
    /// `SHRUTI_*` variables. An empty value turns an optional setting off.
    pub fn from_env(repo_root: &Path, var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let home = PathBuf::from(var("HOME").unwrap_or_default());
        let user = var("USER").unwrap_or_default();
        let mut c = Config::defaults(repo_root, &home, &user);
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
        if let Some(v) = var("SHRUTI_THREADS") {
            c.threads = if v.is_empty() {
                None
            } else {
                Some(number("SHRUTI_THREADS", &v)?)
            };
        }
        Ok(c)
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
