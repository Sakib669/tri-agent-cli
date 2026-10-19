//! Gemini provider: Antigravity subscription tokens → Cloud Code PA.
//! Protocol verified live: bare model ids, `userAgent`/`requestType`/
//! `requestId` wrapper, `request{contents,systemInstruction,...}`, and a
//! streamed JSON **array** response (`[{...},{...}]`).

use std::pin::Pin;

use anyhow::{anyhow, Result};
use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::sse;
use crate::store::GeminiCreds;

use super::{
    ContentPart, ModelInfo, Provider, ProviderEvent, ProviderId, Role, TurnRequest, UsageInfo,
};

fn default_base() -> String {
    std::env::var("TRIAGENT_GEMINI_BASE_URL")
        .unwrap_or_else(|_| "https://daily-cloudcode-pa.googleapis.com".to_string())
}

/// agy's client identity — the backend's license/entitlement check keys off it.
const USER_AGENT: &str = "antigravity/cli/1.1.13 (aidev_client; os_type=linux; arch=x86_64; cl=964361259; auth_method=consumer)";

pub struct GeminiProvider {
    creds: GeminiCreds,
    http: reqwest::Client,
    debug: bool,
    base: String,
}

impl GeminiProvider {
    pub fn new(creds: GeminiCreds, http: reqwest::Client, debug: bool) -> Result<Self> {
        Ok(Self {
            creds,
            http,
            debug,
            base: default_base(),
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
        h.insert(reqwest::header::USER_AGENT, USER_AGENT.parse().unwrap());
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

fn new_request_id() -> String {
    let cid = uuid::Uuid::new_v4();
    let tid = uuid::Uuid::new_v4();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("agent/{cid}/{ts}/{tid}/1")
}

/// Session id = first 32 hex chars of sha256 of the first user text (agy's
/// deriveSessionID).
fn derive_session_id(req: &TurnRequest) -> String {
    for m in &req.messages {
        if m.role != Role::User {
            continue;
        }
        let mut text = String::new();
        for p in &m.parts {
            if let ContentPart::Text { text: t } = p {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        if !text.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            let digest = hasher.finalize();
            return digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    uuid::Uuid::new_v4().to_string()
}

fn build_body(req: &TurnRequest) -> Value {
    let mut call_names = std::collections::HashMap::new();
    let contents: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "model",
            };
            let parts: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => json!({"text": text}),
                    ContentPart::ToolUse { id, name, input, thought_signature } => {
                        call_names.insert(id.clone(), name.clone());
                        let mut part = json!({"functionCall": {"id": id, "name": name, "args": input}});
                        if let Some(sig) = thought_signature {
                            // The signature sits on the PART, next to functionCall.
                            part["thoughtSignature"] = json!(sig);
                        }
                        part
                    }
                    ContentPart::ToolResult { tool_use_id, content, is_error } => {
                        let name = call_names.get(tool_use_id).cloned().unwrap_or_default();
                        json!({"functionResponse": {"id": tool_use_id, "name": name, "response": {"result": content, "error": *is_error}}})
                    }
                })
                .collect();
            json!({"role": role, "parts": parts})
        })
        .collect();
    let model = req.model.trim_start_matches("models/").to_string();
    let session_id = derive_session_id(req);
    let tools = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "model": model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": new_request_id(),
        "request": {
            "contents": contents,
            "systemInstruction": {"role": "user", "parts": [{"text": req.system}]},
            "tools": [{"functionDeclarations": tools}],
            "generationConfig": {
                "thinkingConfig": {"includeThoughts": true, "thinkingBudget": 10001},
                "maxOutputTokens": 8192,
            },
            "sessionId": session_id,
        },
    })
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        match self
            .http
            .post(format!("{}/v1internal:fetchAvailableModels", self.base))
            .headers(self.headers())
            .json(&json!({}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let Ok(v) = resp.json::<Value>().await else {
                    return Ok(Self::fallback_models());
                };
                // Response: {"models": {id: {"displayName", "quotaInfo": {"remainingFraction"}}}, "defaultAgentModelId"}
                let mut out: Vec<ModelInfo> = Vec::new();
                if let Some(models) = v.get("models").and_then(Value::as_object) {
                    for (id, info) in models {
                        let quota = info
                            .get("quotaInfo")
                            .and_then(|q| q.get("remainingFraction"))
                            .and_then(Value::as_f64)
                            .map(|f| format!("{:.0}% left", f * 100.0));
                        out.push(ModelInfo {
                            id: id.clone(),
                            display_name: info
                                .get("displayName")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            context_window: info.get("maxTokens").and_then(Value::as_u64),
                            quota,
                        });
                    }
                }
                out.sort_by(|a, b| a.id.cmp(&b.id));
                if out.is_empty() {
                    Ok(Self::fallback_models())
                } else {
                    Ok(out)
                }
            }
            Ok(resp) => Err(anyhow!(
                "fetchAvailableModels returned {}: {}",
                resp.status(),
                truncate(&resp.text().await.unwrap_or_default(), 300)
            )),
            Err(e) => Err(anyhow!("fetchAvailableModels: {e}")),
        }
    }

    fn stream_turn(
        &self,
        req: TurnRequest,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<ProviderEvent>> + Send>> {
        let http = self.http.clone();
        let debug = self.debug;
        let base = self.base.clone();
        let headers = self.headers();
        let body = build_body(&req);

        Box::pin(try_stream! {
            let resp = http
                .post(format!("{base}/v1internal:streamGenerateContent"))
                .headers(headers)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.bytes().await.unwrap_or_default();
                yield ProviderEvent::Error(format!(
                    "HTTP {status} from v1internal:streamGenerateContent: {}",
                    truncate(&String::from_utf8_lossy(&body), 600)
                ));
                yield ProviderEvent::TurnEnd;
                return;
            }
            let mut call_counter: u64 = 0;
            let mut stream = Box::pin(sse::parse_json_array_chunks(resp.bytes_stream()));
            while let Some(line) = stream.next().await {
                let line = line?;
                if debug {
                    eprintln!("[gemini chunk] {}", truncate(&line, 300));
                }
                for out in handle_data_line(&line, &mut call_counter) {
                    yield out;
                }
            }
            yield ProviderEvent::TurnEnd;
        })
    }

    async fn usage(&self) -> Result<UsageInfo> {
        let Ok(resp) = self
            .http
            .post(format!("{}/v1internal:retrieveUserQuotaSummary", self.base))
            .headers(self.headers())
            .json(&json!({}))
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
        let mut fractions: Vec<f64> = Vec::new();
        walk_fractions(&v, &mut fractions);
        if fractions.is_empty() {
            return Ok(UsageInfo::default());
        }
        let min = fractions.iter().cloned().fold(f64::INFINITY, f64::min);
        Ok(UsageInfo {
            label: format!("lowest model quota remaining: {:.0}%", min * 100.0),
        })
    }
}

