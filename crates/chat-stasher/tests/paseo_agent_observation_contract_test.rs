//! ADR-056 D5 — the one Paseo agent observation contract: its identity, its
//! validator, and its agreement with the published schema.
//!
//! Three properties are worth the file:
//!
//! * **One fact, one identity.** `observation_id` covers the content and not the
//!   moment, so an agent whose facts have not changed hashes to the id already
//!   on disk. Every test that changes a fact also says which fact it changed.
//! * **Two laws, one row.** `crate::paseo` and
//!   `contracts/paseo-agent-observation.schema.json` must accept and refuse the
//!   same rows. Where one law is deliberately weaker than the other the test
//!   *names the divergence* rather than loosening the assertion, because a
//!   silently weaker schema is how a corrupt row gets archived.
//! * **Nothing here locates a file.** A row carrying `cwd`, `path`, `env` or any
//!   other key outside the whitelist is refused by both laws.
//!
//! All values are synthetic. The agent ids, the canonical id, the timestamps and
//! the digests are invented here; no real daemon, agent or transcript is used.

use chat_stasher::paseo::{
    self, AgentObservation, AgentObservationRow, AgentProjection, BodyState, SCHEMA,
};
use serde_json::{json, Value};
use std::sync::LazyLock;

/// A complete agent UUID, invented for this file.
const AGENT: &str = "0e3a7f2c-1b64-4f1e-9d2a-6c5f8b1e7d40";
const PARENT: &str = "7c9d0e5b-2a3f-4b8c-a1d6-0e4f9c2b5a83";
/// A Codex rollout filename stem — an id its own harness spelled, not a bare
/// UUID, and it must survive the contract unchanged.
const NATIVE: &str = "rollout-2026-10-09-13-05-12-0e3a7f2c-1b64-4f1e-9d2a-6c5f8b1e7d40";
/// `<harness>.<machine>.<native-id>`, spelled the way `crate::id` spells it.
const CANONICAL: &str =
    "codex.synthetic-machine.rollout-2026-10-09-13-05-12-0e3a7f2c-1b64-4f1e-9d2a-6c5f8b1e7d40";
const BODY_SHA: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0";
const FIRST_SEEN: &str = "2026-10-09T13:05:12Z";
const SEEN_AGAIN: &str = "2026-10-10T06:41:00Z";

static SCHEMA_VALUE: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../contracts/paseo-agent-observation.schema.json"
    ))
    .expect("the committed Paseo observation schema is valid JSON")
});
static SCHEMA_OK: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    jsonschema::options()
        .should_validate_formats(false)
        .build(&SCHEMA_VALUE)
        .expect("the committed Paseo observation schema compiles")
});

/// The full projection, every whitelisted field recorded.
fn full_projection() -> AgentProjection {
    AgentProjection {
        workspace_id: Some("workspace-4f1e".to_string()),
        parent_agent_id: Some(PARENT.to_string()),
        title: Some("Reconcile the shard cursor".to_string()),
        status: Some("idle".to_string()),
        created_at: Some("2026-10-08T21:00:00Z".to_string()),
        updated_at: Some("2026-10-09T12:58:31Z".to_string()),
        last_activity_at: Some("2026-10-09T13:04:59Z".to_string()),
        archived_at: None,
    }
}

fn observation() -> AgentObservation {
    AgentObservation {
        daemon_key: "srv9k2m4p7q1".to_string(),
        agent_id: AGENT.to_string(),
        provider: "codex".to_string(),
        native_session_id: Some(NATIVE.to_string()),
        projection: full_projection(),
        body: BodyState::Archived {
            canonical_session_id: CANONICAL.to_string(),
            shard_sequences: vec![1, 2, 7],
            body_sha256: BODY_SHA.to_string(),
        },
    }
}

fn row() -> AgentObservationRow {
    AgentObservationRow::new(observation(), FIRST_SEEN)
}

/// A valid row, then one edit applied to its JSON, then both laws asked about it.
fn tampered(edit: impl FnOnce(&mut Value)) -> Value {
    let mut value = serde_json::to_value(row()).expect("a row is a plain data tree");
    edit(&mut value);
    value
}

