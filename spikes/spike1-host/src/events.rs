//! Derives structured, live events from pi session-file JSONL entries.
//!
//! Pi's *stream* protocol (`pi --mode json`, see docs/json.md) defines events
//! such as `message_end` and `tool_execution_start/end`. The session *file*
//! (docs/session-format.md) instead stores tree entries: `message` entries
//! with roles (`user`/`assistant`/`toolResult`/`system`), where assistant
//! messages carry `toolCall` content blocks. This module derives
//! stream-event-shaped observations from those entries so a host that only
//! tails the session file (the file `PI_SESSION_FILE` points at) can still
//! print `message_end` / `tool_execution_start` / `tool_execution_end` live.
//! The approximation (tool_execution_start is inferred from the finalized
//! assistant message, so it lags the true start) is documented in README.md.

use serde_json::{json, Value};

const PREVIEW_MAX: usize = 200;

fn truncate_str(s: &str) -> String {
    if s.chars().count() <= PREVIEW_MAX {
        s.to_string()
    } else {
        let cut: String = s.chars().take(PREVIEW_MAX).collect();
        format!("{}…", cut)
    }
}

/// Concatenated text blocks of a message content (string or block array).
fn text_preview(content: &Value) -> String {
    let mut out = String::new();
    match content {
        Value::String(s) => out.push_str(s),
        Value::Array(blocks) => {
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
        }
        _ => {}
    }
    let cleaned: String = out.chars().filter(|&c| c != '\r').collect();
    truncate_str(&cleaned)
}

/// Map one parsed session-file line into zero or more derived events.
///
/// Unknown entry types pass through as `{"type": <entry.type>}` so the
/// observer never silently drops new formats.
pub fn entry_to_events(entry: &Value) -> Vec<Value> {
    let etype = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out = Vec::new();
    match etype {
        "session" => out.push(json!({
            "type": "session_start",
            "sessionId": entry.get("id"),
            "version": entry.get("version"),
            "cwd": entry.get("cwd"),
        })),
        "message" => {
            let msg = entry.get("message").cloned().unwrap_or(Value::Null);
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "assistant" => {
                    out.push(json!({
                        "type": "message_end",
                        "role": "assistant",
                        "model": msg.get("model"),
                        "stopReason": msg.get("stopReason"),
                        "preview": text_preview(msg.get("content").unwrap_or(&Value::Null)),
                    }));
                    if let Some(Value::Array(blocks)) = msg.get("content") {
                        for b in blocks {
                            if b.get("type").and_then(Value::as_str) == Some("toolCall") {
                                out.push(json!({
                                    "type": "tool_execution_start",
                                    "toolCallId": b.get("id"),
                                    "toolName": b.get("name"),
                                    "args": b.get("arguments"),
                                }));
                            }
                        }
                    }
                }
                "toolResult" => out.push(json!({
                    "type": "tool_execution_end",
                    "toolCallId": msg.get("toolCallId"),
                    "toolName": msg.get("toolName"),
                    "isError": msg.get("isError"),
                    "preview": text_preview(msg.get("content").unwrap_or(&Value::Null)),
                })),
                "user" => out.push(json!({
                    "type": "message_end",
                    "role": "user",
                    "preview": text_preview(msg.get("content").unwrap_or(&Value::Null)),
                })),
                "system" => {
                    let added = msg
                        .get("toolsAdded")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let removed = msg
                        .get("toolsRemoved")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    if added > 0 || removed > 0 {
                        out.push(json!({
                            "type": "system_prompt_update",
                            "toolsAdded": added,
                            "toolsRemoved": removed,
                        }));
                    }
                }
                _ => {}
            }
        }
        "compaction" => out.push(json!({
            "type": "compaction",
            "tokensBefore": entry.get("tokensBefore"),
        })),
        "model_change" => out.push(json!({
            "type": "model_change",
            "provider": entry.get("provider"),
            "modelId": entry.get("modelId"),
        })),
        other if !other.is_empty() => out.push(json!({ "type": other })),
        _ => {}
    }
    out
}

