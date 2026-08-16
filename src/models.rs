//! Shared data types for the scanner output.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// Which harness produced a session file. Source identity is derived from the
/// containing directory (`.claude` vs `.codex`), never guessed from content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSource {
    ClaudeCode,
    Codex,
}

impl HarnessSource {
    /// Short label used as the `<source>` component of a session id.
    pub fn short(&self) -> &'static str {
        match self {
            HarnessSource::ClaudeCode => "claude-code",
            HarnessSource::Codex => "codex",
        }
    }
}

impl From<HarnessSource> for String {
    fn from(s: HarnessSource) -> String {
        s.short().to_string()
    }
}

impl fmt::Display for HarnessSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short())
    }
}

/// One discovered session file. Deliberately metadata-only: we never carry
/// (or read) the session's body — privacy line.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// Canonical id: `<source>.<machine>.<native-id>`.
    pub id: String,
    /// Absolute path to the session file.
    pub absolute_path: PathBuf,
    /// File size in bytes.
    pub byte_size: u64,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Which harness produced it.
    pub source: HarnessSource,
    /// True when the file is zst-compressed (`*.jsonl.zst`).
    pub compressed: bool,
}