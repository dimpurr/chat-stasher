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
    GoogleAntigravity,
    OpenCode,
    OpenClaw,
    HermesAgent,
    Cursor,
    Grok,
    GrokBot,
    CopilotCli,
    Aider,
    Crush,
    Zed,
    Continue,
    KimiCode,
    DeepSeekHarness,
}

/// Layout of a virtual session backed by a structured, read-only local source.
///
/// File-backed sources leave this as `None`; the scanner sets it only when a
/// `SessionRecord` represents a logical row, composer, or app replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteSessionLayout {
    OpenCode,
    OpenClaw,
    HermesAgent,
    CursorGlobal,
    CursorLegacy,
    Grok,
    Zed,
    GrokBot,
}

impl HarnessSource {
    /// Short label used as the `<source>` component of a session id.
    /// Values match the `id` field of `data/harness-registry-v1.json`.
    pub fn short(&self) -> &'static str {
        match self {
            HarnessSource::ClaudeCode => "claude-code",
            HarnessSource::Codex => "codex",
            HarnessSource::GeminiCli => "gemini-cli",
            HarnessSource::GoogleAntigravity => "google-antigravity",
            HarnessSource::OpenCode => "opencode",
            HarnessSource::OpenClaw => "openclaw",
            HarnessSource::HermesAgent => "hermes-agent",
            HarnessSource::Cursor => "cursor",
            HarnessSource::Grok => "grok",
            HarnessSource::GrokBot => "grok-bot",
            HarnessSource::CopilotCli => "github-copilot-cli",
            HarnessSource::Aider => "aider",
            HarnessSource::Crush => "crush",
            HarnessSource::Zed => "zed",
            HarnessSource::Continue => "continue",
            HarnessSource::KimiCode => "kimi-code",
            // Not abbreviable to `deepseek`: that id is the deepseek.com web
            // chat platform, a different harness from this local agent.
            HarnessSource::DeepSeekHarness => "deepseek-harness",
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
            "google-antigravity" => Some(HarnessSource::GoogleAntigravity),
            "opencode" => Some(HarnessSource::OpenCode),
            "openclaw" => Some(HarnessSource::OpenClaw),
            "hermes-agent" => Some(HarnessSource::HermesAgent),
            "cursor" => Some(HarnessSource::Cursor),
            "grok" => Some(HarnessSource::Grok),
            "grok-bot" => Some(HarnessSource::GrokBot),
            "github-copilot-cli" => Some(HarnessSource::CopilotCli),
            "aider" => Some(HarnessSource::Aider),
            "crush" => Some(HarnessSource::Crush),
            "zed" => Some(HarnessSource::Zed),
            "continue" => Some(HarnessSource::Continue),
            "kimi-code" => Some(HarnessSource::KimiCode),
            "deepseek-harness" => Some(HarnessSource::DeepSeekHarness),
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
    /// True when the file is zst-compressed, whichever spelling the registry
    /// declares: `*.jsonl.zst` (codex) or `*.jsonl.zstd` (DeepSeek Harness).
    /// Read from the declared format rather than sniffed, so a source that
    /// declares compression and is read as text cannot happen silently.
    pub compressed: bool,
    /// Present for a virtual SQLite session; absent for ordinary files.
    pub sqlite_layout: Option<SqliteSessionLayout>,
    /// Capture-time dimensions declared by the registry path that found this
    /// source. This never changes the session identity or transcript bytes.
    pub provenance: crate::provenance::SessionProvenance,
}