fn parse(value: &Value) -> anyhow::Result<Vec<AgentObservationRow>> {
    paseo::parse_rows(&serde_json::to_string(value).expect("a row is a plain data tree"))
}

/// Must be refused, and the message must say which rule broke without quoting
/// an archived value.
fn refused(value: &Value) -> String {
    let error = match parse(value) {
        // The row is deliberately not printed here: it carries a title, and a
        // failing test log is exactly the place an archived value must not turn up.
        Ok(rows) => panic!("a corrupt row was accepted as {} valid rows", rows.len()),
        Err(error) => error.to_string(),
    };
    assert!(
        !error.contains("Reconcile the shard cursor"),
        "a rejection must not repeat an archived title: {error}"
    );
    error
}

fn schema_refused(value: &Value) {
    assert!(
        !SCHEMA_OK.is_valid(value),
        "the published schema accepted a row it must refuse: {}",
        SCHEMA_OK
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    );
}

fn schema_accepted(value: &Value) {
    assert!(
        SCHEMA_OK.is_valid(value),
        "the published schema refused a row the code accepts: {}",
        SCHEMA_OK
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    );
}

/// Refused by both laws. The default, and the only way the two stay honest about
/// the same rows.
fn both_refuse(value: &Value) -> String {
    let error = refused(value);
    schema_refused(value);
    error
}

// --- identity ---------------------------------------------------------------

#[test]
fn a_recorded_row_is_valid_under_both_laws() {
    let row = row();
    row.validate().expect("the reference row is valid");
    assert_eq!(
        row.observation_id,
        row.semantic_sha256(),
        "a stamped row's identity is its own content's digest"
    );
    let encoded = serde_json::to_value(&row).expect("a row is a plain data tree");
    assert_eq!(parse(&encoded).unwrap(), vec![row]);
    schema_accepted(&encoded);
}

#[test]
fn re_observing_unchanged_facts_yields_the_identity_already_on_disk() {
    // The whole point of excluding observed_at: an unchanged agent appends
    // nothing (ADR-056 D5).
    let first = AgentObservationRow::new(observation(), FIRST_SEEN);
    let again = AgentObservationRow::new(observation(), SEEN_AGAIN);
    assert_eq!(
        first.observation_id, again.observation_id,
        "the same facts must not mint a second row"
    );
    assert_ne!(first.observed_at, again.observed_at);
    assert_ne!(
        first, again,
        "the two rows still differ in when they were seen"
    );
    assert!(parse(&serde_json::to_value(&again).unwrap())
        .unwrap()
        .iter()
        .all(|row| row.observation_id == first.observation_id));
}

#[test]
fn the_identity_is_idempotent_within_one_process() {
    let observation = observation();
    assert_eq!(
        observation.semantic_sha256(),
        observation.semantic_sha256(),
        "hashing the same facts twice yields the same digest"
    );
}

#[test]
fn every_recorded_change_yields_a_new_identity() {
    let base = row().observation_id;
    let mut cases: Vec<(String, AgentObservation)> = Vec::new();

    let mut renamed = observation();
    renamed.projection.title = Some("Reconcile the shard cursor, take two".to_string());
    cases.push(("title".to_string(), renamed));

    let mut closed = observation();
    closed.projection.status = Some("closed".to_string());
    cases.push(("status".to_string(), closed));

    let mut rearchived = observation();
    rearchived.projection.archived_at = Some("2026-10-11T02:14:00Z".to_string());
    cases.push(("archived_at".to_string(), rearchived));

    let mut unnamed = observation();
    unnamed.projection = AgentProjection::default();
    cases.push(("every projection field dropped".to_string(), unnamed));

    let mut moved = observation();
    moved.native_session_id = Some("rollout-2026-10-09-13-05-12-7c9d0e5b".to_string());
    cases.push(("recorded native_session_id".to_string(), moved));

    let mut grok = observation();
    grok.provider = "grok".to_string();
    cases.push(("provider".to_string(), grok));

    let mut daemon = observation();
    daemon.daemon_key = "srv7x8y9z0".to_string();
    cases.push(("daemon_key".to_string(), daemon));

    let mut extra_shard = observation();
    if let BodyState::Archived {
        shard_sequences, ..
    } = &mut extra_shard.body
    {
        shard_sequences.push(8);
    }
    cases.push(("shard set".to_string(), extra_shard));

    let mut lost_body = observation();
    lost_body.body = BodyState::Missing;
    cases.push(("body state".to_string(), lost_body));

    for (what, case) in cases {
        let id = case.semantic_sha256();
        assert_ne!(
            base, id,
            "a change to {what} is a different fact and must hash differently"
        );
        case.validate()
            .unwrap_or_else(|error| panic!("the {what} case is itself invalid: {error}"));
    }
}

