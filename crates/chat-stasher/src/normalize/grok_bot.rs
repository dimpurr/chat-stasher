//! grok-bot — reader for one archived Grok Bot desktop replica session.
//!
//! The archive keeps one raw replica record per line — the observed windows
//! the app's `sand-client-persistence` blobs held, merged into a per-agent
//! union by `collect` (`grok_bot.rs`) — plus one `_chat_stasher` metadata
//! line per delivered shard that carries the union's own gap measurement.
//! Every record shape below was measured read-only on a local snapshot
//! (2026-10-10): a numeric `seq` numbered from one and a string `kind` on
//! every record, of seven kinds — `message`, `notice`, `send-message`,
//! `event`, `user-attachment`, `at`, `voice-call`.
//!
//! This file is the transcript slice of the approved GB-5 reader plan
//! (Slice A, §10; 2026-10-11). What renders here and why:
//!
//! * **Variants** — the app rewrites a row while streaming, and the
//!   append-only union keeps every observation, so one seq can arrive as
//!   several lines. One seq renders **one** message: the last variant marked
//!   complete (`isStreaming == false`), or the last variant when none is.
//!   Rejected: rendering every variant (the same words twice, twice the
//!   search hits), first-observed (freezes a streaming partial as the
//!   visible text), concatenation (fabricates text no observation carried).
//!   All variants stay archived; this only picks what is displayed.
//! * `message` — the human↔agent transcript: speaker from `role` with the
//!   channel-shape identity objects as fallbacks, text from `content` or
//!   (only when content is empty) `richText` — never both.
//! * `notice` — platform banners, `System`: a notice is banner-shaped, not
//!   speaker-ambiguous.
//! * `user-attachment` — the person's upload, so the role is `User` and the
//!   record's file name and byte size become an attachment reference.
//! * `event` — understood automation bookkeeping that holds no message of
//!   its own: counted unrendered, never dropped.
//! * `at` — a sync cursor ("N newer rows exist"), a record of nothing said:
//!   skipped with zero counts, the same known-empty treatment the chatgpt
//!   reader gives a null mapping node.
//! * `send-message` / `voice-call` — kinds whose per-field mapping is the
//!   next slice of the same plan (GB-5 §10 Slice B): each record is counted
//!   as understood-but-not-yet-rendered, never silently dropped.
//! * an unknown kind, a record missing `seq` or `kind`, or a line that does
//!   not parse — `unrecognized_lines`, and the rest of the session still
//!   renders (one bad line never kills a session, GB-5 §2).
//!
//! Gap measurement lives on the metadata line (GB-5 §7): `sequence_gaps` is
//! what `collect` measured — missing positions from one through the highest
//! observed seq, head gap included — and the reader compresses it to
//! inclusive runs and exposes them on
//! [`Conversation::sequence_gap_runs`] with the recorded `partial_replica`
//! on [`Conversation::partial`]. Runs are never recomputed from the rendered
//! seqs: variants share seqs, so the reader's own view proves nothing about
//! completeness. A body may concatenate several shards, so the last
//! metadata line wins.

use std::collections::BTreeMap;

use super::{message_time, AttachmentRef, Block, Conversation, Message, Role};
use serde_json::Value;

/// Read one archived grok-bot session body into `conversation`.
///
/// The reader owns the whole line walk (like `kimi-code`): which variant of
/// a seq is rendered is decided after the body is read, because a position
/// re-observed by the replica arrives on a later line, and no single line
/// knows whether it is the final observation.
pub(super) fn normalize_body(body: &str, conversation: &mut Conversation) {
    // Seq -> its variants, in observation order (the body's own line order).
    let mut records: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
    // The last `_chat_stasher` measurement line seen; each one replaces the
    // previous, so a body of concatenated shards keeps the newest union
    // state (GB-5 §7).
    let mut measurement = None;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            conversation.unrecognized_lines += 1;
            continue;
        };
        if record.get("_chat_stasher").is_some() {
            // The reader's own measurement line, one per delivered shard: not
            // conversation content and not a failure — skipped with no count.
            measurement = Some(record);
            continue;
        }
        let Some(sequence) = record.get("seq").and_then(Value::as_u64) else {
            // The replica record model itself refuses the same shape
            // (`grok_bot::ReplicaRecord::from_value`), so the reader counts
            // it here rather than guessing a position for it.
            conversation.unrecognized_lines += 1;
            continue;
        };
        if record.get("kind").and_then(Value::as_str).is_none() {
            conversation.unrecognized_lines += 1;
            continue;
        }
        records.entry(sequence).or_default().push(record);
    }
    for variants in records.values() {
        render_record(pick_variant(variants), conversation);
    }
    // The gap state is the measurement the metadata line carried, or the
    // struct's own no-claim defaults when the body has no metadata line at
    // all: `records` proves nothing about completeness (variants share seqs,
    // so a gap hiding under a rendered seq would be recomputed as present).
    if let Some(measurement) = measurement
        .as_ref()
        .and_then(|record| record.get("_chat_stasher"))
    {
        conversation.sequence_gap_runs = gap_runs(measurement.get("sequence_gaps"));
        conversation.partial =
            measurement.get("partial_replica").and_then(Value::as_bool) == Some(true);
    }
}

