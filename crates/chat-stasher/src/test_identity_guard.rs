//! Code-level fail-safe: a test fixture identity must never reach a real user's
//! archive or coordination state (W306).
//!
//! The suite builds its fixtures out of reserved identities — `synthetic-session`,
//! `synthetic-install`, `b83.synthetic-session`, `w292.synthetic-session-one` —
//! and hands the production write paths (`inbox::seal_payload`, the EXT-3
//! coordination transaction) those identities directly. That is deliberate: the
//! tests exercise the real code. But it also means the *only* thing standing
//! between a fixture identity and the author's real
//! `~/.local/share/chat-stasher/stage/…` is the test's own sandbox. On
//! 2026-10-02 a test run resolved the real data root and planted
//! `stage/sessions/<machine>/chatgpt.synthetic-session` and a `synthetic-install`
//! row in the real `extension-coordination.sqlite3`. The sandbox held in every
//! other test; the one that lost it wrote to the author's machine.
//!
//! The environment can be isolated — and W306 does isolate it — but an
//! environment-only fix is a promise each new test has to keep. This module is
//! the backstop that does not depend on the test getting it right: it inspects
//! the *identity* at the write boundary and refuses when a reserved fixture
//! identity is aimed at a destination outside the process temp directory.
//!
//! Three properties make this safe in production:
//!
//!  · It is keyed on a **reserved namespace**. A fixture identity is one whose
//!    dot/dash/underscore-separated components include `synthetic`, `fixture`,
//!    `probe` or `dummy` as a whole token, compared case-insensitively. Real
//!    users do not name their conversations that; the namespace is documented
//!    here as reserved.
//!  · It is keyed on the **destination**, not on `cfg(test)`. A fixture written
//!    under a temp root — every correct test — passes. Only a fixture headed
//!    outside temp is refused, which is exactly the incident.
//!  · It **fails closed for the fixture namespace only**: an ordinary identity is
//!    untouched no matter where it goes, so no real delivery can be refused by
//!    this check.
//!
//! It is deliberately not a `cfg(test)` guard: the spawned-binary tests run
//! production code with no `cfg(test)` at all, and the incident was reached
//! through production code. The check has to live where the write happens.

use std::path::Path;

/// Whole-token names reserved for test scaffolding. An identity is a fixture
/// identity when one of its `.`/`-`/`_`-separated components equals one of
/// these, compared case-insensitively.
///
/// `chatgpt.synthetic-session` is the incident's own spelling: the session id
/// `synthetic-session` composes with the platform into a component
/// `synthetic-session`, whose tokens are `synthetic` and `session`.
///
/// `probe` and `dummy` are here because the suite uses them for the same
/// purpose (`w306-probe-machine`, `dummy-install`), and case-folding is here
/// because nothing stops a fixture from being spelled `Synthetic-session` —
/// the namespace is a contract, not a spelling. This list is restated in
/// `scripts/dev/check-test-isolation.sh`, whose selftest reads this constant
/// back out of the source so the two cannot drift apart.
pub const FIXTURE_IDENTITY_TOKENS: &[&str] = &["synthetic", "fixture", "probe", "dummy"];

/// Is `id` a reserved test fixture identity?
///
/// The split is on every non-alphanumeric ASCII character, so `chatgpt.synthetic-session`,
/// `synthetic-install`, `b83.synthetic-session`, `w292.synthetic-session-one`,
/// `Synthetic-session` and `b83-fixture` all answer `true`, while an ordinary
/// id that merely contains the letters (`photosynthetic-blend`,
/// `reprobe-session`) answers `false` because no whole token matches.
pub fn is_fixture_identity(id: &str) -> bool {
    id.split(|c: char| !c.is_ascii_alphanumeric()).any(|token| {
        FIXTURE_IDENTITY_TOKENS
            .iter()
            .any(|reserved| token.eq_ignore_ascii_case(reserved))
    })
}

