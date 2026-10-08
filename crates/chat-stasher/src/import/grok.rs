//! The Grok official-export parser — the Grok leg of TKO-4's order.
//!
//! The measured shape (W909 §2, census of the 2026-09-24 export:
//! 312 conversations, 2,397 responses) is one JSON **object** — not
//! the flat array a Claude `conversations.json` is — whose
//! `conversations` key holds a list of wrappers, with `projects`,
//! `tasks` and `media_posts` present but empty:
//!
//! ```json
//! {
//!   "conversations": [
//!     {
//!       "conversation": { "id": "…", "create_time": "…", "modify_time": "…" },
//!       "responses": [
//!         {
//!           "response": {
//!             "_id": "…",
//!             "create_time": { "$date": { "$numberLong": "…" } }
//!           },
//!           "share_link": null
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! The parser rules the measurement fixed, and this module honours:
//!
//! * **The raw `responses` list is the complete node set and the only
//!   measurement axis.** `conversation.leaf_response_id` (present on
//!   65/312) is the active-branch tip and is not necessarily the
//!   deepest branch; walking it invented false shorts in the oracle's
//!   compare (27-ORACLE §4.4). The parser keeps the whole list and
//!   walks nothing.
//! * **Times are carried, never derived and never rebuilt.** The
//!   conversation's `create_time`/`modify_time` are ISO-8601 strings;
//!   a response's `create_time` is a MongoDB extended-JSON `$date`
//!   dict whose magnitude is epoch **milliseconds** (the DeepSeek
//!   s-vs-ms hazard: divide by 1000, never read as seconds). The
//!   export's own `conversation.create_time` is the created time —
//!   the minimum response time equals it on 0/38 measured
//!   conversations, so approximating it from the responses would be
//!   wrong every time (W909 §2.4). Nothing here parses, reformats or
//!   validates a time: a value of any shape is the value the record
//!   keeps.
//! * **A missing parent is a root, never an error** (62 measured
//!   responses carry a `parent_response_id` outside their
//!   conversation's response set).
//! * **Attachments are references, not bytes.** `file_attachments`
//!   entries are asset-reference strings; the export carries no
//!   attachment binaries, so the record keeps the references and
//!   promises nothing about the bytes.
//!
//! The record is the seam's own [`ExportConversation`]: the whole
//! wrapper, field for field (ADR-052 D6 — no projection at capture),
//! so every field the census marked *not interpreted* survives in the
//! record exactly as the platform wrote it — including its absences,
//! which stay absent rather than becoming empty lists or zeros.
//!
//! Read-only on the source (ADR-011): the parser takes bytes and
//! never touches the file they came from.

use super::{conversation_id_is_safe, ExportConversation, ImportError, ParsedExport};
use std::collections::BTreeSet;

