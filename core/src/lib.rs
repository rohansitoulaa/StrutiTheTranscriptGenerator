//! Shruti core: link → transcript, with no UI.
//!
//! The pipeline is `Source::parse` → yt-dlp metadata → captions when the video
//! has them in its spoken language, otherwise audio → whisper.cpp → a Markdown
//! note. The Tauri app (later build steps) drives this crate; nothing here
//! knows about windows or IPC.

pub mod captions;
pub mod cleanup;
pub mod config;
pub mod db;
pub mod error;
pub mod folders;
pub mod llm;
pub mod metadata;
pub mod output;
pub mod pipeline;
mod process;
pub mod queue;
pub mod secrets;
pub mod source;
pub mod transcript;
pub mod whisper;
pub mod ytdlp;

pub use error::{Error, Result};