/// Is `path` inside the process temp directory?
///
/// Purely lexical, on purpose: at the moment of the check the destination
/// directory usually does not exist yet, so `canonicalize` would fail or, on
/// macOS, resolve `/var/folders/…` to `/private/var/folders/…` and make the two
/// spellings disagree. `Path::starts_with` is component-wise, so it is not
/// fooled by a prefix that is only a string prefix
/// (`/tmpfoo` does not start with `/tmp`).
pub fn is_under_temp_dir(path: &Path) -> bool {
    path.starts_with(std::env::temp_dir())
}

/// Refuse a write of any fixture identity in `ids` to `destination_root` when
/// that root is outside the process temp directory.
///
/// `destination_root` is the directory the write lands in — a stage root, a
/// state directory — not the individual file, so a caller does not have to
/// know the leaf name the writer will choose.
///
/// Returns `Ok(())` when no id is a fixture identity, or when every fixture id
/// is headed for a temp destination. Returns `Err` (with the offending id and
/// both paths named) otherwise.
pub fn refuse_fixture_write(ids: &[&str], destination_root: &Path) -> anyhow::Result<()> {
    let Some(id) = ids.iter().copied().find(|id| is_fixture_identity(id)) else {
        return Ok(());
    };
    if is_under_temp_dir(destination_root) {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to write fixture identity `{id}` outside a temporary directory: \
         destination {} is not under the process temp directory {}",
        destination_root.display(),
        std::env::temp_dir().display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_incident_identity_is_a_fixture_identity() {
        // The exact staged directory name and install id from the 2026-10-02
        // incident, plus the shapes the suite actually uses.
        for id in [
            "chatgpt.synthetic-session",
            "synthetic-install",
            "synthetic-session",
            "b83.synthetic-session",
            "w292.synthetic-session-one",
            "b83-fixture",
            "fixture-install",
            "w306-probe-machine",
            "dummy-install",
        ] {
            assert!(is_fixture_identity(id), "`{id}` must be reserved");
        }
    }

    #[test]
    fn a_fixture_identity_is_reserved_whatever_its_case() {
        // The namespace is a contract, not a spelling: a fixture that happens
        // to be capitalized must not slip past the fail-safe.
        for id in [
            "chatgpt.Synthetic-session",
            "PROBE-install",
            "Dummy.session",
            "Fixture.Install",
        ] {
            assert!(is_fixture_identity(id), "`{id}` must be reserved");
        }
    }

    #[test]
    fn an_ordinary_identity_is_not_reserved() {
        // A token has to match whole: a real id is not hijacked by a substring.
        for id in [
            "chatgpt.6a4bb1c6-458c-83eb-a146-676418a2f960",
            "photosynthetic-blend",
            "my-fixtures-collection",
            "claude.28bca09c-196b-4521-8c29-b8de23343d00",
            "reprobe-session",
            "dummyish-install",
            "",
        ] {
            assert!(!is_fixture_identity(id), "`{id}` must not be reserved");
        }
    }

    #[test]
    fn a_fixture_identity_under_temp_is_allowed() {
        let temp = tempfile::tempdir().expect("temp root");
        let stage = temp.path().join("stage");
        refuse_fixture_write(&["chatgpt.synthetic-session"], &stage)
            .expect("a fixture identity under the temp root is the correct test arrangement");
    }

    #[test]
    fn a_fixture_identity_outside_temp_is_refused() {
        // Deliberately not a real user directory and not under temp: this is a
        // read-only judgement about a path, and nothing here creates it.
        let elsewhere = std::env::current_dir()
            .expect("current dir")
            .join("w306-guard-probe-not-a-real-destination");
        let err = refuse_fixture_write(&["synthetic-install"], &elsewhere)
            .expect_err("a fixture identity outside temp must be refused");
        let text = err.to_string();
        assert!(text.contains("synthetic-install"), "{text}");
        assert!(text.contains("outside a temporary directory"), "{text}");
    }

    #[test]
    fn an_ordinary_identity_outside_temp_is_untouched() {
        let elsewhere = Path::new("/var/lib/chat-stasher");
        refuse_fixture_write(&["chatgpt.6a4bb1c6-458c-83eb-a146-676418a2f960"], elsewhere)
            .expect("this guard must never refuse a real identity");
    }
}
