#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::config::Config;
use chat_stasher::scanner;
use std::collections::BTreeMap;
use std::fs;

const AGENT: &str = "11111111-2222-4333-8444-555555555555";
const ACCOUNT: &str = "grok%7Cuser_synthetic-0000";

fn blob_name(leaf: &str) -> String {
    format!(
        "{}.blob",
        test_support::grok_bot_blob_name(&format!("sand.client.slice.account.{ACCOUNT}.{leaf}"))
    )
}

fn wrapped(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "value": value
    }))
    .unwrap()
}

#[test]
fn scanner_discovers_per_agent_replica_and_marks_it_partial() {
    let sandbox = test_support::Sandbox::new();
    let app_support = sandbox.root().join("Grok Bot");
    let persistence = app_support.join("profile").join("sand-client-persistence");
    fs::create_dir_all(&persistence).unwrap();
    fs::write(
        persistence.join(".migrated-from-local-storage"),
        b"migrated-sentinel-24byte",
    )
    .unwrap();
    // Entry key names from W321: id/kind/message/seq/timestampMs, with the
    // W319b message keys (role/content) nested inside. Sequences start at 2,
    // so the report also has to count the head gap at 1.
    fs::write(
        persistence.join(blob_name(&format!("transcript.replicas.{AGENT}"))),
        wrapped(serde_json::json!({
            "acceptedSequenceHint": 0,
            "entries": [
                {
                    "id": "synthetic-2",
                    "kind": "message",
                    "message": {"content": "synthetic-two", "role": "user"},
                    "seq": 2,
                    "timestampMs": 1780000000000u64
                },
                {
                    "id": "synthetic-4",
                    "kind": "message",
                    "message": {"content": "synthetic-four", "role": "user"},
                    "seq": 4,
                    "timestampMs": 1780000000200u64
                },
            ],
            "epochHint": "synthetic-epoch",
            "persistedAt": 1780000000000u64
        })),
    )
    .unwrap();
    // A non-replica state key (W321 lists the selection key among the ten
    // state blobs); it must not produce an agent record.
    fs::write(
        persistence.join(blob_name("selection.last-agent")),
        wrapped(serde_json::json!({"agentId": AGENT})),
    )
    .unwrap();

    let mut harness_roots: BTreeMap<String, String> = scanner::load_registry_from_repo()
        .unwrap()
        .harnesses
        .into_iter()
        .map(|harness| {
            (
                harness.id,
                sandbox
                    .root()
                    .join("empty-source")
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect();
    harness_roots.insert(
        "grok-bot".to_string(),
        app_support.to_string_lossy().into_owned(),
    );
    let config = Config {
        harness_roots,
        ..Config::default()
    };
    let report = scanner::scan_with_machine(&config, "synthetic-machine").unwrap();
    if scanner::current_platform() == "macos" {
        assert_eq!(report.records.len(), 1);
        assert_eq!(
            report.records[0].id,
            format!("grok-bot.synthetic-machine.{AGENT}")
        );
        let probe = report
            .probes
            .iter()
            .find(|probe| probe.id == "grok-bot")
            .unwrap();
        assert_eq!(probe.record_count, Some(1));
        assert!(probe.note.contains("partial replica"));
        // Sequences 2 and 4 leave 1 and 3 missing, head position included.
        assert!(probe.note.contains("observed gaps=2"));
    } else {
        assert!(report.records.is_empty());
        let probe = report
            .probes
            .iter()
            .find(|probe| probe.id == "grok-bot")
            .unwrap();
        assert!(probe.note.contains("macOS only"));
        // Grok Bot has no cell for this platform and no app that could run
        // here, so the probe must land on B82's "no cell for this platform"
        // state — `not_applicable`, not "unknown" ("might exist, we did not
        // look"), and never inside the unlooked-harness count.
        assert_eq!(probe.state, scanner::ProbeState::SkipWrongPlatform);
        assert!(!probe.not_probed_p());
        let count = scanner::probe_session_count(probe);
        assert!(matches!(
            count,
            chat_stasher::json_out::CountState::NotApplicable { .. }
        ));
    }
}