#[test]
fn an_explicit_null_is_the_same_fact_as_an_absent_field() {
    // Two producers, two spellings, one identity: `"archived_at": null` and no
    // key both mean "the source recorded nothing", and a row must not fork into
    // two ids over the difference.
    let absent = AgentObservationRow::new(
        AgentObservation {
            projection: AgentProjection {
                archived_at: None,
                ..full_projection()
            },
            ..observation()
        },
        FIRST_SEEN,
    );
    let mut spelled = serde_json::to_value(&absent).expect("plain data tree");
    spelled["projection"]["archived_at"] = Value::Null;
    let parsed = parse(&spelled).expect("the null spelling parses to the same row");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0], absent);
    assert_eq!(
        parsed[0].observation_id, absent.observation_id,
        "one fact must keep one id across both spellings"
    );
}

#[test]
fn a_row_round_trips_through_exactly_one_line() {
    let line = row().to_line().expect("one line");
    assert!(!line.contains('\n'));
    let parsed = paseo::parse_rows(&format!("{line}\n")).expect("one line reads back");
    assert_eq!(parsed, vec![row()]);
}

// --- the two laws over the same corruptions ----------------------------------

#[test]
fn the_whitelist_is_a_whitelist_under_both_laws() {
    // ADR-056 D5 keeps no source-file locator, no configuration and no
    // credential in this contract, and `cwd` is existing provenance that
    // travels separately (`crate::provenance`), so none of them has a field.
    for (key, value) in [
        ("cwd", json!("/synthetic-root/agents")),
        ("path", json!("/synthetic-root/agents/0e3a.json")),
        ("config", json!({"PASEO_HOME": "/synthetic-home"})),
        ("env", json!({"server_id": "srv_abc"})),
        ("api_key", json!("sk-synthetic")),
    ] {
        let value = tampered(|row| row[key] = value.clone());
        let error = both_refuse(&value);
        assert!(
            error.contains("unknown field"),
            "the Rust validator must say the key is not part of the contract: {error}"
        );
    }
    for (key, value) in [
        ("cwd", json!("/synthetic-root/agents")),
        ("labels", json!({"paseo.parent-agent-id": PARENT})),
        ("persistence", json!({"sessionId": NATIVE})),
        ("runtimeInfo", json!({"provider": "codex"})),
    ] {
        let value = tampered(|row| row["projection"][key] = value.clone());
        let error = both_refuse(&value);
        assert!(
            error.contains("unknown field"),
            "a projection field outside the whitelist is not a projection: {error}"
        );
    }
    let body_with_a_path = tampered(|row| {
        row["body"]["path"] = json!("/synthetic-root/sessions/rollout-2026-10-09.jsonl")
    });
    both_refuse(&body_with_a_path);
}

