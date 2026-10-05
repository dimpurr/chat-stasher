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

use sha2::{Digest, Sha256};

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
/// There is deliberately no invented fallback: `None` means a caller must
/// either report the unresolved identity or require an explicit `--machine`.
/// (Before 2026-08-18 this returned `"localhost"` instead, which silently
/// filed one machine's sessions into a partition shared with every other
/// machine that also failed to resolve.)
///
/// 🔴 ADR-018: this is **no longer the archive-partition source.** The archive
/// partition comes from the config `machine` field or the identity file (see
/// `main.rs::resolve_machine`); a hostname-derived partition is exactly the
/// bug ADR-018 removed. `machine_id()` survives only as the machine component
/// of *session ids* produced by the status/doctor scanner, which never writes
/// a partition.
///
/// 🔴 The source list is platform-specific, and that is the whole point of
/// `machine_sources()`. `hostname -s` is right on unix — `scutil --get
/// ComputerName` on macOS returns a display name with spaces and typographic
/// quotes that would need a wholly different kind of cleaning. But **Windows'
/// `hostname` takes no arguments at all**, and Windows does not set
/// `$HOSTNAME`; it sets `%COMPUTERNAME%`. Asking Windows for `hostname -s`
/// therefore fails, and the old `localhost` fallback is what kept that
/// failure invisible: every Windows install was writing to `localhost/`.
pub fn machine_id() -> Option<String> {
    pick_machine(machine_sources())
}

