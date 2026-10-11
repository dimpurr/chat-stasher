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
//! This file is the approved GB-5 reader plan, all three slices (§10;
//! 2026-10-11) — the transcript kinds, the tool-shaped kinds, and the
//! sub-agent association rules. What renders here and why:
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
//! * `send-message` — the bot→user direction: base role `Assistant`, one
//!   record → one message, its own words from `message.content` or — only
//!   when content carries nothing — `message.alt`, never both. The rest of
//!   the record joins the same message as further blocks: the question it
//!   asked (`message.widget` → one text block of prompt, help text, option
//!   labels and the answered/skipped resolution), its tool traffic
//!   (`message.approval`, `message.ask` and the record-level `box`
//!   request/resolution, each a `ToolCall` named after its gate), and what
//!   it delivered (`message.images[]`, `file_name`, `url` as attachment
//!   references). Threading and routing metadata is recognised, raw-only
//!   and uncounted (GB-5 §3).
//! * the **secret rule** — a record naming a credential exchange (a
//!   `secretRequest` or `secretProvided` key anywhere in it) renders **no**
//!   message and contributes no text at all, not even its own content or
//!   the `label`/`target`/`description` strings, which name the credential.
//!   It is counted unrendered — understood, deliberately withheld — never
//!   unrecognized. The raw record stays archived untouched; the reader
//!   withholds it from every rendered surface only (GB-5 §3).
//! * `voice-call` — one spoken call expanded into messages: the call
//!   description first with its speaker unknown, then the turns and nudges
//!   merged in the order the call stamped them (`atMs`); a turn's speaker
//!   is read with the shared role vocabulary and any other speaker label
//!   stays speaker-unknown, each nudge is an `Assistant` message whose
//!   `answer` — null in every observation — adds no block rather than an
//!   empty claim. The call's bookkeeping (ids, durations, counts, the
//!   always-empty `events`/`toolCalls`/`overheard` arrays) is raw-only
//!   (GB-5 §4).
//! * **Sub-agents** (GB-5 §5) — a delegation an approval carried
//!   (`message.approval.subagent`) renders inside the parent agent's
//!   conversation, as that approval gate's own `ToolCall` with the
//!   delegation's `title` in the summary: delegation is tool traffic. The
//!   archive's identity is machine + agent uuid and no replica exists
//!   behind a child's id, so the one conversation stays whole — no child
//!   conversation is invented, and nothing is merged from anywhere else.
//!   `subagent.id`/`parentAgentId` are ids and stay raw-only; an
//!   `event.from`/`to` transfer record around a delegation is the `event`
//!   bookkeeping counted unrendered, never dropped.
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
    if names_a_secret_exchange(record) {
        // The secret rule, ahead of every kind mapping (GB-5 §3): a record
        // that names a credential exchange renders no message at all — not
        // its content, not the label/target/description that name the
        // credential. Counted unrendered, understood and deliberately
        // withheld, never unrecognized: the shape is known; this is the one
        // place the reader refuses to render a known shape.
        conversation.unrendered_lines += 1;
        return;
    }
    match kind {
        "message" => render_message(record, conversation),
        "notice" => render_notice(record, conversation),
        "user-attachment" => render_user_attachment(record, conversation),
        "send-message" => render_send_message(record, conversation),
        "voice-call" => render_voice_call(record, conversation),
        // A sync cursor: "N newer rows exist", a record of nothing said.
        // Known-empty like the chatgpt reader's null mapping node — no
        // message, no count (GB-5 §2/§6).
        "at" => {}
        // `event` is understood automation bookkeeping with no message in
        // it — including the `from`/`to` transfer records around a
        // sub-agent delegation, which associate with the parent session
        // only (GB-5 §5): the transfer is not speech, no replica exists
        // behind a child conversation to move it into, and the record may
        // not be dropped — the count is what keeps the archive's lines
        // visible.
        "event" => conversation.unrendered_lines += 1,
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

/// Whether a record names a credential exchange anywhere in its JSON: a
/// `secretRequest` or `secretProvided` key at any depth.
///
/// The measured shape puts them at `message.secretRequest` and the record's
/// top level, but the rule is keyed on the names, not the nesting: the
/// strings around such a key name a credential, and where the app puts them
/// next is not a fact this withhold should depend on. Over-triggering is
/// also cheap and one-sided — it can only withhold a record that names a
/// secret exchange, which is exactly what the rule exists to do.
fn names_a_secret_exchange(record: &Value) -> bool {
    match record {
        Value::Object(map) => map.iter().any(|(key, value)| {
            (key == "secretRequest" || key == "secretProvided") || names_a_secret_exchange(value)
        }),
        Value::Array(items) => items.iter().any(names_a_secret_exchange),
        _ => false,
    }
}

/// `kind: send-message` — the bot→user direction, one message per record
/// with base role `Assistant` (GB-5 §3).
///
/// The message carries the record's own words, its widget question, its
/// tool traffic and its delivered files as blocks, in that order. A record
/// none of whose fields held anything readable is a known-empty — skipped
/// with no count, never rendered as an empty message (the §2 common rule).
/// An approval that names a sub-agent delegation carries the delegation's
/// title in its summary — the delegation is tool traffic, and it stays
/// inside this, the parent's, conversation (GB-5 §5). Threading and routing
/// metadata (`reply_to`/`replyTo`, `serverId`, `requestId`, `boxRequestId`,
/// `intent`, `provider`, `variant`, `channel`, `connector`) is recognised
/// and raw-only: position in the thread and the channel that carried it
/// never become display text.
fn render_send_message(record: &Value, conversation: &mut Conversation) {
    let message = record.get("message").filter(|message| !message.is_null());
    let mut blocks: Vec<Block> = Vec::new();
    if let Some(message) = message {
        // Words first: `content`, or `alt` only when content carries
        // nothing — the same never-both rule the `message` kind applies to
        // its content/richText pair.
        if let Some(text) =
            text_field(message.get("content")).or_else(|| text_field(message.get("alt")))
        {
            blocks.push(Block::Text(text));
        }
        if let Some(widget) = message.get("widget").filter(|widget| !widget.is_null()) {
            if let Some(text) = widget_text(record, widget) {
                blocks.push(Block::Text(text));
            }
        }
        if let Some(approval) = message.get("approval").filter(|value| !value.is_null()) {
            blocks.push(tool_call("approval", approval_summary(approval)));
        }
        if let Some(ask) = message.get("ask").filter(|value| !value.is_null()) {
            blocks.push(tool_call("ask", joined_fields(ask, ASK_FIELDS)));
        }
        for image in message
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // An image is named by its `alt` text or, when it has none, by
            // its `url`; width/height are pixels, not bytes, and no media
            // type exists in the observed shape, so none is claimed.
            let name = text_field(image.get("alt")).or_else(|| text_field(image.get("url")));
            blocks.push(Block::AttachmentRef(AttachmentRef {
                name,
                media_type: None,
                bytes: None,
            }));
        }
        for delivery in [message.get("file_name"), message.get("url")] {
            if let Some(name) = text_field(delivery) {
                blocks.push(Block::AttachmentRef(AttachmentRef {
                    name: Some(name),
                    media_type: None,
                    bytes: None,
                }));
            }
        }
    }
    // The `box` fields live at the record's top level, so a command request
    // and resolution render even from a record whose `message` object is
    // absent. `boxRequestId` is an id and stays raw-only.
    if record.get("boxInstruction").is_some() || record.get("boxResolution").is_some() {
        blocks.push(tool_call("box", joined_fields(record, BOX_FIELDS)));
    }
    if blocks.is_empty() {
        // Known-empty (GB-5 §2): every text field empty, no tool traffic,
        // nothing delivered. No message and no count.
        return;
    }
    conversation.push_message(Message {
        role: Role::Assistant,
        time: message_time(record.get("timestampMs")),
        blocks,
    });
}