#[test]
fn identity_hashes_and_ids_are_validated_under_both_laws() {
    let short_hash = tampered(|row| row["body"]["body_sha256"] = json!("9f8e7d6c"));
    both_refuse(&short_hash);

    let upper_hash = tampered(|row| row["body"]["body_sha256"] = json!(BODY_SHA.to_uppercase()));
    both_refuse(&upper_hash);

    let short_id = tampered(|row| row["observation_id"] = json!("0e3a7f2c"));
    both_refuse(&short_id);

    let stamped = row().observation_id.to_uppercase();
    let upper_id = tampered(|row| row["observation_id"] = json!(stamped));
    both_refuse(&upper_id);

    let truncated_agent = tampered(|row| row["agent_id"] = json!("0e3a7f2c"));
    both_refuse(&truncated_agent);

    let upper_agent = tampered(|row| row["agent_id"] = json!(AGENT.to_uppercase()));
    both_refuse(&upper_agent);

    let parent_not_uuid = tampered(|row| row["projection"]["parent_agent_id"] = json!("7c9d0e5b"));
    both_refuse(&parent_not_uuid);

    let empty_provider = tampered(|row| row["provider"] = json!(""));
    both_refuse(&empty_provider);

    let spaced_provider = tampered(|row| row["provider"] = json!("Codex CLI"));
    both_refuse(&spaced_provider);

    let spaced_daemon = tampered(|row| row["daemon_key"] = json!("srv 9k2m"));
    both_refuse(&spaced_daemon);

    // A native id keeps its harness's spelling, but it is still an id and not a
    // path: the shape that would locate a file is refused.
    let native_as_path =
        tampered(|row| row["native_session_id"] = json!("/synthetic-root/sessions/x.jsonl"));
    both_refuse(&native_as_path);

    let canonical_with_two_parts =
        tampered(|row| row["body"]["canonical_session_id"] = json!("codex.synthetic-machine"));
    let error = refused(&canonical_with_two_parts);
    assert!(
        error.contains("three dot-separated"),
        "the rejection must name the identity shape: {error}"
    );
    // The schema can only say "these characters, up to 255 of them"; that
    // `<harness>.<machine>.<native-id>` needs three components is the code's.
    schema_accepted(&canonical_with_two_parts);

    let empty_native = tampered(|row| row["native_session_id"] = json!(""));
    both_refuse(&empty_native);

    let missing_schema_key = tampered(|row| {
        let _ = row.as_object_mut().map(|map| map.remove("schema"));
    });
    both_refuse(&missing_schema_key);
}

#[test]
fn shard_sequences_are_an_exact_ordered_set() {
    let empty = tampered(|row| row["body"]["shard_sequences"] = json!([]));
    both_refuse(&empty);

    let zero = tampered(|row| row["body"]["shard_sequences"] = json!([0, 1, 2]));
    both_refuse(&zero);

    let repeated = tampered(|row| row["body"]["shard_sequences"] = json!([1, 1, 2]));
    both_refuse(&repeated);

    // Reordered: still unique, still above zero, so the schema cannot see it. The
    // code refuses it, because the list *is* the body's definition — a second
    // order of one shard set would otherwise be a second body identity. The
    // divergence is asserted rather than hidden.
    let reordered = tampered(|row| row["body"]["shard_sequences"] = json!([2, 1, 7]));
    let error = refused(&reordered);
    assert!(
        error.contains("strictly increasing"),
        "the rejection must name the ordering rule: {error}"
    );
    schema_accepted(&reordered);
}

#[test]
fn title_and_reason_are_bounded_and_privacy_safe() {
    let empty_title = tampered(|row| row["projection"]["title"] = json!(""));
    both_refuse(&empty_title);

    let long_title = tampered(|row| row["projection"]["title"] = json!("t".repeat(101)));
    both_refuse(&long_title);

    // 100 is 100 characters, not 100 bytes: a label in any script gets the same
    // allowance. This row has to be *stamped* rather than tampered with, because
    // a valid-looking edit of a stored row is still a row whose content no longer
    // hashes to its own id — see `a_stored_row_cannot_be_rewritten_in_place`.
    let exactly_a_hundred = AgentObservationRow::new(
        AgentObservation {
            projection: AgentProjection {
                title: Some("标".repeat(100)),
                ..full_projection()
            },
            ..observation()
        },
        FIRST_SEEN,
    );
    exactly_a_hundred
        .validate()
        .expect("a 100-character label is within the cap");
    schema_accepted(&serde_json::to_value(&exactly_a_hundred).expect("plain data tree"));

    for (state, bad) in [
        ("unreadable", "/synthetic-root/agents/0e3a.json"),
        ("unsupported", "\\synthetic-root\\agents\\0e3a.json"),
        ("ambiguous", "~/.synthetic-host/agents"),
    ] {
        let row = tampered(|row| {
            row["body"] = json!({"state": state, "reason": format!("cannot tell which: {bad}")})
        });
        let error = both_refuse(&row);
        assert!(
            !error.contains("cannot tell which"),
            "the reason text is not repeated back: {error}"
        );
    }

    let empty_reason = tampered(|row| row["body"] = json!({"state": "missing", "reason": ""}));
    both_refuse(&empty_reason);

    let long_reason =
        tampered(|row| row["body"] = json!({"state": "ambiguous", "reason": "x".repeat(201)}));
    both_refuse(&long_reason);

    // A reason that stays inside the rules still says something.
    let honest = AgentObservationRow::new(
        AgentObservation {
            body: BodyState::Ambiguous {
                reason: "two candidate rollout files".to_string(),
            },
            ..observation()
        },
        FIRST_SEEN,
    );
    honest.validate().expect("a bounded reason is valid");
    schema_accepted(&serde_json::to_value(&honest).expect("plain data tree"));
}

