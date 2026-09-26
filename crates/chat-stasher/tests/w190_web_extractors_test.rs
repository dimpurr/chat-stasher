//! W190 — UIA-6: the reader's web-platform extractors, with known answers.
//!
//! Three archived body shapes, one minimal fixture each under
//! `tests/fixtures/web-bodies/`, and the canonical-thread outcome of each
//! asserted against a number written down in advance:
//!
//! * **chatgpt** — `mapping` + `current_node`: the active branch is the chain
//!   of `parent` links up from `current_node`, and every mapping node off that
//!   chain is counted as a side branch;
//! * **claude** — a flat `chat_messages` array that nonetheless carries a tree:
//!   the active branch is the chain of `parent_message_uuid` links up from
//!   `current_leaf_message_uuid`, and the messages off it are counted;
//! * **a web body whose top level is a JSON array** — read by the generic
//!   reader, which counts every record it cannot turn into a message.
//!
//! The `/reader` route is then driven in-process over the same fixture, so the
//! work is proven where a reader sees it: the page names the branch it followed
//! and does not show the abandoned one.
//!
//! The label half of the row is a different question and a different tier: a web
//! platform's own archived body carries the conversation's label
//! (`tests/.../chatgpt-detail.json` `title`, `claude-detail.json` `name`), and
//! `activity-index` has to record it instead of the by-design
//! `no_label_recorded` a harness with no extractor gets. That runs the real
//! binary against a synthetic stage. The label is read from the same body the
//! time pass reads this session's own metadata from, so a **one-row** list body
//! labels from its row while a page of several conversations labels nothing —
//! that boundary is asserted here on both platforms.
//!
//! Everything here is synthetic and shape-only: no real conversation, title,
//! session id, account or machine appears, and no request leaves the machine
//! (the extractor tests never touch the network, and the index tests run the
//! real `activity-index` over a temporary stage). The body shapes come from the
//! capture contract's own rows — `apps/extension/lib/contract.ts:434-460`
//! (chatgpt: `requiredPaths: ['mapping','current_node']`) and
//! `apps/extension/lib/contract.ts:498-560` (claude: `requiredPaths:
//! ['chat_messages']`) — plus the measured claude envelope recorded by the
//! capture-side test `apps/extension/tests/w31-claude.test.ts:161-203`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use chat_stasher::activity::TimeSource;
use chat_stasher::normalize::{normalize, Block, Conversation};
use chat_stasher::search::SessionLabel;
use chat_stasher::ui::{
    handle, split_target, Content, ContentSource, DestinationState, NoIndex, UiData, UiSession,
};

// --------------------------------------------------------------- fixtures

/// The ChatGPT detail body: four nodes on the active branch (`n1`…`n4`) and one
/// abandoned sibling (`side`, re-answered from `n2`).
const CHATGPT_DETAIL: &str = include_str!("fixtures/web-bodies/chatgpt-detail.json");

/// The claude.ai detail body: the leaf is `c4`, its chain runs `c1`…`c4`, and
/// `c3b` is the abandoned sibling of `c3`.
const CLAUDE_DETAIL: &str = include_str!("fixtures/web-bodies/claude-detail.json");

/// A web body whose top level is an array: four message records and one record
/// that carries no role at all.
const GENERIC_ARRAY: &str = include_str!("fixtures/web-bodies/web-generic-array.json");

/// The known answers, written down before the extractors were changed.
const CHATGPT_MESSAGES: usize = 4;
const CHATGPT_BRANCH_NODES: usize = 1;
const CLAUDE_MESSAGES: usize = 4;
const CLAUDE_BRANCH_NODES: usize = 1;
const GENERIC_MESSAGES: usize = 4;
const GENERIC_UNRENDERED: usize = 1;