/// The variant of one seq that is rendered.
///
/// The last complete observation (`isStreaming == false`): first-observed
/// would freeze a streaming partial as the visible words, and "any complete
/// one" would prefer an older rewrite over the newest. When no observation
/// is complete — the window caught the seq still streaming — the last
/// observation is what the replica left it as.
fn pick_variant(variants: &[Value]) -> &Value {
    let last = variants
        .last()
        .expect("a seq group holds at least the record that created it");
    variants
        .iter()
        .rev()
        .find(|variant| variant.get("isStreaming").and_then(Value::as_bool) == Some(false))
        .unwrap_or(last)
}

/// Render the picked variant of one seq.
fn render_record(record: &Value, conversation: &mut Conversation) {
    let Some(kind) = record.get("kind").and_then(Value::as_str) else {
        // Unreachable: the line walk only groups records that carry a string
        // kind. Handled rather than asserted because the archive is
        // append-only — a line a future collector writes must never panic
        // the reader.
        conversation.unrecognized_lines += 1;
        return;
    };
    match kind {
        "message" => render_message(record, conversation),
        "notice" => render_notice(record, conversation),
        "user-attachment" => render_user_attachment(record, conversation),
        // A sync cursor: "N newer rows exist", a record of nothing said.
        // Known-empty like the chatgpt reader's null mapping node — no
        // message, no count (GB-5 §2/§6).
        "at" => {}
        // `event` is understood automation bookkeeping with no message in
        // it; `send-message` and `voice-call` are the kinds the next slice
        // of this reader's plan maps, and until then each record is counted
        // as understood-but-not-yet-rendered. None of the three may be
        // dropped: the count is what keeps the archive's lines visible while
        // their renderers are written.
        "event" | "send-message" | "voice-call" => conversation.unrendered_lines += 1,
        _ => conversation.unrecognized_lines += 1,
    }
}

/// `kind: message` — one turn of the human↔agent transcript.
fn render_message(record: &Value, conversation: &mut Conversation) {
    // Text is `content`, or `richText` only when content carries nothing —
    // never both: a record holding both renders its words once (GB-5 §2),
    // and the shaped field is not glued onto the plain one.
    let Some(text) =
        text_field(record.get("content")).or_else(|| text_field(record.get("richText")))
    else {
        // Every text field empty: a known-empty record, not an unreadable
        // one. Skipped with no count — the grok-cli empty-export precedent,
        // and never rendered as an empty message (GB-5 §2).
        return;
    };
    conversation.push_message(Message {
        role: message_role(record),
        time: message_time(record.get("timestampMs")),
        blocks: vec![Block::Text(text)],
    });
}

/// `kind: notice` — a platform banner. `System`, because a notice is
/// banner-shaped: presenting it as a speaker would answer a question the
/// record never asks (GB-5 §2).
fn render_notice(record: &Value, conversation: &mut Conversation) {
    let Some(text) = text_field(record.get("text")) else {
        // Known-empty, same as the message kind above: no count.
        return;
    };
    conversation.push_message(Message {
        role: Role::System,
        time: message_time(record.get("timestampMs")),
        blocks: vec![Block::Text(text)],
    });
}