/// The ordered candidate list. Split out from [`machine_id`] so the selection
/// rule can be tested on any platform, while the platform difference stays in
/// exactly one place.
fn machine_sources() -> Vec<Option<String>> {
    #[cfg(windows)]
    {
        vec![
            // Windows' own answer, and the only one that needs no subprocess.
            env_value("COMPUTERNAME"),
            // `hostname` exists on Windows but rejects `-s`; call it bare.
            command_value("hostname", &[]),
            env_value("HOSTNAME"),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![command_value("hostname", &["-s"]), env_value("HOSTNAME")]
    }
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn command_value(program: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// First source that yields a *usable* name after normalisation wins.
///
/// 🔴 "Usable" is stricter than "non-empty", and that was found by writing the
/// test rather than by reading the code: `normalize_machine("???")` does not
/// return `""` — it returns `"-"`, because every rejected character collapses
/// into the separator. A partition literally named `-` carries no information
/// about which machine wrote it, so accepting it would recreate the exact bug
/// this module just removed (`localhost`), only with a less obvious spelling.
/// A source that normalises to nothing but separators is therefore skipped.
fn pick_machine(sources: Vec<Option<String>>) -> Option<String> {
    sources
        .into_iter()
        .flatten()
        .map(|raw| normalize_machine(&raw))
        .find(|machine| !machine.trim_matches('-').is_empty())
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

/// Number of leading characters of the id kept verbatim in the short form.
const SHORT_HEAD_CHARS: usize = 8;
/// Number of sha256 hex digits appended as the discriminator.
const SHORT_TAG_CHARS: usize = 6;

/// Privacy-safe short form of a session id, for reports and terminal output.
///
/// The short id has exactly one job: let a human tell two sessions apart in a
/// report **without** printing the full id. Plain `chars().take(8)` did the
/// second half only. Both id shapes we produce are prefixed, so their first
/// eight characters are a *constant*, not a discriminator:
///
/// ```text
/// ext    deepseek.<sessionId>                 -> "deepseek"
/// native codex.<machine>.<uuid>  (see [`SessionIdentity::id`]) -> "codex.di"
/// ```
///
/// So the head is kept — it is the readable part, and for a bare uuid-shaped
/// id it is genuinely the id's own head — and a short sha256 tag of the
/// **whole** id is appended to carry the distinguishing bits:
///
/// ```text
/// deepseek.d41f6a2b9c0e47aaaa1111        ->  deepseek~e4009d
/// 019bf00d-97b6-7eb2-9bf8-eacbacc09765   ->  019bf00d~b0653a
/// ```
///
/// Properties this must keep, all of them load-bearing:
///   * **deterministic** — sha256 of the id and nothing else; no clock, no rng,
///     so the same session prints the same short form on every run and on
///     every machine.
///   * **non-disclosing** — the head is at most 8 characters and the tag is a
///     one-way digest, so the full id is never recoverable from a report.
///   * **short** — 15 characters, fits a table column.
///
/// `~` is the separator because it appears in neither shape (`.` and `-` both
/// occur inside ids), so the tag is always unambiguously the tag.
pub fn short_session_id(id: &str) -> String {
    let head: String = id.chars().take(SHORT_HEAD_CHARS).collect();
    format!("{head}~{}", sha256_hex_prefix(id, SHORT_TAG_CHARS))
}

/// SHA-256 of `value` as lowercase hex, first `hex_digits` digits.
///
/// One helper for both digests this module appends to something, so the
/// "first N hex digits of the digest of the *whole* value" rule is written
/// down once.
fn sha256_hex_prefix(value: &str, hex_digits: usize) -> String {
    let mut out = String::with_capacity(hex_digits);
    for byte in Sha256::digest(value.as_bytes()) {
        if out.len() >= hex_digits {
            break;
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out.truncate(hex_digits);
    out
}

/// Portable byte budget for a single path component (255 on APFS and most
/// POSIX filesystems). NTFS counts UTF-16 units; this UTF-8 byte bound is
/// conservative there.
pub const MAX_PATH_COMPONENT_BYTES: usize = 255;

/// Hex digits of SHA-256 in the bounded form's tag. 32 digits is 128 bits: the
/// tag is the whole reason two values that share a head stay distinct.
const PATH_COMPONENT_TAG_HEX: usize = 32;

/// Preserve values within `budget` verbatim; otherwise use a UTF-8 prefix plus
/// `~` and the first 32 lowercase hex digits of SHA-256 of the full UTF-8 value.
/// The result fits the byte budget and depends only on the input and budget.
pub fn bounded_path_component(value: &str, budget: usize) -> String {
    if value.len() <= budget {
        return value.to_string();
    }
    let tag = sha256_hex_prefix(value, budget.min(PATH_COMPONENT_TAG_HEX));
    // `saturating_sub`, not a subtraction: a budget smaller than the tag must
    // not underflow.
    let head = truncate_to_byte_boundary(value, budget.saturating_sub(1 + tag.len()));
    if head.is_empty() {
        tag
    } else {
        format!("{head}~{tag}")
    }
}

/// Longest prefix of `value` at most `limit` bytes that ends on a character
/// boundary, so a multi-byte character is never cut in half.
fn truncate_to_byte_boundary(value: &str, limit: usize) -> &str {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
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
    /// Values that already fit remain unchanged. Suffix-bearing filenames must
    /// reserve their own suffix budget. Machine directory names remain governed
    /// by their existing configuration/identity rules.
    pub fn id(&self) -> String {
        bounded_path_component(
            &format!("{}.{}.{}", self.source_short, self.machine, self.native_id),
            MAX_PATH_COMPONENT_BYTES,
        )
    }
}

#[cfg(test)]
mod tests {
    // B65 removed the `localhost` fallback, which was correct — and it
    // immediately turned three Windows scanner tests red, because the only
    // source we asked was `hostname -s` and Windows' `hostname` rejects `-s`.
    // These tests pin the selection rule itself, on every platform, so the
    // platform difference stays confined to `machine_sources()`.
    #[test]
    fn first_usable_source_wins_and_unusable_ones_are_skipped() {
        assert_eq!(
            super::pick_machine(vec![Some("Win-Box".into()), Some("other".into())]),
            Some("win-box".to_string())
        );
        // A missing source is skipped, not treated as an empty name.
        assert_eq!(
            super::pick_machine(vec![None, Some("second".into())]),
            Some("second".to_string())
        );
        // Present but normalises to nothing => keep looking. An empty
        // partition name is not a name.
        assert_eq!(
            super::pick_machine(vec![Some("???".into()), Some("real-host".into())]),
            Some("real-host".to_string())
        );
    }

    #[test]
    fn no_source_means_none_never_an_invented_name() {
        assert_eq!(super::pick_machine(vec![]), None);
        assert_eq!(super::pick_machine(vec![None, None]), None);
        assert_eq!(super::pick_machine(vec![Some("???".into())]), None);
    }

    /// The regression itself: whatever the platform, the list we hand to
    /// `pick_machine` must contain a source that platform can actually answer.
    #[test]
    fn this_platform_offers_at_least_one_answerable_source() {
        let sources = super::machine_sources();
        assert!(!sources.is_empty());
        assert!(
            sources.iter().any(Option::is_some),
            "no machine-name source answered on this platform"
        );
    }

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

    /// B54. The short form exists to distinguish sessions. Ext ids are
    /// `platform.sessionId`, so anything that only looks at the head is
    /// printing the platform name, not the session.
    #[test]
    fn short_id_distinguishes_ext_ids_that_differ_only_in_the_tail() {
        let a = short_session_id("deepseek.d41f6a2b9c0e47aaaa1111");
        let b = short_session_id("deepseek.d41f6a2b9c0e47aaaa2222");
        assert_ne!(a, b, "same short id for two different sessions: {a}");
        assert!(a.starts_with("deepseek~"), "{a}");
        // Pinned against an independent tool, so the doc example above and the
        // digest choice cannot drift:
        //   printf '%s' 'deepseek.d41f6a2b9c0e47aaaa1111' | shasum -a 256
        assert_eq!(a, "deepseek~e4009d");
    }

    /// Same argument for the native shape, which is `<source>.<machine>.<uuid>`
    /// and therefore *also* has a constant head.
    #[test]
    fn short_id_distinguishes_native_ids_from_the_same_machine() {
        let a = short_session_id("codex.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
        let b = short_session_id("codex.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09766");
        assert_ne!(a, b);
    }

    #[test]
    fn short_id_is_stable_short_and_never_the_full_id() {
        let id = "019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        assert_eq!(
            short_session_id(id),
            short_session_id(id),
            "no rng, no clock"
        );
        assert!(
            short_session_id(id).starts_with("019bf00d"),
            "readable head"
        );
        assert_eq!(short_session_id(id).len(), 15, "fits a table column");
        assert!(!short_session_id(id).contains("eacbacc09765"));
    }

    #[test]
    fn short_id_handles_ids_shorter_than_the_head() {
        // No panic, and still distinguishing.
        assert_ne!(short_session_id("a"), short_session_id("b"));
        assert!(short_session_id("a").starts_with("a~"));
        assert_eq!(short_session_id(""), short_session_id(""));
    }

    /// W680. The rule the field failure turned into: a value at or under the
    /// budget comes back byte for byte, so nothing already written under it
    /// moves. Pinned on the boundary from both sides, because "under" and "at"
    /// are the two cases a `<=`/`</` slip lands on.
    #[test]
    fn bounded_component_returns_a_value_within_budget_verbatim() {
        let budget = 64;
        let exact = "a".repeat(budget);
        assert_eq!(bounded_path_component(&exact, budget), exact);
        let under = "a".repeat(budget - 1);
        assert_eq!(bounded_path_component(&under, budget), under);
        // Not even a length rule when there is nothing to enforce.
        assert_eq!(bounded_path_component("", 0), "");
    }

    /// The property the bound exists for, stated as arithmetic rather than as a
    /// filesystem call (the integration test does that): whatever goes in, what
    /// comes out fits.
    #[test]
    fn bounded_component_never_exceeds_the_budget() {
        for budget in [33, 40, 64, MAX_PATH_COMPONENT_BYTES] {
            for length in [0, 1, 32, 33, budget, budget + 1, 2_000] {
                let value = "v".repeat(length);
                let bounded = bounded_path_component(&value, budget);
                assert!(
                    bounded.len() <= budget,
                    "budget {budget}, input {length}: got {} bytes",
                    bounded.len()
                );
            }
        }
    }

    /// The head is a prefix, so the bounded name still reads as the thing it
    /// stands for, and the tag after it carries the part that was dropped. Both
    /// numbers are pinned, so the digest choice cannot drift from the rule the
    /// doc comment states: the tag is the first 32 hex digits of the SHA-256 of
    /// the **whole** value (`printf '%s' "$value" | shasum -a 256 | cut -c1-32`).
    #[test]
    fn bounded_component_keeps_a_readable_head_and_a_tag() {
        let value = format!("oc-6d61696e-{}", "30316636373364632d".repeat(40));
        let bounded = bounded_path_component(&value, 176);
        assert!(
            bounded.starts_with("oc-6d61696e-3031"),
            "the head is not the value's own head: {bounded}"
        );
        assert!(
            bounded.ends_with("~e3abeae592e755f9f1d6e1a7518cf932"),
            "{bounded}"
        );
        let (head, tag) = bounded.rsplit_once('~').expect("a tag after the last ~");
        assert_eq!(head.len(), 143, "the head fills what the tag leaves");
        assert_eq!(tag.len(), PATH_COMPONENT_TAG_HEX);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()), "{tag}");
        assert!(
            value.starts_with(head),
            "the head is not a prefix of the value"
        );
    }

    /// Two values that share every byte a head can hold are still two values.
    /// This is the case a prefix truncation cannot answer, and the one the
    /// OpenClaw cold archives are made of: their file names differ only in a
    /// trailing timestamp and hash.
    #[test]
    fn bounded_component_distinguishes_values_that_share_a_head() {
        let head = format!("oc-6d61696e-{}", "ab".repeat(200));
        let tag_of = |tail: &str| {
            bounded_path_component(&format!("{head}{tail}"), 176)
                .rsplit_once('~')
                .map(|(_, tag)| tag.to_string())
                .expect("a tag after the last ~")
        };
        let first = tag_of("-01-01.201Z.aa.zst");
        assert_eq!(first.len(), PATH_COMPONENT_TAG_HEX);
        assert_ne!(first, tag_of("-01-09.201Z.bb.zst"), "trailing hash");
        assert_ne!(first, tag_of("-09-01.201Z.aa.zst"), "trailing timestamp");
        assert_ne!(first, tag_of("-01-01.202Z.aa.zst"), "trailing year");
    }

    /// Deterministic across calls: no clock, no rng, nothing machine-specific.
    /// This is what makes the bounded form an identity rather than a per-run
    /// name, and the reason it is safe to use as one.
    #[test]
    fn bounded_component_is_a_pure_function() {
        let value = "x".repeat(400);
        assert_eq!(
            bounded_path_component(&value, 176),
            bounded_path_component(&value, 176)
        );
        // The same value under a different budget is a different name, and both
        // fit — which is why a budget is a documented choice per caller rather
        // than a default somebody may assume.
        let wider = bounded_path_component(&value, 200);
        assert_ne!(wider, bounded_path_component(&value, 176));
        assert!(wider.len() <= 200);
    }

    /// A multi-byte character is never cut in half. A native id that is not
    /// hex-encoded — a file stem from a source harness, or a chat platform's own
    /// session id — can carry any UTF-8, and a component ending in a half
    /// character is a name no filesystem will accept.
    #[test]
    fn bounded_component_never_splits_a_character() {
        let bounded = bounded_path_component(&"é".repeat(300), 101);
        assert!(bounded.len() <= 101, "{} bytes", bounded.len());
        let (head, tag) = bounded.rsplit_once('~').expect("a tag after the last ~");
        assert_eq!(tag.len(), PATH_COMPONENT_TAG_HEX);
        assert!(head.starts_with("é"), "the head is not the value's head");
        // Every character of the head survived whole, so the head is still an
        // even number of bytes: two per `é`, never one and a half.
        assert_eq!(head.chars().count() * 2, head.len(), "{head}");
    }

    /// A budget too small to hold a head yields the tag alone, and one too small
    /// for the tag shortens the tag — bounded in every case, and never a panic.
    #[test]
    fn bounded_component_survives_a_budget_smaller_than_its_tag() {
        let value = "v".repeat(400);
        for budget in [0, 1, 8, PATH_COMPONENT_TAG_HEX - 1, PATH_COMPONENT_TAG_HEX] {
            let bounded = bounded_path_component(&value, budget);
            assert!(
                bounded.len() <= budget,
                "budget {budget}: got {} bytes",
                bounded.len()
            );
            // What survives is the value's own head, when there is room for a
            // head at all; below 33 bytes there is only the tag, which is a
            // digest of the whole value and deliberately not a prefix of it.
            if let Some((head, _)) = bounded.split_once('~') {
                assert!(value.starts_with(head), "the head is not a prefix: {head}");
            }
        }
    }
}
