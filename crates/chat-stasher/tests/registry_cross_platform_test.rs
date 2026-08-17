//! Cross-platform registry self-consistency.
//!
//! Every harness in `data/harness-registry-v1.json` carries one cell per OS
//! (macos / linux / windows). Those cells must describe the *same* directory
//! structure for the harness — a cell that silently dropped a directory layer
//! (e.g. Gemini's macOS cell used to be `~/.gemini/tmp/` while linux/windows
//! were `$HOME/.gemini/tmp/<projectId>/chats/`) is a real defect: it makes the
//! scanner walk a shallower tree and mislabel whatever it happens to find.
//!
//! The check is **structural**, not literal: home-dir / env-base prefixes
//! (`~/`, `$HOME/`, `%USERPROFILE%\`, `$CODEX_HOME`, …) and per-session
//! placeholders (`<projectId>`, `<sanitized-cwd>`, `<uuid>`, …) are normalised
//! away, then the remaining segment sequence is compared across the OS cells.
//! A genuine platform difference in the *base* directory (macOS
//! `~/Library/Application Support/…` vs Linux `~/.config/…`) therefore does
//! not itself fail the check — only a difference in the harness-relative
//! structure does.
//!
//! Harnesses whose OS cells *legitimately* differ are listed in
//! [`whitelisted_structure_reasons`] with the reason each is a real
//! difference (never a defect-masking entry).

use chat_stasher::scanner::{self, RegistryCell};
use std::collections::{BTreeMap, HashMap};

/// The platform keys the registry uses. A harness is compared across every
/// cell it actually declares.
const PLATFORMS: [&str; 3] = ["macos", "linux", "windows"];

/// Environment / home markers that stand for the machine-dependent root.
/// Stripping them leaves only the harness-relative structure to compare.
const BASE_MARKERS: [&str; 11] = [
    "~",
    "$HOME",
    "$CWD",
    "$CODEX_HOME",
    "%CODEX_HOME%",
    "%USERPROFILE%",
    "%APPDATA%",
    "%LOCALAPPDATA%",
    "$XDG_DATA_HOME",
    "$XDG_CONFIG_HOME",
    "$XDG_STATE_HOME",
];

/// Normalise one cell template into its structural path-segment sequence.
///
/// 1. Drop any trailing Chinese annotation (`（默认 …）`, `（项目级）…`) and any
///    alternate / sidecar file (` + sessions.json`): the structure check only
///    cares about the primary path.
/// 2. Strip home / env base markers (the machine-dependent root).
/// 3. Replace each per-session / per-project `<placeholder>` with a single
///    structural slot `*` (we care that *a* segment exists there, not what it
///    is named — `<uuid>` vs `<session-uuid>` are the same shape).
/// 4. Split on either path separator and drop empty / lone-`.` (CWD) segments.
fn normalized_segments(template: &str) -> Vec<String> {
    let t: &str = template
        .split('（')
        .next()
        .unwrap_or("")
        .split(" + ")
        .next()
        .unwrap_or("");

    let mut segments: Vec<String> = Vec::new();
    let mut seg = String::new();
    let mut rest = t;
    while !rest.is_empty() {
        // Per-session placeholder → one structural slot.
        if rest.starts_with('<') {
            if let Some(end) = rest.find('>') {
                seg.push('*');
                rest = &rest[end + 1..];
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        if ch == '/' || ch == '\\' {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
            rest = &rest[1..];
            continue;
        }
        let mut stripped = false;
        for m in BASE_MARKERS {
            if let Some(after) = rest.strip_prefix(m) {
                rest = after;
                stripped = true;
                break;
            }
        }
        if stripped {
            continue;
        }
        seg.push(ch);
        rest = &rest[1..];
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    // A lone `.` segment (Windows `.\opencode\…` = CWD-relative) is not a
    // structural directory.
    segments.retain(|s| s != ".");
    segments
}

/// Harnesses whose OS cells legitimately differ in structure — real platform
/// conventions / documented discrepancies, each with the reason it is genuine
/// rather than a defect. A harness in this list is allowed to be inconsistent;
/// everything else must match.
fn whitelisted_structure_reasons() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "codex",
            "macOS cell writes the expanded default `~/.codex/sessions/` while linux/windows write the `$CODEX_HOME`/`%CODEX_HOME%` override variable — whose default *is* `~/.codex`. Both resolve to `<codex-home>/sessions/`; only the notation differs (the `.codex` segment is inlined on macOS but baked into the variable elsewhere). Not a missing layer.",
        ),
        (
            "cursor",
            "macOS stores app data under `~/Library/Application Support/Cursor/User/…` (extra `Library/Application Support` base segments), Linux under `$XDG_CONFIG_HOME/Cursor/User/…` (~/.config), Windows under `%APPDATA%\\Cursor\\User\\…`. A genuine per-OS app-data base-directory convention (the task's own example), not a missing layer.",
        ),
        (
            "zed",
            "macOS `~/Library/Application Support/Zed/threads/…` (extra `Library/Application Support` + capitalized `Zed`) vs Linux `$XDG_DATA_HOME/zed/threads/…` (~/.local/share/zed, lowercase) vs Windows `%LOCALAPPDATA%\\Zed\\threads\\…`. Genuine per-OS app-data base convention including dir casing.",
        ),
    ]
}