/// Parse one raw JSONL line; malformed lines yield no events (tolerated).
pub fn parse_line(line: &str) -> Option<Value> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str(t).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_maps_to_session_start() {
        let v = parse_line(
            r#"{"type":"session","version":3,"id":"uuid-1","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/tmp"}"#,
        )
        .unwrap();
        let evs = entry_to_events(&v);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "session_start");
        assert_eq!(evs[0]["sessionId"], "uuid-1");
        assert_eq!(evs[0]["version"], 3);
    }

    #[test]
    fn user_message_maps_to_message_end() {
        let v = parse_line(
            r#"{"type":"message","id":"a1","parentId":null,"timestamp":"t","message":{"role":"user","content":"Hello pi","timestamp":1}}"#,
        )
        .unwrap();
        let evs = entry_to_events(&v);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "message_end");
        assert_eq!(evs[0]["role"], "user");
        assert_eq!(evs[0]["preview"], "Hello pi");
    }

    #[test]
    fn assistant_with_toolcall_yields_message_end_and_start() {
        let line = r#"{"type":"message","id":"b2","parentId":"a1","timestamp":"t","message":{"role":"assistant","content":[{"type":"text","text":"I will run it."},{"type":"toolCall","id":"call_123","name":"bash","arguments":{"command":"ls"}}],"api":"x","provider":"zai","model":"glm-5.3","stopReason":"toolUse"}}"#;
        let v = parse_line(line).unwrap();
        let evs = entry_to_events(&v);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["type"], "message_end");
        assert_eq!(evs[0]["role"], "assistant");
        assert_eq!(evs[0]["model"], "glm-5.3");
        assert_eq!(evs[0]["preview"], "I will run it.");
        assert_eq!(evs[1]["type"], "tool_execution_start");
        assert_eq!(evs[1]["toolCallId"], "call_123");
        assert_eq!(evs[1]["toolName"], "bash");
        assert_eq!(evs[1]["args"]["command"], "ls");
    }

    #[test]
    fn toolresult_maps_to_tool_execution_end() {
        let line = r#"{"type":"message","id":"c3","parentId":"b2","timestamp":"t","message":{"role":"toolResult","toolCallId":"call_123","toolName":"bash","content":[{"type":"text","text":"file1\nfile2"}],"isError":false}}"#;
        let v = parse_line(line).unwrap();
        let evs = entry_to_events(&v);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "tool_execution_end");
        assert_eq!(evs[0]["toolCallId"], "call_123");
        assert_eq!(evs[0]["toolName"], "bash");
        assert_eq!(evs[0]["isError"], false);
        assert_eq!(evs[0]["preview"], "file1\nfile2");
    }

    #[test]
    fn blocked_tool_result_carries_error_flag_and_reason_preview() {
        let line = r#"{"type":"message","id":"c4","parentId":"b2","timestamp":"t","message":{"role":"toolResult","toolCallId":"call_9","toolName":"bash","content":[{"type":"text","text":"[spike1-gate] blocked: command contains 'spike1-deny-me'"}],"isError":true}}"#;
        let v = parse_line(line).unwrap();
        let evs = entry_to_events(&v);
        assert_eq!(evs[0]["isError"], true);
        assert!(evs[0]["preview"].as_str().unwrap().contains("spike1-deny-me"));
    }

    #[test]
    fn unknown_entry_type_passes_through() {
        let v = parse_line(r#"{"type":"some_future_entry","id":"z","timestamp":"t"}"#).unwrap();
        let evs = entry_to_events(&v);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "some_future_entry");
    }

    #[test]
    fn malformed_line_is_none_and_system_message_without_tools_is_skipped() {
        assert!(parse_line("{not json").is_none());
        assert!(parse_line("").is_none());
        let v = parse_line(
            r#"{"type":"message","id":"s1","timestamp":"t","message":{"role":"system","content":"","sections":{"preamble":"x"},"timestamp":1}}"#,
        )
        .unwrap();
        assert!(entry_to_events(&v).is_empty());
    }

    #[test]
    fn preview_truncates_long_text() {
        let long = "x".repeat(1000);
        let v = parse_line(&format!(
            r#"{{"type":"message","id":"u1","timestamp":"t","message":{{"role":"user","content":"{}"}}}}"#,
            long
        ))
        .unwrap();
        let evs = entry_to_events(&v);
        let p = evs[0]["preview"].as_str().unwrap();
        assert!(p.chars().count() <= 201);
        assert!(p.ends_with('…'));
    }
}
