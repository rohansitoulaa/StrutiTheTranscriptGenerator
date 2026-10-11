use std::io;
use std::path::PathBuf;

/// Errors cross into the UI later, so every variant renders as one readable
/// line that keeps the underlying reason (PDA Q-3: "failed with a readable
/// reason").
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a supported link: {0}")]
    UnsupportedUrl(String),

    #[error("{0} links are not supported yet")]
    NotYetSupported(&'static str),

    #[error("could not start {tool}: {source}")]
    Spawn {
        tool: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("{tool} failed ({status}): {stderr}")]
    ToolFailed {
        tool: &'static str,
        status: String,
        stderr: String,
    },

    #[error("{tool} timed out after {secs}s")]
    Timeout { tool: &'static str, secs: u64 },

    #[error("could not {action} {}: {source}", path.display())]
    Fs {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not read {what}: {reason}")]
    Parse { what: &'static str, reason: String },

    /// 429s or a login wall from Instagram (PDA IG-5): its jobs pause
    /// rather than fail, and YouTube carries on.
    #[error("Instagram is limiting requests, so Instagram jobs are paused: {0}")]
    InstagramBlocked(String),

    #[error("could not download {url}: {reason}")]
    Download { url: String, reason: String },

    #[error("no transcript produced: {0}")]
    Empty(String),

    /// Stored state that should exist doesn't, or kept changing.
    #[error("{0}")]
    State(String),

    #[error("another Shruti is already processing the queue")]
    QueueBusy,

    #[error("the queue was stopped")]
    Stopped,

    #[error("lost the queue lock (database connection dropped): {0}")]
    LockLost(String),

    #[error(transparent)]
    Llm(#[from] crate::llm::LlmError),

    #[error("keyring: {0}")]
    Keyring(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("database migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Attaches the path and the operation to an I/O error, so a log line says
/// which file failed instead of a bare "No such file or directory".
pub(crate) fn fs_err(
    action: &'static str,
    path: impl Into<PathBuf>,
) -> impl FnOnce(io::Error) -> Error {
    let path = path.into();
    move |source| Error::Fs {
        action,
        path,
        source,
    }
}

/// Keep the end of a tool's stderr: that is where yt-dlp and whisper print the
/// actual error, and the start is usually progress noise.
pub(crate) fn stderr_tail(stderr: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    let tail: String = trimmed.chars().skip(count - max_chars).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_keeps_the_end() {
        assert_eq!(stderr_tail(b"  short  ", 100), "short");
        assert_eq!(stderr_tail(b"abcdefghij", 4), "…ghij");
    }

    #[test]
    fn stderr_tail_survives_invalid_utf8() {
        assert_eq!(stderr_tail(&[b'o', b'k', 0xE0, 0xA4], 100), "ok\u{FFFD}");
    }

    #[test]
    fn fs_errors_name_the_path_and_action() {
        let err =
            fs_err("write", "/vault/Shruti/a.md")(io::Error::from(io::ErrorKind::PermissionDenied));
        assert_eq!(
            err.to_string(),
            "could not write /vault/Shruti/a.md: permission denied"
        );
    }
}