impl GeminiProvider {
    fn fallback_models() -> Vec<ModelInfo> {
        [
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-low",
            "gemini-3.5-flash-lite",
            "gemini-3-flash",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b-medium",
        ]
        .iter()
        .map(|id| ModelInfo {
            id: id.to_string(),
            display_name: None,
            context_window: None,
            quota: None,
        })
        .collect()
    }
}

/// Pure data-line handler (testable against recorded fixtures). Each chunk is
/// one top-level object of the streamed array, normally `{"response": {...}}`.
pub fn handle_data_line(line: &str, call_counter: &mut u64) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return out;
    };
    if let Some(msg) = v["error"]["message"].as_str() {
        out.push(ProviderEvent::Error(msg.to_string()));
        return out;
    }
    // Unwrap the Cloud Code envelope.
    let root = v.get("response").unwrap_or(&v);
    if let Some(msg) = root["error"]["message"].as_str() {
        out.push(ProviderEvent::Error(msg.to_string()));
        return out;
    }
    if let Some(cands) = root["candidates"].as_array() {
        for cand in cands {
            if let Some(parts) = cand["content"]["parts"].as_array() {
                for p in parts {
                    let is_thought = p.get("thought").and_then(Value::as_bool).unwrap_or(false);
                    if let Some(t) = p["text"].as_str() {
                        if is_thought {
                            out.push(ProviderEvent::Reasoning(t.to_string()));
                        } else {
                            out.push(ProviderEvent::TextDelta(t.to_string()));
                        }
                    }
                    if let Some(fc) = p["functionCall"].as_object() {
                        *call_counter += 1;
                        let name = fc
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let id = fc
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("call_{call_counter}"));
                        let args = fc.get("args").cloned().unwrap_or(Value::Null);
                        let thought_signature = p
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        out.push(ProviderEvent::ToolCall {
                            id,
                            name,
                            args_json: args.to_string(),
                            thought_signature,
                        });
                    }
                }
            }
        }
    }
    if let Some(um) = root["usageMetadata"].as_object() {
        out.push(ProviderEvent::Usage {
            input_tokens: um.get("promptTokenCount").and_then(Value::as_u64),
            output_tokens: um.get("candidatesTokenCount").and_then(Value::as_u64),
        });
    }
    out
}

fn walk_fractions(v: &Value, out: &mut Vec<f64>) {
    match v {
        Value::Object(map) => {
            if let Some(f) = map.get("remainingFraction").and_then(Value::as_f64) {
                out.push(f.clamp(0.0, 1.0));
            }
            for val in map.values() {
                walk_fractions(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                walk_fractions(val, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_tool_and_usage() {
        let mut c = 0;
        let evs = handle_data_line(
            r#"{"candidates":[{"content":{"parts":[{"text":"hi"},{"functionCall":{"name":"bash","args":{"command":"ls"}}}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":7}}"#,
            &mut c,
        );
        assert!(matches!(&evs[0], ProviderEvent::TextDelta(t) if t == "hi"));
        assert!(
            matches!(&evs[1], ProviderEvent::ToolCall { id, name, args_json, .. }
            if id == "call_1" && name == "bash" && args_json.contains("ls"))
        );
        assert!(matches!(
            evs[2],
            ProviderEvent::Usage {
                input_tokens: Some(5),
                output_tokens: Some(7)
            }
        ));
    }

    #[test]
    fn parses_error() {
        let mut c = 0;
        let evs = handle_data_line(r#"{"error":{"message":"bad"}}"#, &mut c);
        assert!(matches!(&evs[0], ProviderEvent::Error(m) if m == "bad"));
    }
}