/// Parse a Grok export's `prod-grok-backend.json` payload.
///
/// The payload is a dict with a `conversations` list (W909 §2.1), so
/// a Claude `conversations.json` — a flat array — is the wrong input
/// and is refused by shape. A Claude export *manifest* is refused by
/// name for the same reason the Claude parser refuses it: its
/// `export_url` values are one-time-use credentials that must never
/// be echoed, and a file this producer refuses has no business being
/// archived byte-exact either.
///
/// JSON that stops part-way is a read that never finished; JSON that
/// was read to the end and is not valid is a completed read of
/// invalid input. The two are different refusals, and the exit code
/// says which one happened.
pub fn parse_grok_export(bytes: &[u8]) -> Result<ParsedExport, ImportError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        if e.is_eof() {
            // The JSON stopped part-way: the file was cut off
            // before a value completed, so the read never
            // finished and nothing about the contents is known.
            ImportError::ReadIncomplete(format!(
                "export did not parse as JSON and was not read to the end: {e}"
            ))
        } else {
            // The whole file was read and is not valid JSON: a
            // completed read of invalid input, which is a
            // different "no" from an unfinished read, and the
            // exit code must not claim the read was incomplete.
            ImportError::WrongInput(format!(
                "export was read to the end but is not valid JSON: {e}"
            ))
        }
    })?;

    if let Some(files) = value.get("data_files") {
        let count = files.as_array().map_or(0, Vec::len);
        // reason: a manifest whose category list we could not count is an unknown
        // denominator, and reporting zero categories would claim we read them all.
        return Err(ImportError::WrongInput(format!(
            "this is an export manifest ({count} data files listed), not a Grok \
             export payload. A Grok export's payload is the `prod-grok-backend.json` \
             object; the manifest's download links are one-time-use credentials and \
             are never read further."
        )));
    }

    let root = value.as_object().ok_or_else(|| {
        ImportError::WrongInput(
            "a Grok export is one JSON object with a `conversations` key; this file \
             is not an object"
                .to_string(),
        )
    })?;

    let Some(items) = root.get("conversations") else {
        return Err(ImportError::WrongInput(
            "a Grok export carries its conversations under a `conversations` key; \
             this file has none"
                .to_string(),
        ));
    };
    let items = items.as_array().ok_or_else(|| {
        ImportError::WrongInput(
            "the `conversations` key of a Grok export is a list of conversation \
             wrappers; this file's is not"
                .to_string(),
        )
    })?;

    let mut parsed = ParsedExport {
        conversations: Vec::with_capacity(items.len()),
        rejections: Vec::new(),
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let Some(wrapper) = item.as_object() else {
            parsed
                .rejections
                .push(format!("entry {index}: not a conversation wrapper object"));
            continue;
        };
        let Some(conversation) = wrapper.get("conversation") else {
            parsed
                .rejections
                .push(format!("entry {index}: no conversation object"));
            continue;
        };
        let Some(fields) = conversation.as_object() else {
            parsed.rejections.push(format!(
                "entry {index}: conversation is not a readable object"
            ));
            continue;
        };
        let raw_id = fields.get("id").and_then(|v| v.as_str());
        let Some(id) = raw_id.map(str::trim) else {
            // An absent id is not an empty id: say which of the two it was.
            parsed.rejections.push(format!(
                "entry {index}: {}",
                if fields.contains_key("id") {
                    "conversation id is not a readable string"
                } else {
                    "no conversation id"
                }
            ));
            continue;
        };
        if !conversation_id_is_safe(id) {
            parsed
                .rejections
                .push(format!("entry {index}: conversation id is unsafe to carry"));
            continue;
        }
        if !seen.insert(id.to_string()) {
            // Two wrappers under one id would be two bodies for one session id,
            // and which one the reader would see is decided by shard order.
            parsed.rejections.push(format!(
                "entry {index}: conversation id repeats within this export"
            ));
            continue;
        }
        parsed.conversations.push(ExportConversation {
            conversation_id: id.to_string(),
            object: item.clone(),
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    /// A one-time download link, planted so its absence is a fact we
    /// checked rather than a property we hoped for. Not a real
    /// credential format, not reachable.
    const PLANTED_URL: &str = "https://example.invalid/export/one-time-use/SECRETTOKEN1234567890";

    /// A tiny fake `prod-grok-backend.json`: the dict of W909 §2.1,
    /// one wrapper per id, text nobody wrote by hand.
    fn synthetic_grok_export(ids: &[&str]) -> String {
        let conversations: Vec<serde_json::Value> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                serde_json::json!({
                    "conversation": {
                        "id": id,
                        "user_id": format!("synthetic-user-{i}"),
                        "create_time": "2026-09-01T00:00:00.000000Z",
                        "modify_time": "2026-09-02T00:00:00.000Z",
                        "leaf_response_id": format!("{id}-r1"),
                    },
                    "responses": [
                        {
                            "response": {
                                "_id": format!("{id}-r0"),
                                "conversation_id": id,
                                "parent_response_id": null,
                                "create_time": { "$date": { "$numberLong": "1756684800000" } },
                                "sender": "human",
                            },
                            "share_link": null,
                        },
                        {
                            "response": {
                                "_id": format!("{id}-r1"),
                                "conversation_id": id,
                                "parent_response_id": format!("{id}-r0"),
                                // The case inconsistency the census measured
                                // (W909 §2.6): kept as written, not normalised.
                                "sender": "ASSISTANT",
                                "create_time": { "$date": { "$numberLong": "1756684860000" } },
                                "file_attachments": [format!("synthetic-asset-uuid-{i}")],
                            },
                            "share_link": null,
                        },
                        {
                            "response": {
                                "_id": format!("{id}-r2"),
                                "conversation_id": id,
                                "parent_response_id": format!("{id}-r1"),
                                // Deeper than the leaf the conversation names:
                                // the raw list must keep it anyway.
                                "create_time": { "$date": { "$numberLong": "1756684920000" } },
                                "sender": "human",
                            },
                            "share_link": null,
                        },
                    ],
                })
            })
            .collect();
        serde_json::json!({
            "conversations": conversations,
            "projects": [],
            "tasks": [],
            "media_posts": [],
        })
        .to_string()
    }

    fn one_wrapper(object: serde_json::Value) -> String {
        serde_json::json!({
            "conversations": [object],
            "projects": [],
            "tasks": [],
            "media_posts": [],
        })
        .to_string()
    }

    /// A wrapper whose conversation carries the given id, with the
    /// measured minimum shape around it.
    fn wrapper_with_id(id: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "conversation": { "id": id, "create_time": "2026-09-01T00:00:00.000000Z" },
            "responses": [],
        })
    }

    // ---------------------------------------------------------------- the parser

    #[test]
    fn parses_the_wrapper_list_and_keeps_every_field() {
        let export = synthetic_grok_export(&[ID_A, ID_B]);
        let parsed = parse_grok_export(export.as_bytes()).unwrap();
        assert_eq!(parsed.conversations.len(), 2);
        assert!(parsed.rejections.is_empty());
        assert_eq!(parsed.conversations[0].conversation_id, ID_A);
        assert_eq!(parsed.conversations[1].conversation_id, ID_B);
        // Field-for-field: the parser projects nothing.
        let object = &parsed.conversations[0].object;
        assert_eq!(object["conversation"]["user_id"], "synthetic-user-0");
        assert_eq!(
            object["conversation"]["create_time"],
            "2026-09-01T00:00:00.000000Z"
        );
        assert_eq!(object["responses"].as_array().unwrap().len(), 3);
        assert!(object.get("conversation").is_some());
    }

    /// The raw `responses` list is the complete node set: a response
    /// deeper than the named leaf is still in the record, because the
    /// list — not `leaf_response_id` — is the measurement axis.
    #[test]
    fn the_raw_response_list_is_kept_whole_not_walked_from_the_leaf() {
        let export = synthetic_grok_export(&[ID_A]);
        let parsed = parse_grok_export(export.as_bytes()).unwrap();
        let object = &parsed.conversations[0].object;
        assert_eq!(
            object["conversation"]["leaf_response_id"],
            format!("{ID_A}-r1")
        );
        let responses = object["responses"].as_array().unwrap();
        assert_eq!(responses.len(), 3, "the off-leaf response must survive");
        assert_eq!(responses[2]["response"]["_id"], format!("{ID_A}-r2"));
        assert_eq!(
            responses[2]["response"]["parent_response_id"],
            format!("{ID_A}-r1")
        );
    }

    /// An empty message list is a measurement, not a refusal and not
    /// an unknown: the key was present and empty, and the record says
    /// exactly that.
    #[test]
    fn an_empty_message_list_is_a_measurement_not_a_refusal() {
        let export = one_wrapper(wrapper_with_id(serde_json::Value::String(ID_A.to_string())));
        let parsed = parse_grok_export(export.as_bytes()).unwrap();
        assert_eq!(parsed.conversations.len(), 1);
        assert!(parsed.rejections.is_empty());
        let responses = parsed.conversations[0].object["responses"]
            .as_array()
            .unwrap();
        assert!(responses.is_empty());
    }

    /// Invariant 1 on the capture axis: a wrapper with no `responses`
    /// key keeps the absence — the record does not grow an empty list
    /// the platform never wrote.
    #[test]
    fn a_wrapper_without_a_responses_key_keeps_the_absence() {
        let wrapper = serde_json::json!({
            "conversation": { "id": ID_A, "create_time": "2026-09-01T00:00:00.000000Z" },
        });
        let export = one_wrapper(wrapper);
        let parsed = parse_grok_export(export.as_bytes()).unwrap();
        assert_eq!(parsed.conversations.len(), 1);
        assert!(
            parsed.conversations[0].object.get("responses").is_none(),
            "an unobserved message list stays absent, never empty"
        );
    }

    /// The response-level time is a `$date` dict of epoch milliseconds
    /// in the measured export. A row whose time is a string instead is
    /// carried verbatim: the parser neither converts it, rejects it,
    /// nor rebuilds a time from it.
    #[test]
    fn a_row_whose_time_is_a_string_is_carried_verbatim() {
        let wrapper = serde_json::json!({
            "conversation": { "id": ID_A, "create_time": "2026-09-01T00:00:00.000000Z" },
            "responses": [
                {
                    "response": {
                        "_id": format!("{ID_A}-r0"),
                        "create_time": "2026-09-01T00:00:00.000Z",
                        "sender": "human",
                    },
                    "share_link": null,
                }
            ],
        });
        let parsed = parse_grok_export(one_wrapper(wrapper).as_bytes()).unwrap();
        assert_eq!(parsed.conversations.len(), 1);
        assert!(parsed.rejections.is_empty());
        let response = &parsed.conversations[0].object["responses"][0]["response"];
        assert_eq!(
            response["create_time"], "2026-09-01T00:00:00.000Z",
            "a string time is the value the platform wrote, kept as written"
        );
    }

    /// Attachment entries are asset references, and the record keeps
    /// them as the references they are.
    #[test]
    fn attachment_references_are_kept_as_references() {
        let export = synthetic_grok_export(&[ID_A]);
        let parsed = parse_grok_export(export.as_bytes()).unwrap();
        let response = &parsed.conversations[0].object["responses"][1]["response"];
        assert_eq!(response["file_attachments"][0], "synthetic-asset-uuid-0");
        // And the absence on every other response stays absent too.
        let other = &parsed.conversations[0].object["responses"][0]["response"];
        assert!(other.get("file_attachments").is_none());
    }

    /// The id ladder, as named refusals: a row without a readable,
    /// safe, unique id is refused with the reason stated, and nothing
    /// is emitted for it. Absent and unreadable are different answers.
    #[test]
    fn rows_without_a_readable_id_are_refused_not_sanitised() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (
                serde_json::json!({ "responses": [] }),
                "no conversation object",
            ),
            (
                serde_json::json!({ "conversation": "not-an-object", "responses": [] }),
                "conversation is not a readable object",
            ),
            (
                serde_json::json!({ "conversation": { "create_time": "2026-09-01T00:00:00.000000Z" }, "responses": [] }),
                "no conversation id",
            ),
            (
                serde_json::json!({ "conversation": { "id": 12, "responses": [] } }),
                "conversation id is not a readable string",
            ),
            (
                serde_json::json!({ "conversation": { "id": "../escape", "responses": [] } }),
                "conversation id is unsafe to carry",
            ),
            (
                serde_json::json!({ "conversation": { "id": "", "responses": [] } }),
                "conversation id is unsafe to carry",
            ),
            (
                // An oversized id cannot become a path component without
                // rewriting it, so it is refused rather than truncated.
                serde_json::json!({ "conversation": { "id": "a".repeat(400), "responses": [] } }),
                "conversation id is unsafe to carry",
            ),
        ];
        for (wrapper, expected_reason) in cases {
            let bytes = one_wrapper(wrapper);
            let parsed = parse_grok_export(bytes.as_bytes()).unwrap();
            assert!(
                parsed.conversations.is_empty(),
                "{expected_reason}: nothing is emitted for a refused row"
            );
            assert_eq!(parsed.rejections.len(), 1, "{expected_reason}");
            assert!(
                parsed.rejections[0].contains(expected_reason),
                "{expected_reason}: got {}",
                parsed.rejections[0]
            );
        }
    }

    #[test]
    fn a_repeated_id_in_one_export_is_refused_once_emitted_once() {
        let bytes = serde_json::json!({
            "conversations": [
                wrapper_with_id(serde_json::Value::String(ID_A.to_string())),
                wrapper_with_id(serde_json::Value::String(ID_A.to_string())),
            ],
            "projects": [],
            "tasks": [],
            "media_posts": [],
        })
        .to_string();
        let parsed = parse_grok_export(bytes.as_bytes()).unwrap();
        assert_eq!(parsed.conversations.len(), 1);
        assert_eq!(parsed.rejections.len(), 1);
        assert!(
            parsed.rejections[0].contains("repeats"),
            "{}",
            parsed.rejections[0]
        );
    }

    /// A file that stops mid-JSON was never read to the end. That is
    /// a read failure, not a short answer.
    #[test]
    fn a_torn_export_is_a_read_failure_not_a_short_answer() {
        let export = synthetic_grok_export(&[ID_A, ID_B]);
        let cut = &export[..export.len() - 20];
        let err = parse_grok_export(cut.as_bytes()).unwrap_err();
        assert!(matches!(err, ImportError::ReadIncomplete(_)), "{err}");
    }

    /// A file that was read to the end and is not valid JSON is a
    /// completed read of invalid input, not an unfinished read: the
    /// exit code must not claim the read was incomplete.
    #[test]
    fn a_complete_but_malformed_export_is_wrong_input_not_a_failed_read() {
        let full = synthetic_grok_export(&[ID_A]);
        // Garbage after a complete document: the whole file was read,
        // and what it holds is not the payload this parser imports.
        let malformed = format!("{full} {{");
        let err = parse_grok_export(malformed.as_bytes()).unwrap_err();
        let ImportError::WrongInput(message) = err else {
            panic!(
                "a fully read malformed document is wrong input, not a \
                 failed read: {err}"
            );
        };
        assert!(message.contains("read to the end"), "{message}");
    }

    /// The Grok payload is a dict, not Claude's flat array: a file of
    /// the wrong platform's shape is refused by shape, not parsed.
    #[test]
    fn a_flat_array_is_the_wrong_shape_for_a_grok_export() {
        let claude_shaped = format!("[{{\"uuid\":\"{ID_A}\"}}]");
        let err = parse_grok_export(claude_shaped.as_bytes()).unwrap_err();
        let ImportError::WrongInput(message) = err else {
            panic!("a flat array is a wrong input, not a failed read: {err}");
        };
        assert!(message.contains("not an object"), "{message}");
    }

    #[test]
    fn a_payload_without_a_conversations_key_is_refused_by_shape() {
        let no_key = parse_grok_export(br#"{"projects":[]}"#).unwrap_err();
        let ImportError::WrongInput(message) = no_key else {
            panic!("a dict without `conversations` is a wrong input: {no_key}");
        };
        assert!(message.contains("`conversations`"), "{message}");
        assert!(message.contains("has none"), "{message}");

        let not_a_list =
            parse_grok_export(br#"{"conversations":{"not":"a list"}}"#).unwrap_err();
        let ImportError::WrongInput(message) = not_a_list else {
            panic!("`conversations` that is not a list is a wrong input: {not_a_list}");
        };
        assert!(
            message.contains("list of conversation wrappers"),
            "{message}"
        );
    }

    /// A Claude manifest is refused by name whatever platform it is
    /// offered to, and the refusal must not become a channel for
    /// echoing a one-time-use download credential.
    #[test]
    fn a_manifest_is_refused_by_name_and_its_urls_are_not_read() {
        let manifest = serde_json::json!({
            "version": "1.0",
            "total_files": 6,
            "data_files": [
                { "category": "conversations", "part": 0, "filename": "conversations-000.zip",
                  "export_url": PLANTED_URL }
            ]
        })
        .to_string();
        let err = parse_grok_export(manifest.as_bytes()).unwrap_err();
        let ImportError::WrongInput(message) = err else {
            panic!("a manifest is a wrong input, not a failed read: {err}");
        };
        assert!(message.contains("manifest"), "{message}");
        assert!(
            !message.contains("SECRETTOKEN1234567890"),
            "the refusal echoed a one-time-use download link"
        );
    }
}
