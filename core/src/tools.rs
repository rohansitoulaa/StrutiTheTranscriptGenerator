//! What Shruti keeps up to date itself: its own copy of yt-dlp (PDA YT-2,
//! YT-3) and the Whisper models (UI-7).
//!
//! Downloads go to `<name>.part` and are renamed into place only once
//! complete (and, for yt-dlp, checksum-verified and runnable), so an
//! interrupted download never replaces a working file.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::error::fs_err;
use crate::process::{QUICK, run_tool};
use crate::{Error, Result};

pub const GITHUB_API: &str = "https://api.github.com";
/// The standalone Linux build: no Python needed.
const YTDLP_ASSET: &str = "yt-dlp_linux";
const YTDLP_SUMS: &str = "SHA2-256SUMS";

pub const WHISPER_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
pub const VAD_MODEL_URL: &str =
    "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin";
/// The models as downloaded and tested in build step 1. The URLs follow a
/// moving branch, so a changed or tampered file fails here instead of
/// reaching Whisper.
pub const WHISPER_MODEL_SHA256: &str =
    "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2";
pub const VAD_MODEL_SHA256: &str =
    "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf";

/// A model to fetch if it's missing.
pub struct ModelFile<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
    pub path: &'a Path,
}

/// Shruti's own yt-dlp, preferred over the bundled one once it exists.
pub fn managed_ytdlp(data_dir: &Path) -> PathBuf {
    data_dir.join("bin/yt-dlp")
}

/// yt-dlp versions are release dates: `2026.08.19`, or `2026.08.19.1`.
pub fn version_date(version: &str) -> Option<NaiveDate> {
    let mut parts = version.trim().split('.');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

pub async fn ytdlp_version(bin: &Path) -> Result<String> {
    let out = run_tool("yt-dlp", bin, &["--version".into()], QUICK).await?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if version.is_empty() {
        return Err(Error::Empty("yt-dlp --version printed nothing".into()));
    }
    Ok(version)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub binary_url: String,
    pub sums_url: String,
}

/// The latest yt-dlp release on GitHub.
pub async fn latest_ytdlp(http: &reqwest::Client, api: &str) -> Result<Release> {
    #[derive(Deserialize)]
    struct Raw {
        tag_name: String,
        assets: Vec<Asset>,
    }
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    let url = format!("{api}/repos/yt-dlp/yt-dlp/releases/latest");
    let raw: Raw = get(http, &url)
        .await?
        .json()
        .await
        .map_err(|e| download_err(&url, e))?;
    let asset = |name: &str| {
        raw.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| Error::Parse {
                what: "yt-dlp release",
                reason: format!("{} has no {name}", raw.tag_name),
            })
    };
    Ok(Release {
        binary_url: asset(YTDLP_ASSET)?,
        sums_url: asset(YTDLP_SUMS)?,
        tag: raw.tag_name.clone(),
    })
}

/// Downloads `release` into `data_dir`, checks it against the release's
/// SHA-256 list and that it runs, then puts it in place. Returns the
/// installed version.
pub async fn install_ytdlp(
    http: &reqwest::Client,
    release: &Release,
    data_dir: &Path,
) -> Result<String> {
    let sums = get(http, &release.sums_url)
        .await?
        .text()
        .await
        .map_err(|e| download_err(&release.sums_url, e))?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim().trim_start_matches('*') == YTDLP_ASSET).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| Error::Parse {
            what: "yt-dlp checksums",
            reason: format!("no entry for {YTDLP_ASSET}"),
        })?;

    let dest = managed_ytdlp(data_dir);
    let part = download(http, &release.binary_url, &dest, Some(&expected), |_, _| {}).await?;
    make_executable(&part).await?;
    match ytdlp_version(&part).await {
        Ok(version) => {
            tokio::fs::rename(&part, &dest)
                .await
                .map_err(fs_err("replace", &dest))?;
            Ok(version)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(e)
        }
    }
}

/// Fetches whichever of the Whisper and VAD models are missing (UI-7),
/// checking each against its SHA-256. `progress(file, done, total)` reports
/// bytes as they arrive.
pub async fn ensure_models(
    http: &reqwest::Client,
    models: &[ModelFile<'_>],
    progress: impl Fn(&Path, u64, Option<u64>),
) -> Result<()> {
    for ModelFile { url, sha256, path } in models {
        if path.is_file() {
            continue;
        }
        let part = download(http, url, path, Some(sha256), |done, total| {
            progress(path, done, total)
        })
        .await?;
        tokio::fs::rename(&part, path)
            .await
            .map_err(fs_err("replace", path))?;
    }
    Ok(())
}

/// Streams `url` to `<dest>.part`, checking its SHA-256 when given, and
/// returns the `.part` path. The caller moves it into place.
async fn download(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: Option<&str>,
    progress: impl Fn(u64, Option<u64>),
) -> Result<PathBuf> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(fs_err("create", dir))?;
    let part = dest.with_extension(match dest.extension() {
        Some(ext) => format!("{}.part", ext.to_string_lossy()),
        None => "part".into(),
    });
    let result = async {
        let mut response = get(http, url).await?;
        let total = response.content_length();
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(fs_err("create", &part))?;
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| download_err(url, e))? {
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(fs_err("write", &part))?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        file.flush().await.map_err(fs_err("write", &part))?;
        if let Some(expected) = sha256 {
            let actual = hex(&hasher.finalize());
            if actual != expected {
                return Err(Error::Download {
                    url: url.to_string(),
                    reason: format!("checksum mismatch: expected {expected}, got {actual}"),
                });
            }
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(part),
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(e)
        }
    }
}

