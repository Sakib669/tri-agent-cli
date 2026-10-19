//! Claude provider: claude.ai subscription token → Anthropic Messages API.
//! Streaming over standard SSE; tool_use blocks mapped to ProviderEvents.

use std::collections::{BTreeSet, HashMap};
use std::pin::Pin;

use anyhow::Result;
use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::sse;
use crate::store::ClaudeCreds;

use super::{
    ContentPart, ModelInfo, Provider, ProviderEvent, ProviderId, Role, TurnRequest, UsageInfo,
};

pub const API_BASE: &str = "https://api.anthropic.com";

pub struct ClaudeProvider {
    creds: ClaudeCreds,
    http: reqwest::Client,
    debug: bool,
}

#[derive(Default)]
pub(crate) struct AnthropicState {
    tools: HashMap<usize, ToolState>,
}

#[derive(Default)]
pub(crate) struct ToolState {
    id: String,
    name: String,
    args: String,
}

impl ClaudeProvider {
    pub fn new(creds: ClaudeCreds, http: reqwest::Client, debug: bool) -> Result<Self> {
        Ok(Self { creds, http, debug })
    }

    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.creds.access_token)
                .parse()
                .expect("valid header value"),
        );
        h
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

fn build_body(req: &TurnRequest) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            json!({
                "role": role,
                "content": m.parts.iter().map(|p| match p {
                    ContentPart::Text { text } => json!({"type": "text", "text": text}),
                    ContentPart::ToolUse { id, name, input, .. } =>
                        json!({"type": "tool_use", "id": id, "name": name, "input": input}),
                    ContentPart::ToolResult { tool_use_id, content, is_error } =>
                        json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}),
                }).collect::<Vec<_>>()
            })
        })
        .collect();
    json!({
        "model": req.model,
        "max_tokens": req.max_tokens.max(1024),
        "system": req.system,
        "messages": messages,
        "tools": req.tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema,
        })).collect::<Vec<_>>(),
        "stream": true,
    })
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let v: Value = self
            .http
            .get("https://downloads.claude.ai/model-catalog/v1/catalog.json")
            .send()
            .await?
            .json()
            .await?;
        let mut ids = BTreeSet::new();
        walk_ids(&v, &mut ids);
        let mut out: Vec<ModelInfo> = ids
            .into_iter()
            .filter(|id| {
                id.starts_with("claude-")
                    && !matches!(
                        id.as_str(),
                        "claude-api"
                            | "claude-code"
                            | "claude-in-chrome"
                            | "claude-plugin"
                            | "claude-test"
                    )
            })
            .map(|id| ModelInfo {
                id,
                display_name: None,
                context_window: None,
                quota: None,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn stream_turn(
        &self,
        req: TurnRequest,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<ProviderEvent>> + Send>> {
        let http = self.http.clone();
        let debug = self.debug;
        let body = build_body(&req);
        let mut headers = self.auth_headers();
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert("anthropic-beta", "oauth-2025-04-20".parse().unwrap());
        headers.insert(
            "anthropic-dangerous-direct-browser-access",
            "true".parse().unwrap(),
        );
        headers.insert("x-app", "cli".parse().unwrap());

        Box::pin(try_stream! {
            let resp = http
                .post(format!("{API_BASE}/v1/messages"))
                .headers(headers)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.bytes().await.unwrap_or_default();
                yield ProviderEvent::Error(format!(
                    "HTTP {status} from /v1/messages: {}",
                    truncate(&String::from_utf8_lossy(&body), 600)
                ));
                yield ProviderEvent::TurnEnd;
                return;
            }
            let mut st = AnthropicState::default();
            let mut stream = Box::pin(sse::parse_sse(resp.bytes_stream()));
            while let Some(item) = stream.next().await {
                let (ev, data) = item?;
                if debug {
                    eprintln!("[claude sse] event={} data={}", ev, truncate(&data, 200));
                }
                for out in handle_event(&ev, &data, &mut st) {
                    yield out;
                }
            }
        })
    }

    async fn usage(&self) -> Result<UsageInfo> {
        let Ok(resp) = self
            .http
            .get("https://claude.ai/api/oauth/usage")
            .headers(self.auth_headers())
            .send()
            .await
        else {
            return Ok(UsageInfo::default());
        };
        if !resp.status().is_success() {
            return Ok(UsageInfo::default());
        }
        let Ok(v) = resp.json::<Value>().await else {
            return Ok(UsageInfo::default());
        };
        // Defensive: surface any recognizable quota fields.
        let mut bits = Vec::new();
        for key in ["remaining", "limit", "used", "at_wall", "weekly_limit"] {
            if let Some(x) = v.get(key) {
                if !x.is_null() {
                    bits.push(format!("{key}={x}"));
                }
            }
        }
        if bits.is_empty() && v.is_object() {
            return Ok(UsageInfo::default());
        }
        Ok(UsageInfo {
            label: bits.join(" "),
        })
    }
}

/// Pure event handler (testable against recorded fixtures).
pub(crate) fn handle_event(ev: &str, data: &str, st: &mut AnthropicState) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    match ev {
        "message_start" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(t) = v["message"]["usage"]["input_tokens"].as_u64() {
                    out.push(ProviderEvent::Usage {
                        input_tokens: Some(t),
                        output_tokens: None,
                    });
                }
            }
        }
        "content_block_start" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let block = &v["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    st.tools.insert(
                        idx,
                        ToolState {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            args: String::new(),
                        },
                    );
                }
            }
        }
        "content_block_delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let delta = &v["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(t) = delta["text"].as_str() {
                            out.push(ProviderEvent::TextDelta(t.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = delta["partial_json"].as_str() {
                            if let Some(s) = st.tools.get_mut(&idx) {
                                s.args.push_str(p);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                if let Some(t) = st.tools.remove(&idx) {
                    out.push(ProviderEvent::ToolCall {
                        id: t.id,
                        name: t.name,
                        args_json: t.args,
                        thought_signature: None,
                    });
                }
            }
        }
        "message_delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(t) = v["usage"]["output_tokens"].as_u64() {
                    out.push(ProviderEvent::Usage {
                        input_tokens: None,
                        output_tokens: Some(t),
                    });
                }
            }
        }
        "message_stop" => out.push(ProviderEvent::TurnEnd),
        "error" => {
            let msg = serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| data.to_string());
            out.push(ProviderEvent::Error(msg));
        }
        _ => {}
    }
    out
}

