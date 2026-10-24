//! Strata park hint for a Codex Responses request.
//!
//! Strata only sees the rendered token sequence. A Codex compaction prompt does
//! not continue the live tokens, so it looks like a different chat and would
//! take another park slot. A subagent is a real thread of its own: it keeps one
//! slot so the user can switch back to it. This module reads the Codex turn
//! metadata while it still exists and writes two fields onto the JSON body:
//!
//! - `strata_conversation`: the thread id. Strata tags the parked snapshot with
//!   it and drops that snapshot when a later real turn of the same thread does
//!   not extend it (the post-compact transcript, or a rewritten subagent).
//! - `strata_checkpoint`: `false` on a side request (compaction, prewarm,
//!   memory, title). Strata's existing `ckpt=0` then refuses to park that
//!   request when the next one arrives. The real conversation is still parked
//!   on the way out. A subagent turn is not a side request.
//!
//! The fields are added only when the upstream host is loopback or a private
//! IP. A public gateway that rejects unknown JSON keys must not see them.

use serde_json::{json, Value};
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StrataParkHint {
    pub conversation: Option<String>,
    pub ephemeral: bool,
}

pub(crate) fn hint(body: &Value, turn_metadata_header: Option<&str>) -> StrataParkHint {
    let meta = turn_metadata(body, turn_metadata_header);
    let kind = text_field(&meta, "request_kind");
    let ephemeral = is_ephemeral(body, &meta, kind.as_deref());
    let conversation = text_field(&meta, "thread_id")
        .or_else(|| {
            body.get("client_metadata")
                .and_then(|m| text_field(m, "thread_id"))
        })
        .or_else(|| {
            body.get("client_metadata")
                .and_then(|m| text_field(m, "session_id"))
        });
    StrataParkHint {
        conversation,
        ephemeral,
    }
}

/// Write the hint onto the outbound JSON. Returns false when this upstream is
/// not Strata or the hint carries nothing.
pub(crate) fn apply(base_url: &str, body: &mut Value, hint: &StrataParkHint) -> bool {
    if !strata_upstream(base_url) {
        return false;
    }
    if hint.conversation.is_none() && !hint.ephemeral {
        return false;
    }
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    if let Some(id) = &hint.conversation {
        obj.insert("strata_conversation".to_string(), json!(id));
    }
    if hint.ephemeral {
        obj.insert("strata_checkpoint".to_string(), json!(false));
    }
    true
}

fn is_strata_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

pub(crate) fn strata_upstream(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .map(is_strata_ip)
            .unwrap_or(false),
        None => false,
    }
}

fn is_ephemeral(body: &Value, meta: &Value, kind: Option<&str>) -> bool {
    if matches!(kind, Some("compaction" | "prewarm" | "memory")) {
        return true;
    }
    if text_field(meta, "thread_source").as_deref() == Some("thread_title") {
        return true;
    }
    super::codex_compaction::is_compaction_request(body)
}

fn turn_metadata(body: &Value, header: Option<&str>) -> Value {
    let from_body = body
        .get("client_metadata")
        .and_then(|meta| meta.get("x-codex-turn-metadata"));
    if let Some(text) = from_body.and_then(Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if parsed.is_object() {
                return parsed;
            }
        }
    }
    if let Some(Value::Object(map)) = from_body {
        return Value::Object(map.clone());
    }
    if let Some(text) = header {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if parsed.is_object() {
                return parsed;
            }
        }
    }
    Value::Null
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    let raw = value.get(key)?.as_str()?.trim();
    sanitize_id(raw)
}

fn sanitize_id(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 128 {
        return None;
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return None;
    }
    Some(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(kind: &str, thread: &str) -> Value {
        json!({
            "client_metadata": {
                "x-codex-turn-metadata": json!({
                    "request_kind": kind,
                    "thread_id": thread,
                    "session_id": "sess-1"
                }).to_string()
            }
        })
    }

    #[test]
    fn a_normal_turn_keeps_the_thread_and_is_not_ephemeral() {
        let hint = hint(&meta("turn", "thread-a"), None);
        assert_eq!(hint.conversation.as_deref(), Some("thread-a"));
        assert!(!hint.ephemeral);
    }

    #[test]
    fn compaction_prewarm_and_memory_are_ephemeral() {
        for kind in ["compaction", "prewarm", "memory"] {
            assert!(hint(&meta(kind, "thread-a"), None).ephemeral, "{kind}");
        }
    }

    #[test]
    fn a_subagent_turn_keeps_its_own_thread_and_is_not_ephemeral() {
        let child = json!({
            "client_metadata": {
                "x-codex-turn-metadata": json!({
                    "request_kind": "turn",
                    "thread_id": "child-1",
                    "parent_thread_id": "thread-a",
                    "subagent_kind": "review"
                }).to_string()
            }
        });
        let hint = hint(&child, None);
        assert!(!hint.ephemeral);
        assert_eq!(hint.conversation.as_deref(), Some("child-1"));
    }

    #[test]
    fn compaction_trigger_is_ephemeral_without_turn_metadata() {
        let body = json!({"input": [{"type": "message"}, {"type": "compaction_trigger"}]});
        let hint = hint(&body, None);
        assert!(hint.ephemeral);
        assert!(hint.conversation.is_none());
    }

    #[test]
    fn the_header_is_used_when_the_body_has_no_metadata() {
        let header = r#"{"request_kind":"turn","thread_id":"from-header"}"#;
        let hint = hint(&json!({}), Some(header));
        assert_eq!(hint.conversation.as_deref(), Some("from-header"));
        assert!(!hint.ephemeral);
    }

    #[test]
    fn apply_writes_the_fields_only_for_a_private_upstream() {
        let hint = StrataParkHint {
            conversation: Some("thread-a".to_string()),
            ephemeral: true,
        };
        let mut body = json!({"model": "qwen"});
        assert!(apply("http://10.99.0.106:8005/v1", &mut body, &hint));
        assert_eq!(body["strata_conversation"], "thread-a");
        assert_eq!(body["strata_checkpoint"], false);

        let mut public = json!({});
        assert!(!apply("https://api.openai.com/v1", &mut public, &hint));
        assert!(public.get("strata_checkpoint").is_none());

        let plain = StrataParkHint {
            conversation: Some("thread-a".to_string()),
            ephemeral: false,
        };
        let mut body = json!({});
        assert!(apply("http://127.0.0.1:8005/v1", &mut body, &plain));
        assert!(body.get("strata_checkpoint").is_none());
        assert_eq!(body["strata_conversation"], "thread-a");
    }
}