#[test]
fn a_stored_row_cannot_be_rewritten_in_place() {
    // The property the tamper helpers keep tripping over, stated where it
    // belongs: every field a stored row *asserts something about the world* is
    // inside its identity, so replacing one with something perfectly valid on its
    // own still leaves a row that does not hash to itself. In an append-only file
    // that is the difference between "the source changed" and "somebody edited
    // history".
    let original = row();
    for field in ["provider", "daemon_key", "native_session_id", "agent_id"] {
        let mut edited = serde_json::to_value(&original).expect("plain data tree");
        edited[field] = match field {
            "native_session_id" => json!("rollout-2026-10-10-00-00-00-11111111"),
            "daemon_key" => json!("srv000000000"),
            "agent_id" => json!(PARENT),
            _ => json!("grok"),
        };
        let error = refused(&edited);
        assert!(
            error.contains("does not match"),
            "{field} is inside the identity, so the edit must be caught by the digest: {error}"
        );
    }

    // observed_at is the one field outside the identity, and that is the contract
    // rather than a hole in it: re-seeing an unchanged agent is not a new fact
    // (ADR-056 D5), so the moment of seeing must not mint a row. The cost is
    // stated here so nobody reads the digest as a signature over the whole line:
    // it covers what was recorded, not when it was looked at.
    let mut restamped = serde_json::to_value(&original).expect("plain data tree");
    restamped["observed_at"] = json!(SEEN_AGAIN);
    let parsed = parse(&restamped).expect("observed_at sits outside the identity");
    assert_eq!(parsed[0].observation_id, original.observation_id);
    assert_eq!(parsed[0].observed_at, SEEN_AGAIN);
    assert_eq!(parsed[0].semantic_sha256(), parsed[0].observation_id);

    // And the untouched original still stands, which is the point of refusing the
    // edit rather than repairing it.
    original.validate().expect("the original row is untouched");
}

#[test]
fn the_five_body_states_stay_five() {
    let states = [
        (row().body.clone(), "archived"),
        (BodyState::Missing, "missing"),
        (
            BodyState::Unreadable {
                reason: "the rollout file could not be read".to_string(),
            },
            "unreadable",
        ),
        (
            BodyState::Unsupported {
                reason: "a provider format this build does not read".to_string(),
            },
            "unsupported",
        ),
        (
            BodyState::Ambiguous {
                reason: "two candidate rollout files".to_string(),
            },
            "ambiguous",
        ),
    ];
    let mut tags = Vec::new();
    for (state, tag) in &states {
        assert_eq!(state.tag(), *tag);
        tags.push(state.tag());
        let encoded = serde_json::to_value(state).expect("plain data tree");
        assert_eq!(encoded["state"], json!(*tag));
        BodyState::validate(state).unwrap_or_else(|error| panic!("{tag} is invalid: {error}"));
        match *tag {
            "archived" => assert!(state.is_archived()),
            // Not knowing is not the same as an absent body, and the payload each
            // state is allowed to carry is what keeps them from blurring: `missing`
            // pays for nothing, and each unknown state must say which kind of
            // not-knowing it is.
            "missing" => {
                assert!(!state.is_archived());
                assert_eq!(
                    encoded
                        .as_object()
                        .expect("an object")
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    vec!["state"],
                    "missing carries no payload"
                );
            }
            _ => {
                assert!(!state.is_archived());
                assert!(
                    encoded["reason"].is_string(),
                    "{tag} must say which kind of not-knowing it is"
                );
            }
        }
    }
    tags.sort();
    tags.dedup();
    assert_eq!(
        tags.len(),
        5,
        "five states, five tags, no two interchangeable"
    );
}

