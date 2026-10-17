//! Shared data types for the scanner output.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// Which harness produced a session file. Source identity is derived from the
/// containing directory (`.claude` vs `.codex`), or — for the registry-driven
/// scan — from the harness entry whose root is being walked, never guessed
/// from session content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Codebuff / Freebuff (`manicode`): one product, one on-disk layout,
    /// two npm names (ADR-054 D1).
    Codebuff,
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
            HarnessSource::Codebuff => "codebuff",
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
            "codebuff" => Some(HarnessSource::Codebuff),
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
    /// Facts a registry `session_dir` rule (or the multi-root walk above it)
    /// observed about the record's session directory, collected for their
    /// capture-time consumers: the codebuff chat-directory start time and its
    /// cross-check (ADR-054 D4), the whitelist sidecars that seal raw under
    /// the same session identity (ADR-054 D5/D9), and the registry
    /// `source_roots[].id` a skip observation names instead of a path
    /// (ADR-054 D6/D7). Absent for every record a session-directory rule did
    /// not produce; never identity, never content.
    pub session_dir: Option<SessionDirFacts>,
    /// Discovered through a registry source root marked `movable`: a root of
    /// a store that MOVES its files between the roots of one registry entry
    /// (codex's `thread/archive` moves a rollout from `sessions/` into
    /// `archived_sessions/` and `thread/unarchive` moves it back), so the
    /// file's own name is the session's identity and the same native id seen
    /// at another path is the same file, never a second copy of the store's
    /// bytes (W1007, ADR-059 draft D4).
    ///
    /// In the collector this is what lets a movable carrier recover its
    /// position from the stage by content even when the record also declares
    /// captured dimensions: for a store that moves files, byte-equal
    /// discovery is the same file — the "byte equality cannot establish
    /// event identity" ambiguity that makes a fresh provenance-bearing
    /// source re-seal its whole bytes cannot arise.
    pub movable_root: bool,
}

/// Per-session facts the scanner records at discovery for a harness whose
/// registry cell declares a `session_dir` rule that needs them (only
/// codebuff does today; ADR-054). Additive on purpose: fields stay optional
/// or defaulted so adding a fact never rewrites every construction site.
#[derive(Debug, Clone, Default)]
pub struct SessionDirFacts {
    /// The registry `source_roots[].id` whose walk produced the record —
    /// `codebuff` / `codebuff-dev` / `codebuff-staging` — set by the
    /// multi-root probe so control records name a root, never a path
    /// (ADR-054 D6). `None` only on a record no root walk labelled.
    pub root_id: Option<String>,
    /// The chat directory name's own RFC 3339 UTC instant, recorded at
    /// capture and read from that record — never re-parsed from the archive
    /// id (ADR-054 D4). Absent means the name did not match the accepted
    /// shape: unknown, never zero, never a file time.
    pub captured_start: Option<crate::manifest::CapturedStart>,
    /// The session directory's own mtime (and ctime under Unix, whichever is
    /// earlier), for the D4 cross-check that it does not precede
    /// [`Self::captured_start`].
    pub dir_mtime: Option<SystemTime>,
    /// Whitelist sidecar files found beside the transcript (ADR-054 D5/D9),
    /// sealed as raw bytes under the same session identity. Absent-tolerant:
    /// a session without its sidecars is still archived.
    pub extra_files: Vec<ExtraFileSource>,
}

/// One whitelist sidecar file (ADR-054 D5): metadata only at scan time — the
/// collector alone reads its bytes.
#[derive(Debug, Clone)]
pub struct ExtraFileSource {
    /// Literal file name as declared by the registry rule.
    pub name: String,
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Byte size at scan time.
    pub byte_size: u64,
}