/// Render the platform→structure groups for a failure/whitelist message.
fn describe(by_struct: &BTreeMap<Vec<String>, Vec<&str>>) -> String {
    let mut parts = Vec::new();
    for (segs, platforms) in by_struct {
        parts.push(format!(
            "{} → [{}]",
            platforms.join("/"),
            segs.iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    parts.join(" ；")
}

/// The invariant: for every harness, the structural segment sequences of its
/// declared OS cells must all agree (or the harness must be whitelisted with a
/// documented genuine reason).
#[test]
fn every_harness_os_cells_structurally_consistent() {
    let registry = scanner::load_registry_from_repo().expect("registry must load");
    let whitelist: HashMap<&str, &str> = whitelisted_structure_reasons().iter().copied().collect();

    let mut checked = 0usize;
    let mut whitelisted = 0usize;
    for h in &registry.harnesses {
        let cells: Vec<(&str, &RegistryCell)> = PLATFORMS
            .iter()
            .filter_map(|p| h.paths.cell_for(p).map(|c| (*p, c)))
            .collect();
        if cells.len() < 2 {
            continue; // single OS cell — nothing to compare
        }
        checked += 1;

        let mut by_struct: BTreeMap<Vec<String>, Vec<&str>> = BTreeMap::new();
        for (p, c) in &cells {
            by_struct
                .entry(normalized_segments(&c.template))
                .or_default()
                .push(p);
        }

        if by_struct.len() == 1 {
            continue; // all declared OS cells are structurally identical
        }
        if let Some(reason) = whitelist.get(h.id.as_str()) {
            // A version/version difference is configuration drift, not a
            // platform-specific directory convention; it must never excuse
            // structurally inconsistent registry cells.
            assert!(
                !reason.contains("版本") && !reason.to_ascii_lowercase().contains("version"),
                "whitelist reason for {} mentions version/version; fix the template instead",
                h.id
            );
            whitelisted += 1;
            println!(
                "[whitelisted] {id}: {reason} — structs: {structs}",
                id = h.id,
                structs = describe(&by_struct)
            );
            continue;
        }
        panic!(
            "harness {} 各 OS 路径结构不一致：{}",
            h.id,
            describe(&by_struct)
        );
    }
    // Sanity: we must actually have exercised the loop, or the test is a no-op.
    assert!(
        checked >= 10,
        "expected to check >=10 harnesses, checked {checked}"
    );
    println!(
        "checked {checked} harnesses (whitelisted {whitelisted} for genuine platform differences)"
    );
}