#[test]
fn a_body_state_never_pays_for_a_payload_it_does_not_claim() {
    let missing_with_reason = tampered(|row| {
        row["body"] = json!({"state": "missing", "reason": "looked and found nothing"})
    });
    both_refuse(&missing_with_reason);

    let unknown_state = tampered(|row| row["body"] = json!({"state": "probably-there"}));
    both_refuse(&unknown_state);

    let unreadable_without_reason = tampered(|row| row["body"] = json!({"state": "unreadable"}));
    both_refuse(&unreadable_without_reason);

    let archived_without_shards = tampered(|row| {
        let _ = row
            .as_object_mut()
            .and_then(|row| row.get_mut("body"))
            .and_then(Value::as_object_mut)
            .map(|body| body.remove("shard_sequences"));
    });
    both_refuse(&archived_without_shards);
}

#[test]
fn where_the_schema_is_weaker_the_divergence_is_named() {
    // Three rules no JSON Schema can state, and one this build deliberately
    // leaves declarative: recorded as tests so nobody reads the schema as the
    // whole contract.
    let foreign_identity = tampered(|row| row["observation_id"] = json!("f".repeat(64)));
    let error = refused(&foreign_identity);
    assert!(
        error.contains("does not match"),
        "the identity must be bound to the content: {error}"
    );
    schema_accepted(&foreign_identity);

    let not_a_timestamp = tampered(|row| row["observed_at"] = json!("yesterday"));
    let error = refused(&not_a_timestamp);
    assert!(
        error.contains("observed_at"),
        "observed_at must be an instant: {error}"
    );
    schema_accepted(&not_a_timestamp);

    let not_a_lifecycle_instant =
        tampered(|row| row["projection"]["last_activity_at"] = json!("sometime Tuesday"));
    refused(&not_a_lifecycle_instant);
    schema_accepted(&not_a_lifecycle_instant);

    let other_contract =
        tampered(|row| row["schema"] = json!("chat-stasher/paseo-agent-observation@2"));
    refused(&other_contract);
    schema_refused(&other_contract);
}

#[test]
fn the_published_schema_and_the_code_name_one_contract() {
    assert_eq!(SCHEMA, "chat-stasher/paseo-agent-observation@1");
    let schema = &*SCHEMA_VALUE;
    assert_eq!(schema["properties"]["schema"]["const"], json!(SCHEMA));

    let mut fields: Vec<&str> = schema["properties"]
        .as_object()
        .expect("the schema declares its properties as an object")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec![
            "agent_id",
            "body",
            "daemon_key",
            "native_session_id",
            "observation_id",
            "observed_at",
            "projection",
            "provider",
            "schema",
        ],
        "the row has exactly these fields; anything more is a new contract, not a new field"
    );

    let mut projection: Vec<&str> = schema["properties"]["projection"]["properties"]
        .as_object()
        .expect("the projection whitelist is an object")
        .keys()
        .map(String::as_str)
        .collect();
    projection.sort_unstable();
    assert_eq!(
        projection,
        vec![
            "archived_at",
            "created_at",
            "last_activity_at",
            "parent_agent_id",
            "status",
            "title",
            "updated_at",
            "workspace_id",
        ],
        "the projection whitelist is exactly the fields ADR-056 D5 lists"
    );
    assert_eq!(
        schema["properties"]["projection"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(schema["additionalProperties"], json!(false));
}
