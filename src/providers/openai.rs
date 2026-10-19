//! ChatGPT provider: subscription tokens → chatgpt.com/backend-api/codex
//! (OpenAI Responses API over SSE). Protocol verified against openai/codex.

use std::collections::BTreeMap;
use std::pin::Pin;

use anyhow::{anyhow, Result};
use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::auth::jwt_claim;
use crate::sse;
use crate::store::ChatGptCreds;

use super::{
    ContentPart, ModelInfo, Provider, ProviderEvent, ProviderId, Role, TurnRequest, UsageInfo,
};

pub const BASE: &str = "https://chatgpt.com/backend-api/codex";

pub struct ChatGptProvider {
    creds: ChatGptCreds,
    http: reqwest::Client,
    debug: bool,
    plan: Option<String>,
    session_id: String,
}

impl ChatGptProvider {
    pub fn new(creds: ChatGptCreds, http: reqwest::Client, debug: bool) -> Result<Self> {
        let plan = jwt_claim(
            &creds.id_token,
            "https://api.openai.com/auth.chatgpt_plan_type",
        )
        .or_else(|| jwt_claim(&creds.id_token, "chatgpt_plan_type"));
        let session_id = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            creds,
            http,
            debug,
            plan,
            session_id,
        })
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.creds.access_token)
                .parse()
                .unwrap(),
        );
        if let Some(acc) = &self.creds.account_id {
            h.insert("ChatGPT-Account-ID", acc.parse().unwrap());
        }
        h.insert("originator", "codex_cli_rs".parse().unwrap());
        h.insert(
            reqwest::header::USER_AGENT,
            format!("triagent/{}", env!("CARGO_PKG_VERSION"))
                .parse()
                .unwrap(),
        );
        h.insert("session-id", self.session_id.parse().unwrap());
        h
    }
}

/// Pure event handler (testable against recorded fixtures).
pub(crate) fn handle_event(
    ev: &str,
    data: &str,
    tool_args: &mut BTreeMap<String, String>,
) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    match ev {
        "response.output_text.delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(t) = v["delta"].as_str() {
                    out.push(ProviderEvent::TextDelta(t.to_string()));
                }
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(t) = v["delta"].as_str() {
                    out.push(ProviderEvent::Reasoning(t.to_string()));
                }
            }
        }
        "response.custom_tool_call_input.delta" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let call_id = v["call_id"].as_str().unwrap_or("").to_string();
                let args = v["args"].as_str().unwrap_or("");
                tool_args.entry(call_id).or_default().push_str(args);
            }
        }
        "response.output_item.done" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                let item = &v["item"];
                if item["type"].as_str() == Some("function_call") {
                    let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    let args = item["arguments"]
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| tool_args.remove(&call_id))
                        .unwrap_or_else(|| "{}".to_string());
                    out.push(ProviderEvent::ToolCall {
                        id: call_id,
                        name,
                        args_json: args,
                        thought_signature: None,
                    });
                }
            }
        }
        "response.completed" => {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(u) = v["response"]["usage"].as_object() {
                    out.push(ProviderEvent::Usage {
                        input_tokens: u["input_tokens"].as_u64(),
                        output_tokens: u["output_tokens"].as_u64(),
                    });
                }
            }
            out.push(ProviderEvent::TurnEnd);
        }
        "response.incomplete" => out.push(ProviderEvent::TurnEnd),
        "response.failed" => {
            let msg = serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|v| {
                    v["response"]["error"]["message"]
                        .as_str()
                        .or(v["response"]["error"]["code"].as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "response failed".to_string());
            out.push(ProviderEvent::Error(msg));
            out.push(ProviderEvent::TurnEnd);
        }
        "error" => {
            let msg = serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| data.to_string());
            out.push(ProviderEvent::Error(msg));
            out.push(ProviderEvent::TurnEnd);
        }
        _ => {}
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

fn build_input(req: &TurnRequest) -> Vec<Value> {
    let mut input = Vec::new();
    for m in &req.messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut text_buf: Vec<String> = Vec::new();
        let push_text = |buf: &mut Vec<String>, input: &mut Vec<Value>, role: &str| {
            if !buf.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": buf.drain(..).map(|t| json!({"type": "input_text", "text": t})).collect::<Vec<_>>(),
                }));
            }
        };
        for p in &m.parts {
            match p {
                ContentPart::Text { text } => text_buf.push(text.clone()),
                ContentPart::ToolUse {
                    id,
                    name,
                    input: tool_input,
                    ..
                } => {
                    push_text(&mut text_buf, &mut input, role);
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": tool_input.to_string(),
                    }));
                }
                ContentPart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    push_text(&mut text_buf, &mut input, role);
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": format!("{}{}", content, if *is_error { "\n[error]" } else { "" }),
                    }));
                }
            }
        }
        push_text(&mut text_buf, &mut input, role);
    }
    input
}