async fn get(http: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    http.get(url)
        // GitHub's API refuses requests without one.
        .header(reqwest::header::USER_AGENT, "shruti")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| download_err(url, e))
}

fn download_err(url: &str, e: reqwest::Error) -> Error {
    Error::Download {
        url: url.to_string(),
        reason: e.to_string(),
    }
}

async fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .await
        .map_err(fs_err("make executable", path))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FAKE_YTDLP: &str = "#!/bin/sh\necho 2026.10.01\n";

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    async fn release_server(binary: &str, sums: String) -> (MockServer, Release) {
        let server = MockServer::start().await;
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/repos/yt-dlp/yt-dlp/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "2026.10.01",
                "assets": [
                    {"name": "yt-dlp", "browser_download_url": format!("{base}/dl/yt-dlp")},
                    {"name": "yt-dlp_linux", "browser_download_url": format!("{base}/dl/yt-dlp_linux")},
                    {"name": "SHA2-256SUMS", "browser_download_url": format!("{base}/dl/SHA2-256SUMS")},
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/yt-dlp_linux"))
            .respond_with(ResponseTemplate::new(200).set_body_string(binary.to_string()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/SHA2-256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(&server)
            .await;
        let release = latest_ytdlp(&reqwest::Client::new(), &server.uri())
            .await
            .unwrap();
        (server, release)
    }

    #[test]
    fn versions_are_dates() {
        assert_eq!(
            version_date("2026.08.19"),
            NaiveDate::from_ymd_opt(2026, 8, 19)
        );
        assert_eq!(
            version_date("2026.08.19.1\n"),
            NaiveDate::from_ymd_opt(2026, 8, 19)
        );
        assert_eq!(version_date("nightly"), None);
    }

    #[tokio::test]
    async fn the_latest_release_is_verified_and_installed() {
        let sums = format!(
            "{}  yt-dlp\n{}  yt-dlp_linux\n",
            sha(b"other"),
            sha(FAKE_YTDLP.as_bytes())
        );
        let (_server, release) = release_server(FAKE_YTDLP, sums).await;
        assert_eq!(release.tag, "2026.10.01");
        assert!(release.binary_url.ends_with("/dl/yt-dlp_linux"));

        let data = tempfile::tempdir().unwrap();
        let version = install_ytdlp(&reqwest::Client::new(), &release, data.path())
            .await
            .unwrap();
        assert_eq!(version, "2026.10.01");
        let bin = managed_ytdlp(data.path());
        assert_eq!(ytdlp_version(&bin).await.unwrap(), "2026.10.01");
        assert!(!bin.with_extension("part").exists());
    }

    #[tokio::test]
    async fn a_checksum_mismatch_installs_nothing() {
        let sums = format!("{}  yt-dlp_linux\n", sha(b"something else"));
        let (_server, release) = release_server(FAKE_YTDLP, sums).await;
        let data = tempfile::tempdir().unwrap();
        // An existing working copy stays.
        let bin = managed_ytdlp(data.path());
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "old").unwrap();

        let err = install_ytdlp(&reqwest::Client::new(), &release, data.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "old");
        assert!(!bin.with_extension("part").exists());
    }

    #[tokio::test]
    async fn missing_models_are_fetched_with_progress_and_present_ones_skipped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/whisper.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 4096]))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let whisper = dir.path().join("models/ggml-whisper.bin");
        let vad = dir.path().join("models/vad.bin");
        std::fs::create_dir_all(vad.parent().unwrap()).unwrap();
        std::fs::write(&vad, "present").unwrap();

        let seen = Mutex::new(Vec::new());
        let whisper_url = format!("{}/whisper.bin", server.uri());
        let vad_url = format!("{}/never-fetched.bin", server.uri());
        let good = sha(&[7u8; 4096]);
        ensure_models(
            &reqwest::Client::new(),
            &[
                ModelFile {
                    url: &whisper_url,
                    sha256: &good,
                    path: &whisper,
                },
                ModelFile {
                    url: &vad_url,
                    sha256: &good,
                    path: &vad,
                },
            ],
            |_, done, total| seen.lock().unwrap().push((done, total)),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&whisper).unwrap().len(), 4096);
        assert_eq!(seen.lock().unwrap().last(), Some(&(4096, Some(4096))));
    }

    #[tokio::test]
    async fn a_failed_download_leaves_no_partial_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.bin");
        let url = format!("{}/m.bin", server.uri());
        let file = ModelFile {
            url: &url,
            sha256: "00",
            path: &model,
        };
        let err = ensure_models(&reqwest::Client::new(), &[file], |_, _, _| {})
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Download { .. }), "{err:?}");
        assert!(!model.exists() && !dir.path().join("m.bin.part").exists());
    }

    #[tokio::test]
    async fn a_model_with_the_wrong_checksum_is_never_put_in_place() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 64]))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.bin");
        let url = format!("{}/m.bin", server.uri());
        let file = ModelFile {
            url: &url,
            sha256: WHISPER_MODEL_SHA256,
            path: &model,
        };
        let err = ensure_models(&reqwest::Client::new(), &[file], |_, _, _| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert!(!model.exists() && !dir.path().join("m.bin.part").exists());
    }
}
