//! Running the external tools.

use std::ffi::OsString;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use crate::error::stderr_tail;
use crate::{Error, Result};

/// Metadata and caption fetches: a few seconds normally, so this only trips
/// on a stalled connection.
pub(crate) const QUICK: Duration = Duration::from_secs(120);
/// Audio downloads and Whisper runs on long videos.
pub(crate) const LONG: Duration = Duration::from_secs(30 * 60);

/// Runs `bin args…` to completion, or kills it after `limit`. A non-zero
/// exit becomes [`Error::ToolFailed`] carrying the end of stderr. The child
/// is killed whenever the future is dropped (timeout or a cancelled job), so
/// nothing is left running behind.
pub(crate) async fn run_tool(
    tool: &'static str,
    bin: &Path,
    args: &[OsString],
    limit: Duration,
) -> Result<Output> {
    tracing::debug!(tool, bin = %bin.display(), ?args, "running");
    let mut attempt = 0;
    let output = loop {
        let child = tokio::process::Command::new(bin)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let result = tokio::time::timeout(limit, child)
            .await
            .map_err(|_| Error::Timeout {
                tool,
                secs: limit.as_secs(),
            })?;
        match result {
            // ETXTBSY: the binary was just written (a yt-dlp update) and a
            // forked child elsewhere still briefly holds it open.
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 5 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => break other.map_err(|source| Error::Spawn { tool, source })?,
        }
    };

    if !output.status.success() {
        return Err(Error::ToolFailed {
            tool,
            status: output.status.to_string(),
            stderr: stderr_tail(&output.stderr, STDERR_TAIL_CHARS),
        });
    }
    Ok(output)
}

const STDERR_TAIL_CHARS: usize = 1_500;

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<OsString> {
        vec!["-c".into(), script.into()]
    }

    #[tokio::test]
    async fn success_returns_stdout() {
        let out = run_tool("sh", Path::new("sh"), &sh("printf hi"), QUICK)
            .await
            .unwrap();
        assert_eq!(out.stdout, b"hi");
    }

    #[tokio::test]
    async fn a_hung_tool_times_out_and_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-running");
        let script = format!("sleep 1; touch {}", marker.display());
        let started = std::time::Instant::now();
        let err = run_tool(
            "yt-dlp",
            Path::new("sh"),
            &sh(&script),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::Timeout { tool: "yt-dlp", .. }),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_millis(900));
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert!(!marker.exists(), "child kept running after the timeout");
    }

    #[tokio::test]
    async fn failure_carries_exit_status_and_stderr_tail() {
        let err = run_tool("sh", Path::new("sh"), &sh("echo boom >&2; exit 3"), QUICK)
            .await
            .unwrap_err();
        match err {
            Error::ToolFailed {
                tool,
                status,
                stderr,
            } => {
                assert_eq!(tool, "sh");
                assert!(status.contains('3'), "{status}");
                assert_eq!(stderr, "boom");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_error_naming_the_tool() {
        let err = run_tool("yt-dlp", Path::new("/nonexistent/yt-dlp"), &[], QUICK)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Spawn { tool: "yt-dlp", .. }));
        assert!(err.to_string().contains("yt-dlp"));
    }
}