#[async_trait]
impl Provider for ChatGptProvider {
    fn id(&self) -> ProviderId {
        ProviderId::ChatGpt
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .http
            .get(format!("{BASE}/models"))
            .query(&[("client_version", env!("CARGO_PKG_VERSION"))])
            .headers(self.headers())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "models endpoint returned {}: {}",
                resp.status(),
                truncate(&resp.text().await.unwrap_or_default(), 300)
            ));
        }
        let v: Value = resp.json().await?;
        let arr = v
            .as_array()
            .or_else(|| v.get("models").and_then(Value::as_array));
        let Some(arr) = arr else {
            return Ok(vec![]);
        };
        let mut out = arr
            .iter()
            .filter_map(|m| {
                Some(ModelInfo {
                    id: m.get("slug")?.as_str()?.to_string(),
                    display_name: m
                        .get("display_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    context_window: m.get("context_window").and_then(Value::as_u64),
                    quota: None,
                })
            })
            .collect::<Vec<_>>();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn stream_turn(
        &self,
        req: TurnRequest,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<ProviderEvent>> + Send>> {
        let http = self.http.clone();
        let debug = self.debug;
        let headers = self.headers();
        let input = build_input(&req);
        let body = json!({
            "model": req.model,
            "input": input,
            "tools": req.tools.iter().map(|t| json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })).collect::<Vec<_>>(),
            "tool_choice": "auto",
            "store": false,
            "stream": true,
            "reasoning": {"effort": "medium"},
        });

        Box::pin(try_stream! {
            let resp = http
                .post(format!("{BASE}/responses"))
                .headers(headers)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.bytes().await.unwrap_or_default();
                yield ProviderEvent::Error(format!(
                    "HTTP {status} from /backend-api/codex/responses: {}",
                    truncate(&String::from_utf8_lossy(&body), 600)
                ));
                yield ProviderEvent::TurnEnd;
                return;
            }
            let mut tool_args: BTreeMap<String, String> = BTreeMap::new();
            let mut stream = Box::pin(sse::parse_sse(resp.bytes_stream()));
            while let Some(item) = stream.next().await {
                let (ev, data) = item?;
                if debug {
                    eprintln!("[chatgpt sse] event={} data={}", ev, truncate(&data, 200));
                }
                for out in handle_event(&ev, &data, &mut tool_args) {
                    yield out;
                }
            }
        })
    }

    async fn usage(&self) -> Result<UsageInfo> {
        let label = match &self.plan {
            Some(plan) => format!("plan: {plan}"),
            None => "plan: unknown".to_string(),
        };
        Ok(UsageInfo { label })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_tool_call_stream() {
        let mut args = BTreeMap::new();
        let evs = handle_event(
            "response.output_text.delta",
            r#"{"delta":"hello"}"#,
            &mut args,
        );
        assert!(matches!(&evs[0], ProviderEvent::TextDelta(t) if t == "hello"));

        let _ = handle_event(
            "response.custom_tool_call_input.delta",
            r#"{"item_id":"i1","call_id":"call_1","args":"{\"cmd\":"}"#,
            &mut args,
        );
        let _ = handle_event(
            "response.custom_tool_call_input.delta",
            r#"{"item_id":"i1","call_id":"call_1","args":"\"ls\"}"}"#,
            &mut args,
        );
        let evs = handle_event(
            "response.output_item.done",
            r#"{"item":{"type":"function_call","call_id":"call_1","name":"bash"}}"#,
            &mut args,
        );
        assert!(matches!(
            &evs[0],
            ProviderEvent::ToolCall { id, name, args_json, .. }
                if id == "call_1" && name == "bash" && args_json == "{\"cmd\":\"ls\"}"
        ));

        let evs = handle_event(
            "response.completed",
            r#"{"response":{"usage":{"input_tokens":10,"output_tokens":20}}}"#,
            &mut args,
        );
        assert!(matches!(
            evs[0],
            ProviderEvent::Usage {
                input_tokens: Some(10),
                output_tokens: Some(20)
            }
        ));
        assert!(matches!(evs[1], ProviderEvent::TurnEnd));
    }

    #[test]
    fn parses_failed_response() {
        let mut args = BTreeMap::new();
        let evs = handle_event(
            "response.failed",
            r#"{"response":{"error":{"code":"insufficient_quota","message":"quota"}}}"#,
            &mut args,
        );
        assert!(matches!(&evs[0], ProviderEvent::Error(m) if m.contains("quota")));
        assert!(matches!(evs[1], ProviderEvent::TurnEnd));
    }
}