fn walk_ids(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(id) = map.get("id").and_then(Value::as_str) {
                out.insert(id.to_string());
            }
            for val in map.values() {
                walk_ids(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                walk_ids(val, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_tool_use_turn() {
        let mut st = AnthropicState::default();
        let evs = handle_event(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":11}}}"#,
            &mut st,
        );
        assert!(matches!(
            evs[0],
            ProviderEvent::Usage {
                input_tokens: Some(11),
                ..
            }
        ));

        let evs = handle_event(
            "content_block_start",
            r#"{"index":1,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}"#,
            &mut st,
        );
        assert!(evs.is_empty());

        let evs = handle_event(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            &mut st,
        );
        assert!(evs.is_empty());
        let evs = handle_event(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            &mut st,
        );
        assert!(evs.is_empty());

        let evs = handle_event("content_block_stop", r#"{"index":1}"#, &mut st);
        assert!(
            matches!(&evs[0], ProviderEvent::ToolCall { id, name, args_json, .. }
            if id == "t1" && name == "bash" && args_json == "{\"command\":\"ls\"}")
        );

        let evs = handle_event(
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
            &mut st,
        );
        assert!(matches!(
            evs[0],
            ProviderEvent::Usage {
                output_tokens: Some(3),
                ..
            }
        ));

        let evs = handle_event("message_stop", "{}", &mut st);
        assert!(matches!(evs[0], ProviderEvent::TurnEnd));
    }

    #[test]
    fn error_event() {
        let mut st = AnthropicState::default();
        let evs = handle_event("error", r#"{"error":{"message":"boom"}}"#, &mut st);
        assert!(matches!(&evs[0], ProviderEvent::Error(m) if m == "boom"));
    }
}
