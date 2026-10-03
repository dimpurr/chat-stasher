//! OpenClaw transcript events. The archived source envelope keeps every raw
//! event (including model/provider/usage metadata); this reader exposes the
//! ordinary message text and counts other event records honestly.

use super::{message_time, Conversation, Message, Role};
use serde_json::Value;

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    let Some(events) = value.get("events").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for row in events {
        let Some(event) = row.get("event") else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let Some(message) = event.get("message") else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let Some(role) = message
            .get("role")
            .and_then(Value::as_str)
            .and_then(Role::from_str)
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let mut blocks = Vec::new();
        match message.get("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                blocks.push(super::Block::Text(text.clone()))
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        blocks.push(super::Block::Text(text.to_string()));
                    } else {
                        conversation.unrendered_lines += 1;
                    }
                }
            }
            _ => {}
        }
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            time: message_time(message.get("timestamp").or_else(|| row.get("created_at"))),
            blocks,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Role};

    #[test]
    fn openclaw_message_raw_audit_fields_survive_export_and_rendering() {
        let body = r#"{"schema":"chat-stasher.openclaw.session.v1","window":{"session_id":"synthetic-window"},"events":[{"seq":5,"event":{"id":"synthetic-event","type":"message","message":{"role":"assistant","content":[{"type":"text","text":"synthetic answer"}],"provider":"synthetic-provider","model":"synthetic-model","usage":{"input":11,"output":13,"cacheRead":17,"cacheWrite":19,"totalTokens":60,"cost":{"total":0.25}}}}}]}"#;
        let result = normalize("openclaw", body);
        assert_eq!(result.messages.len(), 1);
        assert!(matches!(result.messages[0].role, Role::Assistant));
        assert_eq!(
            result.messages[0].blocks[0],
            super::super::Block::Text("synthetic answer".to_string())
        );
        let raw: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            raw["events"][0]["event"]["message"]["provider"],
            "synthetic-provider"
        );
        assert_eq!(
            raw["events"][0]["event"]["message"]["usage"]["cacheWrite"],
            19
        );
    }
}