/// The `ToolCall` block for one of a send-message's gates: the name the plan
/// fixes, and the joined readable fields, falling back to the shared "no
/// input recorded" phrasing when the object holds no readable field at all —
/// the gate still happened, and its presence is the block's claim. No
/// output-size claim is invented either way.
fn tool_call(name: &str, summary: Option<String>) -> Block {
    Block::ToolCall {
        name: Some(name.to_string()),
        input_summary: summary.unwrap_or_else(|| super::compact_json(None)),
        output_bytes: None,
    }
}

/// The readable fields a summary joins, in the order each gate fixes
/// (GB-5 §3). Ids (`approval.requestId`, `ask.machineId`/`requestId`,
/// `boxRequestId`, `approval.subagent.id`/`parentAgentId`) are deliberately
/// absent: they identify the exchange or a session, and §8 keeps ids out of
/// every rendered surface. A sub-agent's `title` is not an id — it is the
/// delegation's own words — and joins the approval summary separately
/// (see [`approval_summary`], GB-5 §5).
const APPROVAL_FIELDS: &[&str] = &["summary", "reason", "command", "status"];
const ASK_FIELDS: &[&str] = &["action", "target", "status"];
const BOX_FIELDS: &[&str] = &["boxInstruction", "boxResolution"];

/// The joined readable fields of the approval gate: the §3 fields in their
/// fixed order, plus — when the approval names a sub-agent (GB-5 §5) — the
/// delegation's own `title`, the only child-identity string that renders:
/// a delegation is tool traffic inside the parent agent's conversation, and
/// `subagent.id`/`parentAgentId` are ids, which every rendered surface
/// keeps out. The whole anchor: spawning a child conversation would forge a
/// session no replica stands behind, so the delegation is read here, where
/// the parent's transcript already places it.
fn approval_summary(approval: &Value) -> Option<String> {
    let summary = joined_fields(approval, APPROVAL_FIELDS);
    match text_field(approval.pointer("/subagent/title")) {
        Some(title) => match summary {
            Some(joined) => Some(format!("{joined} · {title}")),
            None => Some(title),
        },
        None => summary,
    }
}

