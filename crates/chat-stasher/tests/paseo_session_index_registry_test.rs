//! ADR-056 D1 — a harness may declare session indexes next to its body roots,
//! and a declaration this build cannot honour must be refused while the registry
//! is being parsed instead of being read back as "this harness has nothing".
//!
//! The distinction the whole file is about: `load_registry` succeeding over a
//! malformed `session_indexes` block is not a pass, it is the failure mode the
//! ADR names ("silently claiming that nothing was found"), because the caller
//! cannot tell an absent declaration from a rejected one. So every rejection
//! test here asserts on the *outcome of the public load path*, never on a field
//! shape.
//!
//! All harness ids, templates and adapter values are invented for this file. The
//! one real file opened is the shipped registry, and only to prove an old
//! registry without any `session_indexes` key still loads unchanged.

use chat_stasher::scanner::{self, REGISTRY_REL_PATH};
use std::fs;
use std::io;

/// Wrap harness entries in the top-level registry envelope.
fn registry(harnesses: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "generated": "2026-01-01",
  "harnesses": [
{harnesses}
  ]
}}"#
    )
}

/// One body-only harness: `paths` plus the two fields the type has always
/// required, so the envelope is as small as a real pre-D1 entry.
fn body_harness(id: &str) -> String {
    format!(
        r#"{{
      "id": "{id}",
      "display_name": "{id} display",
      "paths": {{
        "macos": {{
          "template": "~/.synthetic-host/sessions",
          "format": "jsonl",
          "confidence": "source-confirmed"
        }}
      }}
    }}"#
    )
}

/// `body_harness` with a raw `session_indexes` array spliced in. The array is
/// written as raw JSON so a test can declare a shape the type may not accept.
fn indexed_harness(id: &str, indexes: &str) -> String {
    let body = body_harness(id);
    let splice_at = body.rfind('}').expect("body_harness emits a closing brace");
    format!(
        "{},\n      \"session_indexes\": {indexes}\n    }}",
        &body[..splice_at]
    )
}

/// Load through the public path a user's edited registry actually takes.
fn load(text: &str) -> io::Result<scanner::HarnessRegistry> {
    let dir = tempfile::tempdir().expect("temp registry root");
    let path = dir.path().join("harness-registry-synthetic.json");
    fs::write(&path, text).expect("write synthetic registry");
    scanner::load_registry(&path)
}

const ONE_INDEX: &str = r#"[
        {"id": "agents", "adapter": "paseo-agent-index", "paths": {
          "macos": {"template": "~/.synthetic-host/agents", "format": "agent-index",
                    "confidence": "measured-locally"}}}
      ]"#;

// --- rejections: each of these is a declaration the scanner cannot honour ----

#[test]
fn unknown_index_adapter_is_rejected() {
    let text = registry(&indexed_harness(
        "synthetic-index-host",
        r#"[{"id": "agents", "adapter": "totem-pole-index", "paths": {}}]"#,
    ));
    let err = load(&text)
        .err()
        .expect("an adapter this build has no reader for must be refused, not ignored");
    let message = err.to_string();
    assert!(
        message.contains("totem-pole-index"),
        "the rejection must name the adapter it refused, got: {message}"
    );
}

#[test]
fn empty_index_adapter_is_rejected() {
    let text = registry(&indexed_harness(
        "synthetic-index-host",
        r#"[{"id": "agents", "adapter": "", "paths": {}}]"#,
    ));
    load(&text)
        .err()
        .expect("an empty adapter is not an adapter");
}

#[test]
fn missing_index_adapter_is_rejected() {
    let text = registry(&indexed_harness(
        "synthetic-index-host",
        r#"[{"id": "agents", "paths": {}}]"#,
    ));
    load(&text)
        .err()
        .expect("an index entry with no adapter discriminator must not parse");
}

#[test]
fn empty_index_id_is_rejected() {
    let text = registry(&indexed_harness(
        "synthetic-index-host",
        r#"[{"id": "", "adapter": "paseo-agent-index", "paths": {}}]"#,
    ));
    let err = load(&text)
        .err()
        .expect("an index id is the stable name a report cites; empty must be refused");
    assert!(
        err.to_string().contains("synthetic-index-host"),
        "the rejection must name the offending harness, got: {err}"
    );
}

