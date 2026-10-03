#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::config::Config;
use chat_stasher::scanner;
use std::collections::BTreeMap;
use std::fs;

const AGENT: &str = "11111111-2222-4333-8444-555555555555";

#[test]
fn scanner_discovers_per_agent_replica_and_marks_it_partial() {
    let sandbox = test_support::Sandbox::new();
    let app_support = sandbox.root().join("Grok Bot");
    let persistence = app_support.join("profile").join("sand-client-persistence");
    fs::create_dir_all(&persistence).unwrap();
    fs::write(
        persistence.join(format!("transcript.replicas.{AGENT}")),
        br#"[{"seq":1,"kind":"message","content":"synthetic-one"},{"seq":3,"kind":"message","content":"synthetic-three"}]"#,
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
        assert!(probe.note.contains("observed gaps=1"));
    } else {
        assert!(report.records.is_empty());
        let probe = report
            .probes
            .iter()
            .find(|probe| probe.id == "grok-bot")
            .unwrap();
        assert!(probe.note.contains("macOS only"));
    }
}
