//! Shared data types for the scanner output.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// Which harness produced a session file. Source identity is derived from the
/// containing directory (`.claude` vs `.codex`), or — for the registry-driven
/// scan — from the harness entry whose root is being walked, never guessed
/// from session content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSource {
    ClaudeCode,
    Codex,
    GeminiCli,
    OpenCode,
    Cursor,
    Grok,
    CopilotCli,
    Aider,
    Crush,
    Zed,
    Continue,
}

impl HarnessSource {
    /// Short label used as the `<source>` component of a session id.
    /// Values match the `id` field of `data/harness-registry-v1.json`.
    pub fn short(&self) -> &'static str {
        match self {
            HarnessSource::ClaudeCode => "claude-code",
            HarnessSource::Codex => "codex",
            HarnessSource::GeminiCli => "gemini-cli",
            HarnessSource::OpenCode => "opencode",
            HarnessSource::Cursor => "cursor",
            HarnessSource::Grok => "grok",
            HarnessSource::CopilotCli => "github-copilot-cli",
            HarnessSource::Aider => "aider",
            HarnessSource::Crush => "crush",
            HarnessSource::Zed => "zed",
            HarnessSource::Continue => "continue",
        }
    }

    /// Map a registry harness `id` (see `data/harness-registry-v1.json`) onto a
    /// source variant. `None` for ids this build does not know — such a harness
    /// is skipped, never scanned under a wrong label.
    pub fn from_id(id: &str) -> Option<HarnessSource> {
        match id {
            "claude-code" => Some(HarnessSource::ClaudeCode),
            "codex" => Some(HarnessSource::Codex),
            "gemini-cli" => Some(HarnessSource::GeminiCli),
            "opencode" => Some(HarnessSource::OpenCode),
            "cursor" => Some(HarnessSource::Cursor),
            "grok" => Some(HarnessSource::Grok),
            "github-copilot-cli" => Some(HarnessSource::CopilotCli),
            "aider" => Some(HarnessSource::Aider),
            "crush" => Some(HarnessSource::Crush),
            "zed" => Some(HarnessSource::Zed),
            "continue" => Some(HarnessSource::Continue),
            _ => None,
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