#[test]
fn duplicate_index_id_in_one_harness_is_rejected() {
    let text = registry(&indexed_harness(
        "synthetic-index-host",
        r#"[{"id": "agents", "adapter": "paseo-agent-index", "paths": {}},
            {"id": "agents", "adapter": "paseo-agent-index", "paths": {}}]"#,
    ));
    let err = load(&text)
        .err()
        .expect("two indexes with one id cannot be told apart in a report or a cursor");
    assert!(
        err.to_string().contains("agents"),
        "the rejection must name the duplicated id, got: {err}"
    );
}

// --- what must keep working -------------------------------------------------

#[test]
fn a_declared_index_is_accepted() {
    let text = registry(&indexed_harness("synthetic-index-host", ONE_INDEX));
    load(&text).expect("a known adapter with a stable id must load");
}

#[test]
fn body_roots_and_an_index_coexist_on_one_harness() {
    // ADR-056 D1: the two products are additive, not alternatives. A harness
    // that declares both must load, and so must one that declares only the
    // index (an index-only source is the reason there is no capture_kind).
    let both = registry(&indexed_harness("synthetic-index-host", ONE_INDEX));
    load(&both).expect("body paths plus an index declaration must load");

    let index_only = registry(
        r#"{
      "id": "synthetic-index-only",
      "display_name": "Synthetic Index Only",
      "session_indexes": [
        {"id": "agents", "adapter": "paseo-agent-index", "paths": {
          "macos": {"template": "~/.synthetic-host/agents", "format": "agent-index",
                    "confidence": "measured-locally"}}}
      ]
    }"#,
    );
    load(&index_only).expect("an index-only harness must load with no body paths");
}

#[test]
fn source_roots_and_indexes_coexist() {
    let text = r#"{
  "schema_version": 1,
  "generated": "2026-01-01",
  "harnesses": [{
      "id": "synthetic-multi-root",
      "display_name": "Synthetic Multi Root",
      "paths": {"macos": {"template": "~/.synthetic-host/a", "format": "jsonl",
                          "confidence": "source-confirmed"}},
      "source_roots": [{
        "id": "legacy",
        "paths": {"macos": {"template": "~/.synthetic-host/b", "format": "jsonl",
                           "confidence": "source-confirmed"}}
      }],
      "session_indexes": [
        {"id": "agents", "adapter": "paseo-agent-index", "paths": {
          "macos": {"template": "~/.synthetic-host/agents", "format": "agent-index",
                    "confidence": "measured-locally"}}}
      ]
  }]
}"#;
    load(text).expect("an index adds a product, it does not displace source roots");
}

#[test]
fn one_index_id_across_two_harnesses_is_accepted() {
    // `id` is stable within its harness — the report cites `harness/index`, so
    // two harnesses each having an `agents` index is ordinary, not a clash.
    let text = registry(&format!(
        "    {},\n    {}",
        indexed_harness("synthetic-host-a", ONE_INDEX),
        indexed_harness("synthetic-host-b", ONE_INDEX)
    ));
    load(&text).expect("the same index id under two harnesses is not a duplicate");
}

#[test]
fn a_registry_without_session_indexes_still_loads() {
    // Every registry written before D1 has no such key at all. That must load
    // exactly as it did, which is what "optional, default empty" has to mean.
    let text = registry(&body_harness("synthetic-old-host"));
    let parsed = load(&text).expect("a pre-D1 registry must still load");
    assert_eq!(parsed.harnesses.len(), 1);
}

#[test]
fn unknown_extra_registry_metadata_is_still_tolerated() {
    // Registry types deliberately stay open: a newer registry that carries a
    // key this build has never heard of still loads, and only a malformed
    // session-index declaration is refused. Tightening this would make every
    // registry edit a version gate.
    let text = r#"{
  "schema_version": 1,
  "generated": "2026-01-01",
  "reviewed_by": "nobody-synthetic",
  "harnesses": [{
      "id": "synthetic-future-host",
      "display_name": "Synthetic Future Host",
      "paths": {"macos": {"template": "~/.synthetic-host/sessions", "format": "jsonl",
                          "confidence": "source-confirmed"}},
      "watch_me": {"plan": "later"}
  }]
}"#;
    load(text).expect("unknown registry metadata is tolerated, not rejected");
}

#[test]
fn the_shipped_registry_still_loads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(REGISTRY_REL_PATH);
    scanner::load_registry(&path)
        .expect("the shipped registry must keep loading once session indexes exist");
}