/// `kind: user-attachment` — the person's upload. The uploader being the
/// person is the whole role claim (GB-5 §2); the attachment's own identity
/// is the file name and byte size the replica recorded. No media-type field
/// exists in the observed shape, so none is claimed.
fn render_user_attachment(record: &Value, conversation: &mut Conversation) {
    let attachment = AttachmentRef {
        name: record
            .get("file_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        media_type: None,
        bytes: record.get("byteSize").and_then(Value::as_u64),
    };
    conversation.attachments.push(attachment.clone());
    conversation.push_message(Message {
        role: Role::User,
        time: message_time(record.get("timestampMs")),
        blocks: vec![Block::AttachmentRef(attachment)],
    });
}

/// The speaker of a `message` record (GB-5 §2).
///
/// `role` first, read with the shared role vocabulary. Then the identity
/// objects the channel-carried shape adds: `fromUser` names the person,
/// `fromAgent` the agent, and `author.kind` — an agent-kind discriminator,
/// not a role word — establishes the agent side when its value is a role
/// word this build knows. `channelSender`/`channel`/`connector` are routing
/// metadata and `toAgent` names the addressee, so none of them decide: a
/// record with text and none of the identities is speaker-unknown, which is
/// an honest "the record did not say" rather than a guess.
fn message_role(record: &Value) -> Role {
    if let Some(role) = record
        .get("role")
        .and_then(Value::as_str)
        .and_then(Role::from_str)
    {
        return role;
    }
    if record.get("fromUser").is_some_and(|value| !value.is_null()) {
        return Role::User;
    }
    if record
        .get("fromAgent")
        .is_some_and(|value| !value.is_null())
    {
        return Role::Assistant;
    }
    if record
        .pointer("/author/kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| Role::from_str(kind).is_some())
    {
        return Role::Assistant;
    }
    Role::Unknown
}

/// A text field as the reader claims it: present, a string, non-empty. An
/// absent, mistyped or empty field is "the record made no claim there", so
/// the caller falls to the next field or to the measured-empty skip.
fn text_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The metadata line's `sequence_gaps` — missing positions from one through
/// the highest observed seq, as `collect` measured them — compressed to
/// inclusive runs (GB-5 §7). A run is the UI's "rows 7–12 missing"
/// formatting of the position set, including the head run a replica whose
/// first rows slid out of the observed window leaves behind: the measurement
/// starts at one by construction, so a head gap is a listed position like
/// any other, never something to infer from where the records begin.
fn gap_runs(gaps: Option<&Value>) -> Vec<(u64, u64)> {
    let Some(positions) = gaps.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut sorted: Vec<u64> = positions.iter().filter_map(Value::as_u64).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for position in sorted {
        match runs.last_mut() {
            Some((_, end)) if end.saturating_add(1) == position => *end = position,
            _ => runs.push((position, position)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use crate::activity::TimeSource;
    use crate::normalize::{normalize, Block, MessageTime, Provenance, Role};

    /// One fixture per rendered kind, in one body: every record carries the
    /// measured shape of its kind (seq/kind/timestampMs at the record top
    /// level; `message`'s text in `content`, `notice`'s in `text`,
    /// `user-attachment`'s identity in `file_name`/`byteSize`), and every
    /// line is accounted for — rendered, or counted where the plan puts it.
    #[test]
    fn replica_body_renders_each_known_kind_and_counts_the_rest() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","isStreaming":false,"role":"user","content":"synthetic user question"}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"message","seq":2,"timestampMs":1780000000500,"requestId":"synthetic-request-2","isStreaming":false,"role":"assistant","content":"synthetic agent answer"}"#,
            "\n",
            r#"{"id":"synthetic-id-3","kind":"notice","seq":3,"timestampMs":1780000001000,"text":"synthetic banner text"}"#,
            "\n",
            r#"{"id":"synthetic-id-4","kind":"event","seq":4,"timestampMs":1780000002000,"event":{"type":"synthetic-event-type","action":"synthetic-action","automationId":"synthetic-automation","automationName":"synthetic automation name"}}"#,
            "\n",
            r#"{"id":"synthetic-id-5","kind":"user-attachment","seq":5,"timestampMs":1780000002500,"batchId":"synthetic-batch-1","clientNonce":"synthetic-nonce-1","file_name":"synthetic-report.txt","file_path":"/synthetic/sessions/synthetic-report.txt","byteSize":128,"height":1,"width":1}"#,
            "\n",
            r#"{"kind":"at","seq":6,"entryId":"synthetic-entry-1","newerRows":3}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
        assert_eq!(conversation.messages.len(), 4);
        assert!(matches!(conversation.messages[0].role, Role::User));
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic user question"
        ));
        assert!(matches!(conversation.messages[1].role, Role::Assistant));
        assert!(matches!(conversation.messages[2].role, Role::System));
        assert!(matches!(
            conversation.messages[2].blocks[0],
            Block::Text(ref text) if text == "synthetic banner text"
        ));
        assert!(matches!(conversation.messages[3].role, Role::User));
        match &conversation.messages[3].blocks[0] {
            Block::AttachmentRef(attachment) => {
                assert_eq!(attachment.name.as_deref(), Some("synthetic-report.txt"));
                assert_eq!(attachment.media_type, None);
                assert_eq!(attachment.bytes, Some(128));
            }
            other => panic!("expected an attachment ref, got {other:?}"),
        }
        assert_eq!(conversation.attachments.len(), 1);
        // The `event` record: understood, unrendered, counted. The `at`
        // record: a sync cursor, skipped with zero counts (not even an
        // unrendered one).
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
        // `timestampMs` is epoch milliseconds: known, and labelled inferred
        // rather than exact — the same conversion the sibling readers use.
        assert!(matches!(
            conversation.messages[0].time,
            MessageTime::Known {
                source: TimeSource::Inferred { .. },
                ..
            }
        ));
    }

    /// The role fallbacks of the channel-carried author shape, and the
    /// fields that never decide a speaker. A record with text but no
    /// parsable `role` and no identity object stays speaker-unknown; routing
    /// metadata (`channelSender`, `channel`, `connector`) and the addressee
    /// (`toAgent`) are refused as speaker evidence (GB-5 §2).
    #[test]
    fn message_role_comes_from_role_then_identity_never_routing_fields() {
        let body = concat!(
            r#"{"kind":"message","seq":1,"isStreaming":false,"fromUser":{"name":"synthetic-person"},"content":"synthetic channel human words"}"#,
            "\n",
            r#"{"kind":"message","seq":2,"isStreaming":false,"fromAgent":{"id":"synthetic-agent-id","name":"synthetic agent"},"content":"synthetic channel agent words"}"#,
            "\n",
            r#"{"kind":"message","seq":3,"isStreaming":false,"author":{"id":"synthetic-author-id","kind":"assistant","name":"synthetic author"},"content":"synthetic authored words"}"#,
            "\n",
            r#"{"kind":"message","seq":4,"isStreaming":false,"channel":"synthetic-channel","channelSender":"synthetic sender label","connector":"synthetic-connector","toAgent":{"id":"synthetic-target-id","kind":"assistant","name":"synthetic target"},"content":"synthetic routing-only words"}"#,
            "\n",
            r#"{"kind":"message","seq":5,"isStreaming":false,"role":"tool","content":"synthetic tool-role words"}"#,
            "\n",
            // A role word the shared vocabulary does not know falls through
            // to the identities; with none, the speaker is unknown.
            r#"{"kind":"message","seq":6,"isStreaming":false,"role":"agent","content":"synthetic unknown-role words"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 6);
        assert!(matches!(conversation.messages[0].role, Role::User));
        assert!(matches!(conversation.messages[1].role, Role::Assistant));
        assert!(matches!(conversation.messages[2].role, Role::Assistant));
        assert!(matches!(conversation.messages[3].role, Role::Unknown));
        assert!(matches!(conversation.messages[4].role, Role::Tool));
        assert!(matches!(conversation.messages[5].role, Role::Unknown));
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// Text is `content`, or `richText` when content carries nothing —
    /// never both, and never both glued together (GB-5 §2).
    #[test]
    fn message_text_prefers_content_and_uses_richtext_only_as_fallback() {
        let body = concat!(
            r#"{"kind":"message","seq":1,"isStreaming":false,"content":"synthetic plain words","richText":"synthetic shaped words"}"#,
            "\n",
            r#"{"kind":"message","seq":2,"isStreaming":false,"content":"","richText":"synthetic shaped words"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic plain words"
        ));
        assert!(matches!(
            conversation.messages[1].blocks[0],
            Block::Text(ref text) if text == "synthetic shaped words"
        ));
    }

    /// A record with empty text in every field is a measured empty: no
    /// message and no count — "known empty", not "could not read" (GB-5 §2).
    #[test]
    fn a_record_with_empty_text_everywhere_is_a_measured_empty() {
        let body = concat!(
            r#"{"kind":"message","seq":1,"isStreaming":false,"role":"user","content":"","richText":""}"#,
            "\n",
            r#"{"kind":"notice","seq":2,"text":""}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        // The provenance pins that the zeros below were *measured* by this
        // reader — against a raw-view-only build the same zeros are unread,
        // and this test would pass vacuously.
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
        assert_eq!(conversation.messages.len(), 0);
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// Variants of one seq: the streaming rewrite the union keeps as two
    /// lines of one position. The complete observation is the message
    /// rendered, the streaming partial it replaced is neither rendered nor
    /// counted, and a seq seen only streaming renders its last observation
    /// — never a fabricated concatenation of both (GB-5 §1).
    #[test]
    fn one_seq_renders_one_message_from_its_final_variant() {
        let body = concat!(
            r#"{"kind":"message","seq":1,"isStreaming":true,"role":"assistant","content":"synthetic partial"}"#,
            "\n",
            r#"{"kind":"message","seq":1,"isStreaming":false,"role":"assistant","content":"synthetic final answer"}"#,
            "\n",
            // A seq the window caught still streaming: no complete variant,
            // so the last (only) observation is what the replica left.
            r#"{"kind":"message","seq":2,"isStreaming":true,"role":"assistant","content":"synthetic still-streaming tail"}"#,
            "\n",
            // Two complete observations of one position: the later rewrite
            // is the message, not the older one.
            r#"{"kind":"message","seq":3,"isStreaming":false,"role":"assistant","content":"synthetic older final"}"#,
            "\n",
            r#"{"kind":"message","seq":3,"isStreaming":false,"role":"assistant","content":"synthetic newer final"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 3);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic final answer"
        ));
        assert!(matches!(
            conversation.messages[1].blocks[0],
            Block::Text(ref text) if text == "synthetic still-streaming tail"
        ));
        assert!(matches!(
            conversation.messages[2].blocks[0],
            Block::Text(ref text) if text == "synthetic newer final"
        ));
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// The metadata line is skipped with no count, and its measurement is
    /// what the conversation exposes: the position list compressed to
    /// inclusive runs (head run included — a listed position, not an
    /// inference), the partial flag as recorded, the last metadata line
    /// winning in a body of concatenated shards. A body with no metadata
    /// line makes no gap claim at all. Runs are the metadata's own
    /// measurement, never recomputed from the rendered seqs (GB-5 §7).
    #[test]
    fn metadata_line_is_skipped_and_its_gap_measurement_is_exposed() {
        let body = concat!(
            r#"{"kind":"message","seq":3,"isStreaming":false,"role":"user","content":"synthetic after a head gap"}"#,
            "\n",
            r#"{"_chat_stasher":{"source":"grok-bot","agent_name":"synthetic agent","agent_name_state":"known","partial_replica":true,"sequence_gaps":[1,2,5,6],"observed_sequence_min":3,"observed_sequence_max":6}}"#,
            "\n",
            r#"{"kind":"message","seq":4,"isStreaming":false,"role":"assistant","content":"synthetic answer"}"#,
            "\n",
            r#"{"_chat_stasher":{"source":"grok-bot","agent_name":"synthetic agent","partial_replica":true,"sequence_gaps":[1,2,4,9,10,11],"observed_sequence_min":3,"observed_sequence_max":11}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.sequence_gap_runs, [(1, 2), (4, 4), (9, 11)]);
        assert!(conversation.partial);
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);

        // The last metadata line is the winner even when it understates an
        // earlier one, and its own partial flag is what is recorded.
        let replaced = concat!(
            r#"{"_chat_stasher":{"partial_replica":true,"sequence_gaps":[1,2,3]}}"#,
            "\n",
            r#"{"kind":"notice","seq":4,"text":"synthetic later notice"}"#,
            "\n",
            r#"{"_chat_stasher":{"partial_replica":false,"sequence_gaps":[]}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", replaced);
        assert_eq!(conversation.messages.len(), 1);
        assert!(conversation.sequence_gap_runs.is_empty());
        assert!(!conversation.partial);

        // A metadata-only body: a shard carrying just the measurement line.
        // No message, no count, and the measurement is still exposed.
        let measured_only = r#"{"_chat_stasher":{"partial_replica":true,"sequence_gaps":[7,8]}}"#;
        let conversation = normalize("grok-bot", measured_only);
        assert_eq!(conversation.messages.len(), 0);
        assert_eq!(conversation.sequence_gap_runs, [(7, 8)]);
        assert!(conversation.partial);

        // No metadata line: the reader makes no gap claim — `records` proves
        // nothing about completeness, and a zero here would assert one.
        let unmetered =
            r#"{"kind":"message","seq":1,"isStreaming":false,"content":"synthetic words"}"#;
        let conversation = normalize("grok-bot", unmetered);
        assert_eq!(conversation.messages.len(), 1);
        assert!(conversation.sequence_gap_runs.is_empty());
        assert!(!conversation.partial);
    }

    /// An unknown kind, a record missing `seq` or `kind`, and a line that
    /// is not JSON: each is counted and the session continues — one bad line
    /// never kills a session, and the rest renders (GB-5 §2/§9).
    #[test]
    fn unknown_kinds_and_unidentifiable_lines_are_counted_and_the_session_continues() {
        let body = concat!(
            r#"{"kind":"future-kind","seq":1,"timestampMs":1780000000000}"#,
            "\n",
            r#"{"kind":"message","seq":2,"isStreaming":false,"role":"user","content":"synthetic words"}"#,
            "\n",
            r#"{"kind":"message","isStreaming":false,"content":"synthetic no-seq words"}"#,
            "\n",
            "not json at all\n",
            r#"{"seq":5}"#,
            "\n",
            r#"{"seq":"not-a-number","kind":"message","content":"synthetic string-seq words"}"#,
            "\n",
            r#"{"kind":"message","seq":7,"isStreaming":false,"role":"assistant","content":"synthetic later answer"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic words"
        ));
        assert!(matches!(
            conversation.messages[1].blocks[0],
            Block::Text(ref text) if text == "synthetic later answer"
        ));
        assert_eq!(conversation.unrendered_lines, 0);
        // future-kind, missing seq, unparseable line, missing kind, and a
        // non-numeric seq: one count each.
        assert_eq!(conversation.unrecognized_lines, 5);
    }

    /// The two kinds whose field mapping is the next slice of this reader's
    /// plan: each record is counted understood-but-not-yet-rendered, never
    /// silently dropped, so the archive's line accounting stays complete
    /// while their renderers are written (GB-5 §10).
    #[test]
    fn send_message_and_voice_call_are_counted_as_not_yet_rendered() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","content":"synthetic bot content"}}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"voice-call","seq":2,"timestampMs":1780000001000,"call":{"callId":"synthetic-call-1","turnCount":1,"nudgeCount":0},"conversation":{"turns":[],"nudges":[]}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 0);
        assert_eq!(conversation.unrendered_lines, 2);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// A body of concatenated shards may interleave seqs; the rendered
    /// conversation follows the replica's own position order (GB-5 §1),
    /// not the line order the shards arrived in.
    #[test]
    fn rendered_output_is_sorted_by_seq_not_line_order() {
        let body = concat!(
            r#"{"kind":"message","seq":2,"isStreaming":false,"role":"assistant","content":"synthetic second position"}"#,
            "\n",
            r#"{"kind":"message","seq":1,"isStreaming":false,"role":"user","content":"synthetic first position"}"#,
            "\n",
            r#"{"kind":"message","seq":3,"isStreaming":false,"role":"assistant","content":"synthetic third position"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 3);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic first position"
        ));
        assert!(matches!(
            conversation.messages[1].blocks[0],
            Block::Text(ref text) if text == "synthetic second position"
        ));
        assert!(matches!(
            conversation.messages[2].blocks[0],
            Block::Text(ref text) if text == "synthetic third position"
        ));
    }

    /// A record without a readable `timestampMs` keeps the time unknown with
    /// the reason, the same state the sibling readers produce — never a zero
    /// pretending to be a measurement (GB-5 §2).
    #[test]
    fn a_record_without_a_readable_time_stays_time_unknown() {
        let body = concat!(
            r#"{"kind":"notice","seq":1,"text":"synthetic untimed notice"}"#,
            "\n",
            r#"{"kind":"notice","seq":2,"timestampMs":"not-a-number","text":"synthetic mistyped-time notice"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            conversation.messages[0].time,
            MessageTime::Unknown { .. }
        ));
        assert!(matches!(
            conversation.messages[1].time,
            MessageTime::Unknown { .. }
        ));
    }
}
