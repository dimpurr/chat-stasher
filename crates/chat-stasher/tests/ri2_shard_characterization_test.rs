//! Synthetic characterization of the stage-local seal's sequence return value.
use chat_stasher::{seal, store};
use std::fs;

#[test]
fn seal_returns_allocated_sequence_without_reparsing_the_filename() {
    let stage = tempfile::tempdir().unwrap();
    let dir = store::session_shard_dir(stage.path(), "synthetic-machine", "synthetic-session");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(store::SHARD_SEQ_FILE), b"999999").unwrap();
    let active = stage.path().join("active.jsonl");
    let raw = b"synthetic\r\n\xffunterminated";
    fs::write(&active, raw).unwrap();
    assert_eq!(
        seal::seal_active_file(
            &active,
            stage.path(),
            "synthetic-machine",
            "synthetic-session",
            1
        )
        .unwrap(),
        1_000_000
    );
    assert!(!active.exists());
    let shard = store::shard_path_with_cap(
        stage.path(),
        "synthetic-machine",
        "synthetic-session",
        1_000_000,
        1,
    );
    assert!(fs::read(shard).unwrap() == raw);
}
