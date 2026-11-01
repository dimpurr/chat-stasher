//! ADR-056 D1 — what a parsed session-index declaration actually holds.
//!
//! The behavioural half of D1 (a declaration this build cannot honour must stop
//! the registry from loading) is in
//! `tests/paseo_session_index_registry_test.rs`, and that file deliberately
//! never names a field, so it compiled and failed against the parser before the
//! parser changed. This file is the other half: once a declaration is accepted,
//! its id, adapter and root must survive
//! parsing, and accepting it must not disturb the body products beside it.

use chat_stasher::scanner::{self, RegistryHarness, REGISTRY_REL_PATH};
use std::fs;

const INDEX_JSON: &str = r#"[
  {"id": "agents", "adapter": "paseo-agent-index", "paths": {
    "macos": {"template": "~/.synthetic-host/agents/<cwd-class>/<agent-id>.json",
              "format": "agent-index", "confidence": "measured-locally"}}}
]"#;

fn harness_body(extra: &str) -> String {
    format!(
        r#"{{
      "id": "synthetic-multi-product",
      "display_name": "Synthetic Multi Product",
      "paths": {{"macos": {{"template": "~/.synthetic-host/sessions", "format": "jsonl",
                           "confidence": "source-confirmed"}}}},
      "source_roots": [{{"id": "legacy",
        "paths": {{"macos": {{"template": "~/.synthetic-host/legacy", "format": "jsonl",
                             "confidence": "source-confirmed"}}}}}}]{extra}
    }}"#
    )
}

fn parse(harnesses: &str) -> Vec<RegistryHarness> {
    let text =
        format!(r#"{{"schema_version":1,"generated":"2026-01-01","harnesses":[{harnesses}]}}"#);
    let dir = tempfile::tempdir().expect("temp registry root");
    let path = dir.path().join("harness-registry-synthetic.json");
    fs::write(&path, text).expect("write synthetic registry");
    scanner::load_registry(&path)
        .expect("a well-formed declaration must load")
        .harnesses
}

#[test]
fn an_accepted_index_keeps_its_id_adapter_and_root() {
    let harnesses = parse(&harness_body(&format!(
        ",\"session_indexes\": {INDEX_JSON}"
    )));
    let indexes = &harnesses[0].session_indexes;
    assert_eq!(indexes.len(), 1, "one declaration, one product");
    assert_eq!(indexes[0].id, "agents");
    assert_eq!(
        indexes[0].adapter,
        chat_stasher::paseo::AGENT_INDEX_ADAPTER,
        "the adapter is the reader's name, kept verbatim"
    );
    let macos = indexes[0]
        .paths
        .cell_for("macos")
        .expect("the declared cell survives parsing");
    assert_eq!(
        macos.template, "~/.synthetic-host/agents/<cwd-class>/<agent-id>.json",
        "an index root is ordinary RegistryPaths: the same template a body cell carries"
    );
    assert_eq!(macos.confidence, "measured-locally");
    assert!(
        indexes[0].paths.cell_for("windows").is_none(),
        "no cell for a platform that declared none — a missing cell stays missing, \
         it is not filled with a guess"
    );
}

#[test]
fn a_harness_without_the_key_has_no_indexes() {
    let harnesses = parse(&harness_body(""));
    assert!(
        harnesses[0].session_indexes.is_empty(),
        "default empty is what keeps every pre-D1 registry entry meaning the same thing"
    );
}

#[test]
fn accepting_an_index_leaves_the_body_products_alone() {
    // The additive half of D1, checked on the parsed result rather than on the
    // outcome: body paths, source roots and the index are three separate
    // products of one harness, and none of them is displaced by another.
    let harnesses = parse(&harness_body(&format!(
        ",\"session_indexes\": {INDEX_JSON}"
    )));
    let harness = &harnesses[0];
    assert!(harness.paths.cell_for("macos").is_some());
    assert_eq!(harness.source_roots.len(), 1);
    assert_eq!(harness.source_roots[0].id, "legacy");
    assert_eq!(harness.session_indexes.len(), 1);
    assert_eq!(harness.session_indexes[0].id, "agents");
}

#[test]
fn the_shipped_registry_declares_no_index_yet() {
    // True as of this commit, and worth pinning: the shipped Paseo row, its
    // confidence evidence and its support-matrix entry are a later step, so
    // nothing here may claim a real index is being scanned yet.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(REGISTRY_REL_PATH);
    let registry = scanner::load_registry(&path).expect("the shipped registry loads");
    assert!(
        registry
            .harnesses
            .iter()
            .all(|harness| harness.session_indexes.is_empty()),
        "no shipped harness declares a session index in this commit"
    );
}
