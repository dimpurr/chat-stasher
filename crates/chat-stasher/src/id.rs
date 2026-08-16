//! Session identifier generation.
//!
//! The stable part of a session's identity is
//!
//! ```text
//! <source>.<machine>.<native-id>
//! ```
//!
//! e.g. `codex.dims-macbook-pro-max-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765`.
//! The component order and the `.` separator are contractual: `.` is legal in
//! filenames on all three platforms we target (`:`, which is what
//! macOS/Windows reject, is never used).
//!
//! Everything here is deliberately pure and testable — no filesystem side
//! effects in this module.

/// Short label used as the `<source>` component for each harness.
pub const SOURCE_CODE_DIR: &str = "claude-code";
pub const SOURCE_CODEX: &str = "codex";

/// Normalise a raw hostname into the `<machine>` component.
///
/// Rules (from the spike, do not invent your own):
///   1. lowercase,
///   2. replace any character outside `[a-z0-9-]` with `-`,
///   3. collapse runs of consecutive `-` into a single `-`,
///   4. truncate to 40 chars.
///
/// Note we only collapse, we do *not* trim leading/trailing dashes — the
/// spike does not ask for that, and trimming could collide two otherwise
/// distinct machines.
pub fn normalize_machine(raw: &str) -> String {
    let dashed: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut out = String::with_capacity(dashed.len());
    let mut prev_was_dash = false;
    for c in dashed.chars() {
        if c == '-' {
            if !prev_was_dash {
                out.push('-');
                prev_was_dash = true;
            }
        } else {
            out.push(c);
            prev_was_dash = false;
        }
    }

    out.chars().take(40).collect()
}

/// Fetch this machine's short hostname and normalise it.
///
/// Shelling out to `hostname -s` (not `scutil --get ComputerName`, which on
/// macOS returns a display name with spaces / typographic quotes that would
/// need a wholly different kind of cleaning). Falls back to `$HOSTNAME`, then
/// to `localhost`, rather than failing the whole scan.
pub fn machine_id() -> String {
    let raw = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string());
    normalize_machine(&raw)
}

/// True when `s` looks like a UUID (8-4-4-4-12 groups of lowercase hex).
///
/// Lenient by design: we check *shape* only, not canonical uppercase vs
/// lowercase (Codex emits lowercase, but a future harness may not). A file
/// whose stem is not UUID-shaped still gets indexed — the native id is kept
/// verbatim — this check only decides what counts as a "standard" id.
pub fn is_uuid_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let groups = bytes.split(|&b| b == b'-');
    let mut widths = Vec::new();
    for g in groups {
        let hex_ok = !g.is_empty()
            && g.iter()
                .all(|b| b.is_ascii_hexdigit() || b.is_ascii_lowercase());
        if !hex_ok {
            return false;
        }
        widths.push(g.len());
    }
    widths == vec![8, 4, 4, 4, 12]
}

/// Pieces of identity needed to build an id from a file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    /// Whichever harness produced the session.
    pub source_short: &'static str,
    /// Normalised machine component.
    pub machine: String,
    /// The native id found in the filename (a UUID when possible).
    pub native_id: String,
}

impl SessionIdentity {
    /// Compose the canonical `<source>.<machine>.<native-id>` string.
    pub fn id(&self) -> String {
        format!("{}.{}.{}", self.source_short, self.machine, self.native_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_lowercases_and_sanitizes() {
        assert_eq!(normalize_machine("My-MacBook Pro"), "my-macbook-pro");
        assert_eq!(normalize_machine("DIMS—MBP·2"), "dims-mbp-2");
    }

    #[test]
    fn machine_collapses_dashes_and_truncates() {
        assert_eq!(normalize_machine("a---b--c"), "a-b-c");
        let long = "x".repeat(60);
        assert_eq!(normalize_machine(&long).len(), 40);
    }

    #[test]
    fn uuid_shape_checked() {
        assert!(is_uuid_like("019bf00d-97b6-7eb2-9bf8-eacbacc09765"));
        assert!(is_uuid_like("019BF00D-97B6-7EB2-9BF8-EACBACC09765"));
        assert!(!is_uuid_like("not-a-uuid"));
        assert!(!is_uuid_like("019bf00d97b67eb29bf8eacbacc09765"));
    }

    #[test]
    fn id_uses_dot_separator_never_colon() {
        let ident = SessionIdentity {
            source_short: SOURCE_CODEX,
            machine: "dims-macbook-pro-max-2".into(),
            native_id: "019bf00d-97b6-7eb2-9bf8-eacbacc09765".into(),
        };
        assert_eq!(
            ident.id(),
            "codex.dims-macbook-pro-max-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765"
        );
        assert!(!ident.id().contains(':'));
    }
}
