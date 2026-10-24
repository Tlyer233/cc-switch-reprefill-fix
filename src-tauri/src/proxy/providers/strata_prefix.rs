//! Keep a Codex thread's rendered prefix stable across turns.
//!
//! Strata reuses KV only when the new token sequence extends the previous one.
//! Codex rewrites the clock and the tool-list order on later turns of the same
//! thread, so the text is a few tokens longer but matches nothing from the
//! start. This remembers the messages and tools actually sent for a thread and
//! sends that prefix again, appending only the new tail. A real edit or a
//! compaction replaces history from the first changed message.

use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

const OPEN: &str = "<codex_apps_client_time_context>";
const CLOSE: &str = "</codex_apps_client_time_context>";
const MAX_THREADS: usize = 16;

struct Snap {
    items: Vec<Value>,
    tools: Option<Value>,
}

struct Store {
    order: VecDeque<String>,
    snaps: HashMap<String, Snap>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| {
    Mutex::new(Store {
        order: VecDeque::new(),
        snaps: HashMap::new(),
    })
});

/// Rewrite `body` so this thread extends the prefix we sent last time.
/// Returns false when the upstream is not Strata or there is no history to pin.
pub(crate) fn stabilize(base_url: &str, body: &mut Value, conversation: &str) -> bool {
    if !super::strata_park::strata_upstream(base_url) || conversation.is_empty() {
        return false;
    }
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    if let Some(text) = obj.get("instructions").and_then(Value::as_str) {
        let stripped = strip_time(text);
        if stripped != text {
            obj.insert("instructions".to_string(), Value::String(stripped));
        }
    }
    let history_key = if obj.get("messages").and_then(Value::as_array).is_some() {
        "messages"
    } else if obj.get("input").and_then(Value::as_array).is_some() {
        "input"
    } else {
        return false;
    };
    let fresh_items = canonical_items(obj.get(history_key).and_then(Value::as_array));
    let fresh_tools = obj.get("tools").map(canonical_tools);

    let mut store = STORE.lock().unwrap_or_else(|p| p.into_inner());
    let (kept, items, tools) = if let Some(prev) = store.snaps.get(conversation) {
        let n = common_prefix(&prev.items, &fresh_items);
        let mut items = prev.items[..n].to_vec();
        items.extend(fresh_items.into_iter().skip(n));
        let tools_same = match (&prev.tools, &fresh_tools) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        };
        let tools = if tools_same {
            prev.tools.clone()
        } else {
            fresh_tools
        };
        (n, items, tools)
    } else {
        (0, fresh_items, fresh_tools)
    };

    obj.insert(history_key.to_string(), Value::Array(items.clone()));
    match &tools {
        Some(tools) => {
            obj.insert("tools".to_string(), tools.clone());
        }
        None => {
            obj.remove("tools");
        }
    }
    remember(&mut store, conversation.to_string(), Snap { items, tools });
    log::info!("[Strata] prefix kept={kept}");
    true
}

fn remember(store: &mut Store, id: String, snap: Snap) {
    if store.snaps.contains_key(&id) {
        store.order.retain(|k| k != &id);
    }
    store.order.push_back(id.clone());
    store.snaps.insert(id, snap);
    while store.order.len() > MAX_THREADS {
        if let Some(old) = store.order.pop_front() {
            store.snaps.remove(&old);
        }
    }
}

fn canonical_items(items: Option<&Vec<Value>>) -> Vec<Value> {
    items
        .into_iter()
        .flatten()
        .map(canon)
        .filter(|item| !blank_system(item))
        .collect()
}

fn canonical_tools(tools: &Value) -> Value {
    let Some(list) = tools.as_array() else {
        return canon(tools);
    };
    let mut tools: Vec<Value> = list.iter().map(canon).collect();
    tools.sort_by(|a, b| tool_name(a).cmp(&tool_name(b)));
    Value::Array(tools)
}

fn common_prefix(prev: &[Value], fresh: &[Value]) -> usize {
    prev.iter()
        .zip(fresh.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn blank_system(item: &Value) -> bool {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "system" && role != "developer" {
        return false;
    }
    match item.get("content") {
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Array(parts)) => {
            !parts.is_empty()
                && parts.iter().all(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.trim().is_empty())
                })
        }
        None => true,
        Some(_) => false,
    }
}

fn tool_name(tool: &Value) -> String {
    tool.get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .or_else(|| tool.get("name").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn canon(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), canon(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
        Value::String(text) => Value::String(strip_time(text)),
        other => other.clone(),
    }
}

fn strip_time(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        rest = &rest[start + OPEN.len()..];
        if let Some(end) = rest.find(CLOSE) {
            rest = &rest[end + CLOSE.len()..];
            if rest.starts_with('\n') {
                rest = &rest[1..];
            }
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn isolate(id: &str) {
        let mut store = STORE.lock().unwrap_or_else(|p| p.into_inner());
        store.snaps.remove(id);
        store.order.retain(|k| k != id);
    }

    #[test]
    fn a_later_clock_keeps_the_first_system_message_and_appends_the_new_turn() {
        let id = "prefix-clock";
        isolate(id);
        let mut first = json!({
            "messages": [
                {"role": "system", "content": "You are Codex.\n<codex_apps_client_time_context>t1</codex_apps_client_time_context>\n"},
                {"role": "user", "content": "hello"}
            ],
            "tools": [{"type": "function", "function": {"name": "b"}}, {"type": "function", "function": {"name": "a"}}]
        });
        assert!(stabilize("http://10.99.0.106:8005/v1", &mut first, id));
        assert!(!first["messages"][0]["content"].as_str().unwrap().contains("time_context"));
        assert_eq!(first["tools"][0]["function"]["name"], "a");

        let mut second = json!({
            "messages": [
                {"role": "system", "content": "You are Codex.\n<codex_apps_client_time_context>t2</codex_apps_client_time_context>\n"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "next"}
            ],
            "tools": [{"type": "function", "function": {"name": "a"}}, {"type": "function", "function": {"name": "b"}}]
        });
        assert!(stabilize("http://10.99.0.106:8005/v1", &mut second, id));
        let messages = second["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0], first["messages"][0]);
        assert_eq!(messages[1], first["messages"][1]);
        assert_eq!(messages[3]["content"], "next");
        assert_eq!(second["tools"], first["tools"]);
    }

    #[test]
    fn a_rewritten_history_is_not_pinned_past_the_change() {
        let id = "prefix-compact";
        isolate(id);
        let mut first = json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "old"}
            ]
        });
        stabilize("http://127.0.0.1:8005/v1", &mut first, id);
        let mut second = json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "summary"}
            ]
        });
        stabilize("http://127.0.0.1:8005/v1", &mut second, id);
        assert_eq!(second["messages"][1]["content"], "summary");
    }

    #[test]
    fn a_public_upstream_is_left_unchanged() {
        let mut body = json!({"messages": [{"role": "user", "content": "x"}]});
        let before = body.clone();
        assert!(!stabilize("https://api.openai.com/v1", &mut body, "prefix-public"));
        assert_eq!(body, before);
    }
}