/// One inbox bundle line, exactly as the archive writes one: the platform's own
/// body as a string under `raw.text`. The reader is handed the shard body, so a
/// fixture body reaches the extractor only through this envelope — which is why
/// the tests build it rather than calling the platform parsers directly.
fn bundle_line(platform: &str, session: &str, body: &str) -> String {
    serde_json::json!({
        "schema": "chat-stasher/inbox@2",
        "platform": platform,
        "sessionId": session,
        "raw": { "text": body, "bytes": body.len() },
    })
    .to_string()
}

/// The text of every rendered block, so an assertion can say both what is there
/// and what must not be.
fn rendered(conversation: &Conversation) -> String {
    conversation
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .map(|block| match block {
            Block::Text(text) => text.clone(),
            Block::CodeBlock { code, .. } => code.clone(),
            Block::Thinking(text) => text.clone(),
            Block::ToolCall { input_summary, .. } => input_summary.clone(),
            Block::AttachmentRef(attachment) => attachment.name.clone().unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------- ChatGPT: mapping/current_node

#[test]
fn chatgpt_follows_the_active_branch_and_counts_the_side_branch() {
    let body = bundle_line("chatgpt", "w190chatgpt0001", CHATGPT_DETAIL);
    let conversation = normalize("chatgpt", &body);

    assert_eq!(
        conversation.messages.len(),
        CHATGPT_MESSAGES,
        "the active branch is n1..n4; the `message: null` root carries no message: {}",
        rendered(&conversation)
    );
    assert_eq!(
        conversation.branch_nodes, CHATGPT_BRANCH_NODES,
        "the abandoned `side` node is the one node off the active chain"
    );
    assert!(
        conversation.canonical_follows_active,
        "current_node resolved and walked to the root, so the branch provenance is known"
    );
    assert_eq!(conversation.unrendered_lines, 0);

    let text = rendered(&conversation);
    assert!(text.contains("Synthetic question one"));
    assert!(text.contains("Synthetic answer two"));
    assert!(
        !text.contains("Synthetic abandoned answer"),
        "an abandoned branch is not part of the conversation: {text}"
    );
}

// --------------------------------------------------- claude: leaf → parents

#[test]
fn claude_follows_the_active_branch_and_counts_the_side_branch() {
    let body = bundle_line("claude", "w190claude0001", CLAUDE_DETAIL);
    let conversation = normalize("claude", &body);

    assert_eq!(
        conversation.messages.len(),
        CLAUDE_MESSAGES,
        "the leaf chain c1..c4, not the five messages the body carries: {}",
        rendered(&conversation)
    );
    assert_eq!(
        conversation.branch_nodes, CLAUDE_BRANCH_NODES,
        "c3b is the one message off the active chain"
    );
    assert!(conversation.canonical_follows_active);
    assert_eq!(conversation.unrendered_lines, 0);

    let text = rendered(&conversation);
    assert!(
        !text.contains("Synthetic abandoned answer"),
        "an abandoned branch is not part of the conversation: {text}"
    );
}

#[test]
fn a_claude_message_whose_first_parent_spelling_is_empty_still_finds_its_link() {
    // The two parent spellings are accepted by the capture side as "the first
    // one that is a non-empty string" (`claudeParentKeyOf`), so a message that
    // carries `parent_message_uuid: ""` *and* a real `parent_uuid` was accepted
    // for capture and has to walk here too. Reading the empty spelling as the
    // answer hides the middle link of z1→z2→z3: the walk then stops at z2, z1
    // drops off the active branch and is counted as a side branch instead.
    let body = serde_json::json!({
        "uuid": "55555555-5555-4555-8555-555555555555",
        "name": "synthetic",
        "current_leaf_message_uuid": "z3",
        "chat_messages": [
            {"uuid": "z1", "sender": "human", "parent_message_uuid": "00000000-0000-0000-0000-000000000001",
             "content": [{"type": "text", "text": "Synthetic one"}]},
            {"uuid": "z2", "sender": "assistant", "parent_message_uuid": "", "parent_uuid": "z1",
             "content": [{"type": "text", "text": "Synthetic two"}]},
            {"uuid": "z3", "sender": "human", "parent_message_uuid": "z2",
             "content": [{"type": "text", "text": "Synthetic three"}]}
        ]
    })
    .to_string();
    let conversation = normalize("claude", &bundle_line("claude", "w190claude0005", &body));

    assert_eq!(
        conversation.messages.len(),
        3,
        "the whole chain z1..z3 is the conversation: {}",
        rendered(&conversation)
    );
    assert_eq!(
        conversation.branch_nodes, 0,
        "no message is off the active chain"
    );
    assert!(conversation.canonical_follows_active);
}

#[test]
fn a_claude_body_without_parent_links_is_the_whole_conversation() {
    // Some response shapes are flat and expose no parent links at all. There is
    // then no tree to follow and nothing hidden, so every message is the
    // conversation and no branch is claimed.
    let body = serde_json::json!({
        "uuid": "22222222-2222-4222-8222-222222222222",
        "name": "synthetic",
        "current_leaf_message_uuid": null,
        "chat_messages": [
            {"uuid": "f1", "sender": "human", "content": [{"type": "text", "text": "Synthetic one"}]},
            {"uuid": "f2", "sender": "assistant", "content": [{"type": "text", "text": "Synthetic two"}]}
        ]
    })
    .to_string();
    let conversation = normalize("claude", &bundle_line("claude", "w190claude0002", &body));

    assert_eq!(conversation.messages.len(), 2);
    assert_eq!(conversation.branch_nodes, 0);
    assert!(conversation.canonical_follows_active);
}

#[test]
fn a_claude_body_without_a_walkable_leaf_is_not_claimed_to_be_the_active_branch() {
    // The same tree, with the leaf name dropped: the body carries parent links
    // but nothing says which of its branches is current. The messages are still
    // shown, and the page says the branch provenance is unknown rather than
    // presenting whichever branch came first as the conversation.
    let mut body: serde_json::Value = serde_json::from_str(CLAUDE_DETAIL).unwrap();
    body.as_object_mut()
        .unwrap()
        .remove("current_leaf_message_uuid");
    let conversation = normalize(
        "claude",
        &bundle_line("claude", "w190claude0003", &body.to_string()),
    );

    assert_eq!(conversation.messages.len(), 5);
    assert!(
        !conversation.canonical_follows_active,
        "a body whose leaf cannot be walked cannot be claimed to be the active branch"
    );
}

#[test]
fn a_claude_leaf_cycle_is_not_claimed_to_be_the_active_branch() {
    // Two messages that name each other as parent: the walk from the leaf never
    // reaches a root. Reported as unknown, not as a conversation that is
    // provably whole.
    let body = serde_json::json!({
        "uuid": "33333333-3333-4333-8333-333333333333",
        "name": "synthetic",
        "current_leaf_message_uuid": "y1",
        "chat_messages": [
            {"uuid": "y1", "sender": "human", "parent_message_uuid": "y2",
             "content": [{"type": "text", "text": "Synthetic one"}]},
            {"uuid": "y2", "sender": "assistant", "parent_message_uuid": "y1",
             "content": [{"type": "text", "text": "Synthetic two"}]}
        ]
    })
    .to_string();
    let conversation = normalize("claude", &bundle_line("claude", "w190claude0004", &body));

    assert_eq!(conversation.messages.len(), 2, "still shown, both of them");
    assert!(!conversation.canonical_follows_active);
}

// --------------------------------------------------- generic: a top-level array

#[test]
fn a_web_body_whose_top_level_is_an_array_is_read_by_the_generic_reader() {
    // A harness with no extractor of its own, in the array shape the design's
    // generic reader is the fallback for. One record in it carries no role: it
    // is counted, never dropped silently and never rendered as an empty
    // message.
    let body = bundle_line("kimi", "w190generic0001", GENERIC_ARRAY);
    let conversation = normalize("kimi", &body);

    assert_eq!(conversation.messages.len(), GENERIC_MESSAGES);
    assert_eq!(
        conversation.unrendered_lines, GENERIC_UNRENDERED,
        "the record with no role is one unreadable line, not a message"
    );
    assert_eq!(conversation.unrecognized_lines, 0, "the body is valid JSON");
}

// --------------------------------------------------- the route a reader sees

/// One session, served from one in-memory body — the shape `/reader` needs and
/// nothing else.
struct Body(&'static str, &'static str);

impl ContentSource for Body {
    fn fetch(&self, _machine: &str, _session_id: &str) -> Result<Content, String> {
        Ok(Content {
            shards: Vec::new(),
            concat_sha256: "aa".repeat(32),
            bytes: self.0.len(),
            body: bundle_line(self.1, "w190reader0001", self.0),
        })
    }
}

fn data_for(harness: &str) -> UiData {
    UiData {
        destination_label: "w190-destination".into(),
        // One destination, read in full: the shape every page in this file is
        // about. The merged fields carry their single-destination values, which
        // is what makes `destinations` here a list of one rather than an empty
        // one — a row always came from somewhere.
        destinations: vec![DestinationState {
            label: "w190-destination".into(),
            snapshots_scanned: 1,
            snapshots_in_repo: 1,
            sessions: 1,
            in_view: 1,
            unreadable: Vec::new(),
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
        }],
        snapshots_scanned: 1,
        snapshots_in_repo: 1,
        sessions_seen: 1,
        archive_sessions: 1,
        raw_sessions: 1,
        sessions: vec![UiSession {
            index: 0,
            machine: "m-1".into(),
            harness: Some(harness.into()),
            session_id: "w190reader0001".into(),
            short_id: "w190reader0".into(),
            shard_count: 1,
            bytes: 64,
            first_unix: Some(1_736_944_400),
            last_unix: Some(1_736_944_600),
            time_why: None,
            time_source: TimeSource::Exact,
            title: SessionLabel::NoLabelRecorded,
            provenance: None,
            line_count: 1,
            archive_time_unix: 1_770_000_000,
            data_blobs: 1,
            destinations: vec![0],
        }],
        launch: Default::default(),
        hosts: Vec::new(),
        machines_without_index: Vec::new(),
        machines_with_legacy_index: Vec::new(),
        unreadable: Vec::new(),
        data_blobs_read: 1,
        index_files_read: 0,
        now_unix: 1_770_000_000,
    }
}

#[test]
fn the_reader_page_names_the_branch_it_followed() {
    let (path, params) = split_target("/reader?i=0&token=t");
    let response = handle(
        path,
        &params,
        "t",
        &data_for("claude"),
        &Body(CLAUDE_DETAIL, "claude"),
        // `/reader` is not a search route: the text index is not reachable
        // from here, and `NoIndex` is the implementation that says so.
        &NoIndex,
    )
    .expect("`/reader` is a route");
    assert_eq!(response.status, 200, "{}", response.body);

    assert!(
        response
            .body
            .contains("Following the active branch. 1 branch node(s) are not shown."),
        "the page says which branch it followed and how many nodes are off it: {}",
        response.body
    );
    assert!(
        !response.body.contains("Synthetic abandoned answer"),
        "the abandoned branch must not be rendered as part of the conversation"
    );
    assert!(
        !response.body.contains("Branch provenance is unknown"),
        "a resolved branch is not an unknown one"
    );
}

#[test]
fn the_reader_page_does_not_claim_a_branch_it_could_not_walk() {
    let mut body: serde_json::Value = serde_json::from_str(CLAUDE_DETAIL).unwrap();
    body.as_object_mut()
        .unwrap()
        .remove("current_leaf_message_uuid");
    let leaked: &'static str = Box::leak(body.to_string().into_boxed_str());

    let (path, params) = split_target("/reader?i=0&token=t");
    let response = handle(
        path,
        &params,
        "t",
        &data_for("claude"),
        &Body(leaked, "claude"),
        &NoIndex,
    )
    .expect("`/reader` is a route");

    assert!(
        response.body.contains("Branch provenance is unknown"),
        "an unwalkable branch is stated as unknown: {}",
        response.body
    );
}

// --------------------------------------------------------------- the label

/// Run the CLI with a sandboxed HOME/XDG, as `tests/w156_label_test.rs` does.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W190 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// The recorded label rows, keyed by session id.
fn labels_by_session(stage: &Path, machine: &str) -> Vec<(String, serde_json::Value)> {
    let path = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let index = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    index
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            (
                row["session_id"].as_str().unwrap().to_string(),
                row["title"].clone(),
            )
        })
        .collect()
}

const MACHINE: &str = "mbp-w190";
const CHATGPT_TITLED: &str = "chatgpt.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d001";
const CLAUDE_TITLED: &str = "claude.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d002";
const CHATGPT_UNTITLED: &str = "chatgpt.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d003";
const CLAUDE_LIST_ROW: &str = "claude.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d004";
const CHATGPT_LONG_TITLE: &str = "chatgpt.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d005";
const CLAUDE_LIST_PAGE: &str = "claude.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d006";
const CHATGPT_LIST_PAGE: &str = "chatgpt.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d007";
const CHATGPT_LIST_ROW: &str = "chatgpt.mbp-w190.019bf00d-97b6-7eb2-9bf8-eacbacc0d008";

const CHATGPT_LABEL: &str = "Synthetic chatgpt conversation";
const CLAUDE_LABEL: &str = "Synthetic claude conversation";
const CLAUDE_LIST_ROW_LABEL: &str = "Synthetic claude list row";
const LONG_LABEL: &str = "synthetic label long enough to be cut by the hundred character cap, written out \
                           so the cut has an honest body to take and the flag has something to report";

#[test]
fn a_web_body_labels_its_session_from_its_own_metadata() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let stage = sandbox.path().join("stage");

    // The two titled sessions are the fixtures themselves; the others are the
    // shapes that must stay unlabelled or capped.
    let untitled = {
        let mut body: serde_json::Value = serde_json::from_str(CHATGPT_DETAIL).unwrap();
        body.as_object_mut().unwrap().remove("title");
        body.to_string()
    };
    let long = {
        let mut body: serde_json::Value = serde_json::from_str(CHATGPT_DETAIL).unwrap();
        body["title"] =
            serde_json::Value::String(LONG_LABEL.split_whitespace().collect::<Vec<_>>().join(" "));
        body.to_string()
    };
    // A ONE-ROW list body: the conversation's own metadata record, the shape the
    // time pass reads its `ListUpdated` span from (`claude_span`'s one-item page,
    // activity.rs). The label and that time are one reading of one body, so this
    // labels — it is the row's own `name`, not a sidebar entry's.
    let list_row = r#"[{"uuid":"44444444-4444-4444-8444-444444444444","name":"Synthetic claude list row","created_at":"2025-01-15T12:30:00.000Z","updated_at":"2025-01-15T13:00:00.000Z"}]"#;
    // A page holding SEVERAL conversations is nobody's own record. Whatever its
    // rows are called, none of their names is this session's label: the page is
    // the failure the single-row rule exists to refuse.
    let list_page = r#"[{"uuid":"44444444-4444-4444-8444-444444444444","name":"Synthetic first sidebar entry","created_at":"2025-01-15T12:30:00.000Z","updated_at":"2025-01-15T13:00:00.000Z"},{"uuid":"55555555-5555-4555-8555-555555555555","name":"Synthetic second sidebar entry","created_at":"2025-01-15T12:40:00.000Z","updated_at":"2025-01-15T13:10:00.000Z"}]"#;
    // ChatGPT's measured list route answers a page under `items` (`total`/
    // `limit`/`offset` beside it), and a list row's times are ISO strings —
    // this reader reads epochs, so such a body yields no time at all. A row of
    // that page, lifted out as if it were the whole body, still carries a
    // `title`: it labels nothing either, because the label is read only from the
    // body that gives this session its time, and this body gives it none.
    let chatgpt_page = r#"{"items":[{"id":"66666666-6666-4666-8666-666666666666","title":"Synthetic sidebar entry","create_time":"2025-01-15T12:30:00.000Z","update_time":"2025-01-15T13:00:00.000Z"}],"total":1,"limit":28,"offset":0}"#;
    let chatgpt_list_row_with_iso_times = r#"{"id":"66666666-6666-4666-8666-666666666666","title":"Synthetic chatgpt list row","create_time":"2025-01-15T12:30:00.000Z","update_time":"2025-01-15T13:00:00.000Z"}"#;

    for (session, platform, body) in [
        (CHATGPT_TITLED, "chatgpt", CHATGPT_DETAIL.to_string()),
        (CLAUDE_TITLED, "claude", CLAUDE_DETAIL.to_string()),
        (CHATGPT_UNTITLED, "chatgpt", untitled),
        (CLAUDE_LIST_ROW, "claude", list_row.to_string()),
        (CHATGPT_LONG_TITLE, "chatgpt", long),
        (CLAUDE_LIST_PAGE, "claude", list_page.to_string()),
        (CHATGPT_LIST_PAGE, "chatgpt", chatgpt_page.to_string()),
        (
            CHATGPT_LIST_ROW,
            "chatgpt",
            chatgpt_list_row_with_iso_times.to_string(),
        ),
    ] {
        write_shard(
            &stage,
            MACHINE,
            session,
            &[bundle_line(platform, session, &body)],
        );
    }

    let out = run(
        sandbox.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    assert!(out.status.success(), "activity-index: {out:?}");

    let rows = labels_by_session(&stage, MACHINE);
    let label = |session: &str| {
        rows.iter()
            .find(|(id, _)| id == session)
            .unwrap_or_else(|| panic!("no row for {session}"))
            .1
            .clone()
    };

    for (session, expected) in [
        (CHATGPT_TITLED, CHATGPT_LABEL),
        (CLAUDE_TITLED, CLAUDE_LABEL),
        (CLAUDE_LIST_ROW, CLAUDE_LIST_ROW_LABEL),
    ] {
        let row = label(session);
        assert_eq!(row["state"], "known", "{session}: {row}");
        assert_eq!(row["text"], expected, "{session}: {row}");
        assert_eq!(
            row["source"], "harness_title",
            "the platform's own label is the harness's own title, never a quoted first line: {row}"
        );
        assert_eq!(row["truncated"], false, "{session}: {row}");
    }

    // A body that carries no label field records no label. It is not an empty
    // string and not the first line of the conversation.
    let row = label(CHATGPT_UNTITLED);
    assert_eq!(row["state"], "no_label_recorded", "{row}");

    // A page of several conversations is not this conversation's own record, on
    // either platform: no entry's name is lent to whatever session the page was
    // filed under. A chatgpt list row lifted out of its page is refused too —
    // its ISO-string times are not a time this reader can read, so it is not the
    // body this session's `ListUpdated` row came from either.
    for session in [CLAUDE_LIST_PAGE, CHATGPT_LIST_PAGE, CHATGPT_LIST_ROW] {
        let row = label(session);
        assert_eq!(row["state"], "no_label_recorded", "{session}: {row}");
    }

    // A label over the cap is stored cut, and says so.
    let row = label(CHATGPT_LONG_TITLE);
    assert_eq!(row["state"], "known", "{row}");
    assert_eq!(row["truncated"], true, "{row}");
    assert_eq!(
        row["text"].as_str().unwrap().chars().count(),
        100,
        "the cap is characters, not bytes: {row}"
    );
}