/// The joined readable fields of one object: the named keys, empty ones
/// absent rather than rendered as gaps. `None` when nothing was readable.
fn joined_fields(value: &Value, keys: &[&str]) -> Option<String> {
    let parts: Vec<String> = keys
        .iter()
        .filter_map(|key| text_field(value.get(*key)))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The one text block a widget record renders (GB-5 §3): the prompt, the
/// help text, the option labels — a description is not a label and stays
/// raw-only — and the resolution. `respondedValue` (record-level) resolves
/// the question as `answered: <value>`; `widgetSkipped` marks it `(skipped)`;
/// a record carrying both keeps the answer and drops the skip.
fn widget_text(record: &Value, widget: &Value) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(prompt) = text_field(widget.get("prompt")) {
        lines.push(prompt);
    }
    if let Some(help) = text_field(widget.get("helpText")) {
        lines.push(help);
    }
    for option in widget
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(label) = text_field(option.get("label")) {
            lines.push(label);
        }
    }
    if let Some(answer) = text_field(record.get("respondedValue")) {
        lines.push(format!("answered: {answer}"));
    } else if record.get("widgetSkipped").and_then(Value::as_bool) == Some(true) {
        lines.push("(skipped)".to_string());
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `kind: voice-call` — one spoken call, expanded into messages
/// (GB-5 §4).
///
/// The description, when the call recorded one, is the first message: what
/// the call was about, speaker unknown — the record names no speaker for
/// its own summary. The turns and nudges then follow in the order the call
/// stamped on them, merged: a turn keeps the speaker its label spells (read
/// with the shared role vocabulary; any other label is speaker-unknown, the
/// `Role` variant carrying no `why` of its own) and a nudge is an
/// `Assistant` message, because a nudge is system-initiated. The nudge's
/// `answer` is null in every observation and renders no block — the
/// absence of an answer is not rendered as an empty claim. Call
/// bookkeeping (`callId`, `realtimeConversationId`, `durationMs`,
/// `endedAtMs`, `turnCount`, `nudgeCount`, the always-empty
/// `events`/`toolCalls`/`overheard` arrays) is recognised, raw-only and
/// uncounted: a duration is not a turn, and fabricating a "call ended"
/// message would invent a speaker to display a number.
fn render_voice_call(record: &Value, conversation: &mut Conversation) {
    if let Some(description) = text_field(record.pointer("/call/description")) {
        conversation.push_message(Message {
            role: Role::Unknown,
            time: message_time(record.get("timestampMs")),
            blocks: vec![Block::Text(description)],
        });
    }
    let mut spoken: Vec<(Option<i64>, Message)> = Vec::new();
    let turns = record
        .pointer("/conversation/turns")
        .and_then(Value::as_array);
    for turn in turns.into_iter().flatten() {
        // An empty-text turn is a known-empty (the §2 common rule): no
        // message and no count, never an empty message.
        let Some(text) = text_field(turn.get("text")) else {
            continue;
        };
        let role = turn
            .get("speaker")
            .and_then(Value::as_str)
            .and_then(Role::from_str)
            .unwrap_or(Role::Unknown);
        spoken.push((
            turn.get("atMs").and_then(Value::as_i64),
            Message {
                role,
                time: message_time(turn.get("atMs")),
                blocks: vec![Block::Text(text)],
            },
        ));
    }
    let nudges = record
        .pointer("/conversation/nudges")
        .and_then(Value::as_array);
    for nudge in nudges.into_iter().flatten() {
        // An empty-request nudge is the same known-empty as a turn.
        let Some(text) = text_field(nudge.get("request")) else {
            continue;
        };
        spoken.push((
            nudge.get("atMs").and_then(Value::as_i64),
            Message {
                role: Role::Assistant,
                time: message_time(nudge.get("atMs")),
                blocks: vec![Block::Text(text)],
            },
        ));
    }
    // `atMs` order, the call's own stamp. The sort is stable, so turns and
    // nudges sharing a timestamp keep the order the record listed them in;
    // a turn or nudge with no readable `atMs` sorts first and keeps the
    // time-unknown state the §2 common rule gives it.
    spoken.sort_by_key(|(at_ms, _)| *at_ms);
    for (_, message) in spoken {
        conversation.push_message(message);
    }
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

    /// `send-message` is the bot→user/toolbox direction: base role
    /// `Assistant`, one record → one message, its text from `message.content`
    /// or — only when content carries nothing — `message.alt`, never both
    /// (GB-5 §3).
    #[test]
    fn send_message_renders_assistant_text_from_content_else_alt() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","content":"synthetic bot words"}}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"send-message","seq":2,"timestampMs":1780000000500,"requestId":"synthetic-request-2","message":{"type":"synthetic-type","content":"synthetic main words","alt":"synthetic alternate words"}}"#,
            "\n",
            r#"{"id":"synthetic-id-3","kind":"send-message","seq":3,"timestampMs":1780000001000,"requestId":"synthetic-request-3","message":{"type":"synthetic-type","content":"","alt":"synthetic alternate words"}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 3);
        for message in &conversation.messages {
            assert!(matches!(message.role, Role::Assistant));
        }
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic bot words"
        ));
        // A record holding both renders its words once: `alt` is a fallback,
        // not a second text nor something glued onto the content.
        assert_eq!(conversation.messages[1].blocks.len(), 1);
        assert!(matches!(
            &conversation.messages[1].blocks[0],
            Block::Text(text) if text == "synthetic main words"
        ));
        assert!(matches!(
            &conversation.messages[2].blocks[0],
            Block::Text(text) if text == "synthetic alternate words"
        ));
        assert!(matches!(
            conversation.messages[0].time,
            MessageTime::Known {
                source: TimeSource::Inferred { .. },
                ..
            }
        ));
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// `message.widget` — the question the bot asked — renders as **one**
    /// text block: prompt, help text, the option labels, and the resolution
    /// line (`answered: <value>`, or `(skipped)` when `widgetSkipped`).
    /// Two messages would invent an intra-seq turn order the record does not
    /// carry, and a tool call would hide a question behind a call name; an
    /// option `description` is not a label and stays raw-only (GB-5 §3).
    #[test]
    fn a_send_message_widget_renders_one_text_with_labels_and_resolution() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","widget":{"prompt":"synthetic pick a file","helpText":"synthetic pick exactly one","options":[{"label":"synthetic option one","value":"synthetic-value-1","description":"synthetic option one description","style":"default"},{"label":"synthetic option two","value":"synthetic-value-2"}],"allowCustom":true,"multiSelect":false,"dismissOnMoveOn":false}},"respondedValue":"synthetic-value-1"}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"send-message","seq":2,"timestampMs":1780000010000,"requestId":"synthetic-request-2","message":{"type":"synthetic-type","widget":{"prompt":"synthetic pick again","options":[{"label":"synthetic option three","value":"synthetic-value-3"}]}},"widgetSkipped":true}"#,
            "\n",
            // Both a resolution and the skip flag: the answered line wins and
            // the skipped one is not glued next to it.
            r#"{"id":"synthetic-id-3","kind":"send-message","seq":3,"timestampMs":1780000020000,"requestId":"synthetic-request-3","message":{"type":"synthetic-type","widget":{"prompt":"synthetic choose a path","options":[{"label":"synthetic option four","value":"synthetic-value-4"}]}},"respondedValue":"synthetic-value-4","widgetSkipped":true}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 3);
        for message in &conversation.messages {
            assert!(matches!(message.role, Role::Assistant));
            assert_eq!(message.blocks.len(), 1);
        }
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text
                == "synthetic pick a file\nsynthetic pick exactly one\nsynthetic option one\nsynthetic option two\nanswered: synthetic-value-1"
        ));
        assert!(matches!(
            &conversation.messages[1].blocks[0],
            Block::Text(text) if text
                == "synthetic pick again\nsynthetic option three\n(skipped)"
        ));
        assert!(matches!(
            &conversation.messages[2].blocks[0],
            Block::Text(text) if text
                == "synthetic choose a path\nsynthetic option four\nanswered: synthetic-value-4"
        ));
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// The tool traffic of a `send-message`: `message.approval`,
    /// `message.ask` and the record-level `box` request/resolution each
    /// become one `ToolCall` block — a name the gate is known by, and the
    /// joined readable fields of the object. Ids never reach a summary
    /// (`approval.requestId`, `ask.machineId`/`requestId`,
    /// `boxRequestId`), and a record-level `box` pair renders even when the
    /// record's `message` object holds no text of its own (GB-5 §3).
    #[test]
    fn send_message_approval_ask_and_box_become_tool_calls() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","approval":{"summary":"synthetic approval summary","reason":"synthetic approval reason","command":"synthetic approval command","proposedRule":"synthetic proposed rule","status":"approved","requestId":"synthetic-approval-request","surface":"sync"}}}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"send-message","seq":2,"timestampMs":1780000001000,"requestId":"synthetic-request-2","message":{"type":"synthetic-type","ask":{"action":"synthetic ask action","target":"synthetic ask target","status":"pending","machineId":"synthetic-machine","requestId":"synthetic-ask-request"}}}"#,
            "\n",
            r#"{"id":"synthetic-id-3","kind":"send-message","seq":3,"timestampMs":1780000002000,"requestId":"synthetic-request-3","boxInstruction":"synthetic box instruction","boxRequestId":"synthetic-box-request","boxResolution":"synthetic box resolution","message":{"type":"synthetic-type"}}"#,
            "\n",
            // The `box` fields are record-level: a record with no `message`
            // object at all still renders its command request and resolution.
            r#"{"id":"synthetic-id-4","kind":"send-message","seq":4,"timestampMs":1780000003000,"boxInstruction":"synthetic box instruction two","boxResolution":"synthetic box resolution two"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 4);
        for message in &conversation.messages {
            assert!(matches!(message.role, Role::Assistant));
            assert_eq!(message.blocks.len(), 1);
        }
        for (message, (name, summary)) in conversation.messages.iter().zip([
            (
                "approval",
                "synthetic approval summary · synthetic approval reason · synthetic approval command · approved",
            ),
            ("ask", "synthetic ask action · synthetic ask target · pending"),
            ("box", "synthetic box instruction · synthetic box resolution"),
            (
                "box",
                "synthetic box instruction two · synthetic box resolution two",
            ),
        ]) {
            match &message.blocks[0] {
                Block::ToolCall {
                    name: block_name,
                    input_summary,
                    output_bytes,
                } => {
                    assert_eq!(block_name.as_deref(), Some(name));
                    assert_eq!(input_summary, summary);
                    // The record names the call; no output field exists, so
                    // no output size is claimed.
                    assert_eq!(*output_bytes, None);
                }
                other => panic!("expected a tool call, got {other:?}"),
            }
        }
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
        // Ids stayed raw-only: no request id the record carried is in the
        // whole rendered model.
        let serialized = format!("{conversation:?}");
        for id in [
            "synthetic-approval-request",
            "synthetic-ask-request",
            "synthetic-box-request",
        ] {
            assert!(!serialized.contains(id), "`{id}` reached the model");
        }
    }

    /// The sub-agent association rule (GB-5 §5): a delegation renders
    /// inside the parent agent's conversation — the approval gate that
    /// carried it, with the delegation's own `title` joined to the
    /// summary's end — and nothing is split off or merged in: the archive
    /// keeps no replica behind a child's id, so no child conversation is
    /// invented. `subagent.id`/`parentAgentId` are session ids and stay
    /// raw-only, and the `event.from`/`to` transfer record around a
    /// delegation is `event` bookkeeping: counted unrendered, never
    /// dropped.
    #[test]
    fn a_subagent_approval_renders_inline_in_the_parent_conversation() {
        let body = concat!(
            r#"{"kind":"message","seq":1,"isStreaming":false,"role":"user","content":"synthetic delegate a task request"}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"send-message","seq":2,"timestampMs":1780000000000,"requestId":"synthetic-request-2","message":{"type":"synthetic-type","approval":{"summary":"synthetic delegation summary","reason":"synthetic delegation reason","command":"synthetic delegation command","proposedRule":"synthetic delegated rule","status":"approved","requestId":"synthetic-approval-request","surface":"sync","subagent":{"id":"synthetic-subagent-id","parentAgentId":"synthetic-parent-agent-id","title":"synthetic delegated subagent title","type":"subagent"}}}}"#,
            "\n",
            r#"{"id":"synthetic-id-3","kind":"event","seq":3,"timestampMs":1780000000100,"event":{"type":"synthetic-transfer-type","from":"synthetic-parent-agent-id","to":"synthetic-subagent-id"}}"#,
            "\n",
            r#"{"kind":"message","seq":4,"isStreaming":false,"role":"assistant","content":"synthetic parent agent answer"}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        // The parent's own conversation, with the delegation inline: one
        // session, no child split off, nothing merged in.
        assert_eq!(conversation.messages.len(), 3);
        assert!(matches!(conversation.messages[0].role, Role::User));
        assert!(matches!(conversation.messages[1].role, Role::Assistant));
        assert!(matches!(conversation.messages[2].role, Role::Assistant));
        assert_eq!(conversation.messages[1].blocks.len(), 1);
        match &conversation.messages[1].blocks[0] {
            Block::ToolCall {
                name,
                input_summary,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("approval"));
                // The delegation rides the approval gate: the §3 fields in
                // their fixed order, the delegation's title the last part.
                // `proposedRule` is §3 raw-only and stays out.
                assert_eq!(
                    input_summary,
                    "synthetic delegation summary · synthetic delegation reason · \
                     synthetic delegation command · approved · synthetic delegated subagent title"
                );
            }
            other => panic!("expected the delegation's approval tool call, got {other:?}"),
        }
        // The transfer record around the delegation: understood, counted
        // unrendered, and the session's own transcript continues.
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
        // Session identity is raw-only: neither agent id reaches the
        // rendered model.
        let serialized = format!("{conversation:?}");
        for id in ["synthetic-subagent-id", "synthetic-parent-agent-id"] {
            assert!(!serialized.contains(id), "`{id}` reached the model");
        }
    }

    /// What the bot delivered: `message.images[]`, `message.file_name` and
    /// `message.url` become attachment reference blocks inside the one
    /// message the record renders. An image is named by its `alt` text, or
    /// by its `url` when it has none; no media type or byte size exists in
    /// the observed shape, so none is claimed. Unlike the person's upload
    /// (§2), a bot-sent file is not pushed to `conversation.attachments`.
    #[test]
    fn send_message_images_files_and_urls_render_as_attachment_refs() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","content":"synthetic report delivered","images":[{"alt":"synthetic chart alt text","url":"synthetic://chart.png","width":4,"height":3},{"alt":"","url":"synthetic://plain.png","width":2,"height":1}],"file_name":"synthetic-report.txt","url":"synthetic://synthetic-report.txt"}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        assert_eq!(conversation.messages.len(), 1);
        let blocks = &conversation.messages[0].blocks;
        assert_eq!(blocks.len(), 5);
        assert!(matches!(
            &blocks[0],
            Block::Text(text) if text == "synthetic report delivered"
        ));
        for (block, name) in blocks[1..].iter().zip([
            "synthetic chart alt text",
            "synthetic://plain.png",
            "synthetic-report.txt",
            "synthetic://synthetic-report.txt",
        ]) {
            match block {
                Block::AttachmentRef(attachment) => {
                    assert_eq!(attachment.name.as_deref(), Some(name));
                    assert_eq!(attachment.media_type, None);
                    assert_eq!(attachment.bytes, None);
                }
                other => panic!("expected an attachment ref, got {other:?}"),
            }
        }
        // The bot's delivery stays inside its message; §2's push to
        // `conversation.attachments` is the user's upload, not this.
        assert!(conversation.attachments.is_empty());
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    /// The secret rule (GB-5 §3): a record naming a credential exchange — a
    /// `secretRequest` anywhere in it, or a `secretProvided` key — renders
    /// zero messages and contributes zero text, including its own `content`
    /// and the `label`/`target`/`description` strings, which name the
    /// credential. Counted unrendered (understood, deliberately withheld),
    /// never unrecognized. The raw record stays archived untouched — the
    /// reader withholds it from every rendered surface only, and here that
    /// includes the whole serialized conversation, which must lack every
    /// secret-adjacent string the fixture wrote.
    #[test]
    fn send_message_secret_records_render_nothing_and_reach_no_text() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","content":"synthetic secret adjacent words","secretRequest":{"label":"synthetic-secret-label","description":"synthetic secret description","target":{"name":"synthetic-secret-target","kind":"token","field":"value","platform":"synthetic"}}},"secretProvided":true}"#,
            "\n",
            // The key decides, not the value: a recorded "not provided"
            // still names the credential exchange, so the record renders
            // nothing even though it also carries plain content.
            r#"{"id":"synthetic-id-2","kind":"send-message","seq":2,"timestampMs":1780000001000,"requestId":"synthetic-request-2","message":{"type":"synthetic-type","content":"synthetic second secret adjacent words"},"secretProvided":false}"#,
            "\n",
            r#"{"id":"synthetic-id-3","kind":"send-message","seq":3,"timestampMs":1780000002000,"requestId":"synthetic-request-3","message":{"type":"synthetic-type","content":"synthetic open bot words"}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        // Only the record with no secret shape renders.
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic open bot words"
        ));
        assert_eq!(conversation.unrendered_lines, 2);
        assert_eq!(conversation.unrecognized_lines, 0);
        let serialized = format!("{conversation:?}");
        for secret in [
            "synthetic-secret-label",
            "synthetic secret description",
            "synthetic-secret-target",
            "synthetic secret adjacent words",
            "synthetic second secret adjacent words",
        ] {
            assert!(
                !serialized.contains(secret),
                "the secret record's string {secret:?} reached the rendered model"
            );
        }
    }

    /// `kind: voice-call` — one spoken call expanded into messages (GB-5
    /// §4): `call.description` first with its speaker unknown, then the
    /// turns and nudges merged in the order the call stamped them
    /// (`atMs`), a turn's speaker read with the shared role vocabulary and
    /// any other speaker label left speaker-unknown, each nudge an
    /// `Assistant` message whose `answer` — null in every observation —
    /// adds no block rather than an empty claim. The call's bookkeeping
    /// (ids, durations, counts, the always-empty
    /// `events`/`toolCalls`/`overheard` arrays) is raw-only and uncounted.
    #[test]
    fn a_voice_call_expands_description_turns_and_nudges_in_atms_order() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"voice-call","seq":1,"timestampMs":1780000000000,"call":{"callId":"synthetic-call-1","realtimeConversationId":"synthetic-realtime-1","description":"synthetic call description","durationMs":90000,"endedAtMs":1780000090000,"turnCount":4,"nudgeCount":2,"answeredNudgeCount":1,"ending":"synthetic-ending"},"conversation":{"events":[],"toolCalls":[],"overheard":[],"turns":[{"id":"synthetic-turn-3","speaker":"assistant","text":"synthetic spoken answer","atMs":1780000030000},{"id":"synthetic-turn-1","speaker":"user","text":"synthetic spoken question","atMs":1780000010000},{"id":"synthetic-turn-2","speaker":"sprocket","text":"synthetic spoken unknown-speaker words","atMs":1780000020000},{"id":"synthetic-turn-4","speaker":"user","text":"","atMs":1780000040000}],"nudges":[{"id":"synthetic-nudge-2","request":"","atMs":1780000060000,"answer":null},{"id":"synthetic-nudge-1","request":"synthetic nudge request","atMs":1780000025000,"answer":null}]}}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        // Description, three spoken turns, one nudge; the empty-text turn
        // and the empty-request nudge are measured empties — no message, no
        // count.
        assert_eq!(conversation.messages.len(), 5);
        assert!(matches!(conversation.messages[0].role, Role::Unknown));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic call description"
        ));
        // The description keeps the record's own `timestampMs`, not a
        // turn's `atMs`.
        match &conversation.messages[0].time {
            MessageTime::Known { unix, .. } => assert_eq!(*unix, 1_780_000_000),
            other => panic!("expected a known time, got {other:?}"),
        }
        let spoken = &conversation.messages[1..];
        let expected: &[(Role, &str, i64)] = &[
            (Role::User, "synthetic spoken question", 1_780_000_010),
            (
                Role::Unknown,
                "synthetic spoken unknown-speaker words",
                1_780_000_020,
            ),
            (Role::Assistant, "synthetic nudge request", 1_780_000_025),
            (Role::Assistant, "synthetic spoken answer", 1_780_000_030),
        ];
        for (message, (role, text, unix)) in spoken.iter().zip(expected) {
            assert_eq!(message.role, *role);
            assert_eq!(message.blocks.len(), 1);
            assert!(matches!(
                &message.blocks[0],
                Block::Text(actual) if actual == text
            ));
            match &message.time {
                MessageTime::Known {
                    unix: actual,
                    source: TimeSource::Inferred { .. },
                } => assert_eq!(*actual, *unix),
                other => panic!("expected an inferred time, got {other:?}"),
            }
        }
        // The nudge carries no answer block: a null answer is not rendered
        // as an empty claim.
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
        let serialized = format!("{conversation:?}");
        for raw_only in [
            "synthetic-call-1",
            "synthetic-realtime-1",
            "synthetic-ending",
        ] {
            assert!(
                !serialized.contains(raw_only),
                "call bookkeeping {raw_only:?} reached the rendered model"
            );
        }
    }

    /// A `send-message` whose every text field is empty and a `voice-call`
    /// whose transcript is empty are measured empties (GB-5 §2): zero
    /// messages, zero counts — known-empty, not unreadable, and since the
    /// Slice B mapping landed, no longer counted as not-yet-rendered either.
    #[test]
    fn empty_send_message_and_voice_call_records_are_measured_empties() {
        let body = concat!(
            r#"{"id":"synthetic-id-1","kind":"send-message","seq":1,"timestampMs":1780000000000,"requestId":"synthetic-request-1","message":{"type":"synthetic-type","content":"","alt":"","widget":{"options":[]}}}"#,
            "\n",
            r#"{"id":"synthetic-id-2","kind":"send-message","seq":2,"timestampMs":1780000001000,"requestId":"synthetic-request-2","message":{}}"#,
            "\n",
            r#"{"id":"synthetic-id-3","kind":"send-message","seq":3,"timestampMs":1780000002000,"requestId":"synthetic-request-3"}"#,
            "\n",
            r#"{"id":"synthetic-id-4","kind":"voice-call","seq":4,"timestampMs":1780000003000,"call":{"callId":"synthetic-call-1","durationMs":0,"turnCount":0,"nudgeCount":0},"conversation":{"events":[],"toolCalls":[],"overheard":[],"turns":[],"nudges":[]}}"#,
            "\n",
            r#"{"id":"synthetic-id-5","kind":"voice-call","seq":5,"timestampMs":1780000004000}"#,
            "\n",
        );
        let conversation = normalize("grok-bot", body);
        // The provenance pins that the zeros below were *measured* by this
        // reader — against a raw-view-only build the same zeros are unread.
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
        assert_eq!(conversation.messages.len(), 0);
        assert_eq!(conversation.unrendered_lines, 0);
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
