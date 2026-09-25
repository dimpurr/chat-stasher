//! gemini-cli — reader for one archived Gemini CLI session document.
//!
//! One archived body holds one or more pretty-printed JSON documents run
//! together (a session plus export mirrors); the framing that turns the body
//! into per-document values lives in [`super::normalize`], which calls
//! [`document`] once per document it read. Each document is
//! `{"startTime", "lastUpdated", "messages":[{"type":"user"|"gemini", …}]}`
//! (`activity.rs` reads the same fields for the session span).
//!
//! Only `messages` carries the conversation; a document without one is
//! counted, not skipped, so a document that holds nothing readable stays
//! visible as a number.

use super::{blocks_from_content, message_time, Conversation, Message, Role};
use serde_json::Value;

pub(super) fn document(value: &Value, conversation: &mut Conversation) {
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for item in messages {
        let role = match item.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("gemini") | Some("assistant") => Role::Assistant,
            _ => {
                conversation.unrendered_lines += 1;
                continue;
            }
        };
        let content = item.get("content").unwrap_or(item);
        let blocks = blocks_from_content(content, conversation);
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            time: message_time(item.get("timestamp")),
            blocks,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Role};

    #[test]
    fn gemini_fixture_produces_roles() {
        let body = r#"{"messages":[{"type":"user","timestamp":"2026-09-25T10:00:00Z","content":[{"text":"hi"}]},{"type":"gemini","content":[{"text":"hello"}]}]}"#;
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(result.messages[1].role, Role::Assistant));
    }

    /// `gemini-cli` does not archive JSONL: its body is pretty-printed JSON
    /// documents run together, so line-by-line parsing reads nothing at all.
    /// The body has to be re-read as the stream of documents it is, and the
    /// lines must not then be reported as unreadable content.
    #[test]
    fn a_gemini_cli_document_is_read_as_documents_not_as_lines() {
        let body = concat!(
            "{\n",
            "  \"sessionId\": \"s1\",\n",
            "  \"messages\": [\n",
            "    {\"id\": \"m1\", \"timestamp\": \"2026-09-25T10:00:00Z\",\n",
            "     \"type\": \"user\", \"content\": [{\"text\": \"hi\"}]},\n",
            "    {\"id\": \"m2\", \"type\": \"gemini\", \"content\": [{\"text\": \"hello\"}]}\n",
            "  ],\n",
            "  \"kind\": \"main\"\n",
            "}\n",
            // A second document in the same body, which is what the archive
            // actually holds: the walk above has to reach it too.
            "{\n",
            "  \"sessionId\": \"s1\",\n",
            "  \"messages\": [\n",
            "    {\"id\": \"m3\", \"type\": \"user\", \"content\": [{\"text\": \"again\"}]}\n",
            "  ],\n",
            "  \"kind\": \"summary\"\n",
            "}\n",
        );
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 3, "both documents must be read");
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        assert!(matches!(result.messages[2].role, Role::User));
        assert_eq!(
            result.unrecognized_lines, 0,
            "the framing is ours to solve, not the body's defect"
        );
        assert_eq!(
            result.unrendered_lines, 0,
            "no record in either document went unrendered"
        );
    }

    /// A body that reads as documents but yields no message must not report
    /// the lines as invalid JSON: the framing is solved and the residue is a
    /// document with nothing readable in it, which is the honest thing to
    /// count.
    #[test]
    fn a_readable_gemini_cli_body_with_no_message_does_not_blame_the_json() {
        let body = "{\n  \"sessionId\": \"s1\",\n  \"kind\": \"main\"\n}\n";
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(
            result.unrecognized_lines, 0,
            "the body is valid JSON; it simply holds no message"
        );
        assert_eq!(
            result.unrendered_lines, 1,
            "one document was framed and held no message"
        );
    }

    /// A body whose lines do not parse and whose document stream does not read
    /// either keeps the line counts: they are the only measurement there is,
    /// and replacing them with a clean zero would turn "unreadable" into
    /// "empty".
    #[test]
    fn an_unreadable_gemini_cli_body_keeps_its_counts() {
        // A document, then something that is not part of any document.
        let body = "{\n  \"nothing\": [\n    1, 2\n  ]\n}\nnot json at all\n";
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(
            result.unrecognized_lines, 6,
            "every line of an unreadable body stays counted as unreadable"
        );
        assert_eq!(result.unrendered_lines, 0);
    }
}
