use crate::{
    domain::{Role, ToolCall, ToolFunctionCall, ToolSpec},
    error::AppError,
};
use futures_util::StreamExt;
use http::StatusCode;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct LlmClient {
    http: Client,
    chat_completions_only: Arc<RwLock<HashSet<String>>>,
}

#[derive(Clone, Debug)]
pub enum LlmContentPart {
    Text(String),
    ImageUrl(String),
}

#[derive(Clone, Debug)]
pub struct PreparedMessage {
    pub role: Role,
    pub text: String,
    pub parts: Vec<LlmContentPart>,
    pub reasoning_items: Vec<Value>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum LlmEvent {
    Delta {
        content: String,
        reasoning: String,
    },
    ToolCalls {
        calls: Vec<ToolCall>,
        reasoning_items: Vec<Value>,
    },
    Usage(u64),
}

pub struct ChatCompletionRequest<'a> {
    pub base_url: &'a str,
    pub api_key: Option<&'a str>,
    pub model: &'a str,
    pub reasoning: Option<&'a str>,
    pub system_prompt: Option<&'a str>,
    pub messages: &'a [PreparedMessage],
    pub tools: &'a [ToolSpec],
    pub timeout: Option<Duration>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct StreamEnvelope {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    error: Option<ApiErrorPayload>,
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCallDelta>,
}

#[derive(Deserialize)]
struct StreamToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: Option<ApiErrorPayload>,
}

#[derive(Deserialize)]
struct ApiErrorPayload {
    message: Option<String>,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl LlmClient {
    pub fn new() -> Result<Self, AppError> {
        let http = Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            chat_completions_only: Arc::new(RwLock::new(HashSet::new())),
        })
    }

    pub async fn discover_models(
        &self,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<String>, AppError> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let mut req = self.http.get(url).timeout(Duration::from_secs(15));
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Upstream(format!(
                "model discovery failed ({status}): {}",
                extract_api_error(&body)
            )));
        }
        let parsed: ModelsResponse = resp.json().await?;
        let mut ids: Vec<String> = parsed
            .data
            .into_iter()
            .map(|m| m.id)
            .filter(|id| !id.trim().is_empty())
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub async fn stream_chat(
        &self,
        req_params: ChatCompletionRequest<'_>,
    ) -> Result<mpsc::Receiver<Result<LlmEvent, AppError>>, AppError> {
        let base = req_params.base_url.trim_end_matches('/');
        let force_chat = self
            .chat_completions_only
            .read()
            .map(|set| set.contains(base))
            .unwrap_or(false);

        if !force_chat {
            let responses_url = format!("{base}/responses");
            let responses_payload = build_responses_payload(&req_params);
            let mut req = self.http.post(&responses_url).json(&responses_payload);
            if let Some(timeout) = req_params.timeout {
                req = req.timeout(timeout);
            }
            if let Some(key) = req_params.api_key {
                req = req.bearer_auth(key);
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status.is_success() {
                return spawn_responses_reader(resp).await;
            }
            if matches!(
                status,
                StatusCode::NOT_FOUND
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::NOT_IMPLEMENTED
            ) {
                if let Ok(mut set) = self.chat_completions_only.write() {
                    set.insert(base.to_owned());
                }
            } else {
                let body = resp.text().await.unwrap_or_default();
                return Err(AppError::Upstream(format!(
                    "upstream error ({status}): {}",
                    extract_api_error(&body)
                )));
            }
        }

        let chat_url = format!("{base}/chat/completions");
        let chat_payload = build_chat_payload(&req_params);
        let mut req = self.http.post(chat_url).json(&chat_payload);
        if let Some(timeout) = req_params.timeout {
            req = req.timeout(timeout);
        }
        if let Some(key) = req_params.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Upstream(format!(
                "upstream error ({status}): {}",
                extract_api_error(&body)
            )));
        }
        spawn_chat_completions_reader(resp).await
    }
}

async fn spawn_responses_reader(
    resp: reqwest::Response,
) -> Result<mpsc::Receiver<Result<LlmEvent, AppError>>, AppError> {
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (tx, rx) = mpsc::channel(64);

    if !content_type.contains("text/event-stream") {
        let body = resp.text().await?;
        tokio::spawn(async move {
            let Ok(val) = serde_json::from_str::<Value>(&body) else {
                let _ = tx
                    .send(Err(AppError::Upstream(
                        "unexpected upstream response".to_owned(),
                    )))
                    .await;
                return;
            };
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            let mut reasoning_items = Vec::new();
            if let Some(items) = val["output"].as_array() {
                for item in items {
                    match item["type"].as_str().unwrap_or("") {
                        "message" => {
                            if let Some(parts) = item["content"].as_array() {
                                for p in parts {
                                    if let Some(t) = p["text"].as_str() {
                                        text.push_str(t);
                                    }
                                }
                            }
                        }
                        "reasoning" => {
                            if let Some(summary) = item["summary"].as_array() {
                                for s in summary {
                                    if let Some(t) = s["text"].as_str() {
                                        reasoning.push_str(t);
                                    }
                                }
                            }
                            reasoning_items.push(clean_reasoning_item(item));
                        }
                        "function_call" => {
                            if let Some(tc) = parse_responses_tool_call(item) {
                                calls.push(tc);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !text.is_empty() || !reasoning.is_empty() {
                let _ = tx
                    .send(Ok(LlmEvent::Delta {
                        content: text,
                        reasoning,
                    }))
                    .await;
            }
            if !calls.is_empty() {
                let _ = tx
                    .send(Ok(LlmEvent::ToolCalls {
                        calls,
                        reasoning_items,
                    }))
                    .await;
            }
            if let Some(tokens) = total_tokens(&val["usage"]) {
                let _ = tx.send(Ok(LlmEvent::Usage(tokens))).await;
            }
        });
        return Ok(rx);
    }

    let mut byte_stream = resp.bytes_stream();
    tokio::spawn(async move {
        let mut buffer = Vec::with_capacity(4096);
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut reasoning_items: Vec<Value> = Vec::new();

        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(AppError::Upstream(e.to_string()))).await;
                    return;
                }
            };
            buffer.extend_from_slice(&chunk);

            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let mut line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                while matches!(line_bytes.last(), Some(b'\n' | b'\r')) {
                    line_bytes.pop();
                }
                if line_bytes.is_empty() {
                    continue;
                }
                let line = String::from_utf8_lossy(&line_bytes);
                let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
                    continue;
                };
                if data == "[DONE]" {
                    if !calls.is_empty() {
                        let _ = tx
                            .send(Ok(LlmEvent::ToolCalls {
                                calls,
                                reasoning_items,
                            }))
                            .await;
                    }
                    return;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                match ev["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => {
                        if let Some(d) = ev["delta"].as_str().filter(|s| !s.is_empty())
                            && tx
                                .send(Ok(LlmEvent::Delta {
                                    content: d.to_owned(),
                                    reasoning: String::new(),
                                }))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(d) = ev["delta"].as_str().filter(|s| !s.is_empty())
                            && tx
                                .send(Ok(LlmEvent::Delta {
                                    content: String::new(),
                                    reasoning: d.to_owned(),
                                }))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }
                    "response.output_item.done" => {
                        let item = &ev["item"];
                        match item["type"].as_str().unwrap_or("") {
                            "function_call" => {
                                if let Some(tc) = parse_responses_tool_call(item) {
                                    calls.push(tc);
                                }
                            }
                            "reasoning" => {
                                reasoning_items.push(clean_reasoning_item(item));
                            }
                            _ => {}
                        }
                    }
                    "response.completed" => {
                        if let Some(tokens) = total_tokens(&ev["response"]["usage"]) {
                            let _ = tx.send(Ok(LlmEvent::Usage(tokens))).await;
                        }
                        if let Some(out) = ev["response"]["output"].as_array() {
                            if calls.is_empty() {
                                for it in out {
                                    if it["type"] == "function_call"
                                        && let Some(tc) = parse_responses_tool_call(it)
                                    {
                                        calls.push(tc);
                                    }
                                }
                            }
                            if reasoning_items.is_empty() {
                                for it in out {
                                    if it["type"] == "reasoning" {
                                        reasoning_items.push(clean_reasoning_item(it));
                                    }
                                }
                            }
                        }
                        if !calls.is_empty() {
                            let _ = tx
                                .send(Ok(LlmEvent::ToolCalls {
                                    calls,
                                    reasoning_items,
                                }))
                                .await;
                        }
                        return;
                    }
                    "response.failed" => {
                        let msg = ev["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("upstream response failed")
                            .to_owned();
                        let _ = tx.send(Err(AppError::Upstream(msg))).await;
                        return;
                    }
                    "error" => {
                        let msg = ev["error"]["message"]
                            .as_str()
                            .or_else(|| ev["message"].as_str())
                            .unwrap_or("upstream stream error")
                            .to_owned();
                        let _ = tx.send(Err(AppError::Upstream(msg))).await;
                        return;
                    }
                    _ => {}
                }
            }
        }

        if !calls.is_empty() {
            let _ = tx
                .send(Ok(LlmEvent::ToolCalls {
                    calls,
                    reasoning_items,
                }))
                .await;
        }
    });

    Ok(rx)
}

fn parse_responses_tool_call(item: &Value) -> Option<ToolCall> {
    let name = item["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let call_id = item["call_id"]
        .as_str()
        .or_else(|| item["id"].as_str())
        .unwrap_or_default();
    let args = item["arguments"].as_str().unwrap_or("{}");
    Some(ToolCall {
        id: call_id.to_owned(),
        function: ToolFunctionCall {
            name: name.to_owned(),
            arguments: args.to_owned(),
        },
    })
}

fn clean_reasoning_item(item: &Value) -> Value {
    let mut out = json!({
        "type": "reasoning",
        "id": item["id"].as_str().unwrap_or_default(),
        "summary": item.get("summary").cloned().unwrap_or_else(|| json!([])),
    });
    if let Some(enc) = item.get("encrypted_content").filter(|v| !v.is_null()) {
        out["encrypted_content"] = enc.clone();
    }
    if let Some(cnt) = item.get("content").filter(|v| !v.is_null()) {
        out["content"] = cnt.clone();
    }
    out
}

fn build_responses_payload(req: &ChatCompletionRequest<'_>) -> Value {
    let effort = req.reasoning.map(str::trim).filter(|s| !s.is_empty());
    let include_reasoning = effort.is_some_and(|e| e != "none");

    let mut input = Vec::with_capacity(req.messages.len() + 2);
    for msg in req.messages {
        match msg.role {
            Role::System => {
                if !msg.text.is_empty() {
                    input.push(json!({ "role": "system", "content": msg.text }));
                }
            }
            Role::User => {
                if msg.parts.is_empty() {
                    input.push(json!({ "role": "user", "content": msg.text }));
                } else {
                    let parts: Vec<Value> = msg
                        .parts
                        .iter()
                        .map(|p| match p {
                            LlmContentPart::Text(t) => json!({ "type": "input_text", "text": t }),
                            LlmContentPart::ImageUrl(url) => json!({
                                "type": "input_image",
                                "image_url": url,
                                "detail": "auto"
                            }),
                        })
                        .collect();
                    input.push(json!({ "role": "user", "content": parts }));
                }
            }
            Role::Assistant => {
                if include_reasoning {
                    for ri in &msg.reasoning_items {
                        input.push(ri.clone());
                    }
                }
                if !msg.text.is_empty() {
                    input.push(json!({ "role": "assistant", "content": msg.text }));
                }
                for tc in &msg.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.function.name,
                        "arguments": tc.function.arguments,
                    }));
                }
            }
            Role::Tool => {
                if let Some(call_id) = &msg.tool_call_id {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": msg.text,
                    }));
                }
            }
        }
    }

    let mut body = json!({
        "model": req.model,
        "input": input,
        "store": false,
        "stream": true,
    });

    if let Some(sys) = req.system_prompt.filter(|s| !s.trim().is_empty()) {
        body["instructions"] = Value::String(sys.to_owned());
    }

    if let Some(eff) = effort {
        body["reasoning"] = json!({ "effort": eff });
        if include_reasoning {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }

    body
}

async fn spawn_chat_completions_reader(
    resp: reqwest::Response,
) -> Result<mpsc::Receiver<Result<LlmEvent, AppError>>, AppError> {
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (tx, rx) = mpsc::channel(64);

    if !content_type.contains("text/event-stream") {
        let body = resp.text().await?;
        tokio::spawn(async move {
            if let Ok(val) = serde_json::from_str::<Value>(&body) {
                let msg = &val["choices"][0]["message"];
                let content = msg["content"].as_str().unwrap_or_default().to_owned();
                let reasoning = msg["reasoning_content"]
                    .as_str()
                    .or_else(|| msg["reasoning"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                if !content.is_empty() || !reasoning.is_empty() {
                    let _ = tx.send(Ok(LlmEvent::Delta { content, reasoning })).await;
                }
                if let Some(tokens) = total_tokens(&val["usage"]) {
                    let _ = tx.send(Ok(LlmEvent::Usage(tokens))).await;
                }
            } else {
                let _ = tx
                    .send(Err(AppError::Upstream(
                        "unexpected upstream response".to_owned(),
                    )))
                    .await;
            }
        });
        return Ok(rx);
    }

    let mut byte_stream = resp.bytes_stream();
    tokio::spawn(async move {
        let mut buffer = Vec::with_capacity(4096);
        let mut tool_calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();

        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(AppError::Upstream(e.to_string()))).await;
                    return;
                }
            };
            buffer.extend_from_slice(&chunk);

            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let mut line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                while matches!(line_bytes.last(), Some(b'\n' | b'\r')) {
                    line_bytes.pop();
                }
                if line_bytes.is_empty() {
                    continue;
                }
                let line = String::from_utf8_lossy(&line_bytes);
                let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
                    continue;
                };
                if data == "[DONE]" {
                    flush_chat_tool_calls(&tx, &mut tool_calls).await;
                    return;
                }
                if let Ok(env) = serde_json::from_str::<StreamEnvelope>(data) {
                    if let Some(tokens) = env.usage.as_ref().and_then(total_tokens) {
                        let _ = tx.send(Ok(LlmEvent::Usage(tokens))).await;
                    }
                    if let Some(err) = env.error.and_then(|e| e.message) {
                        let _ = tx.send(Err(AppError::Upstream(err))).await;
                        return;
                    }
                    for choice in env.choices {
                        let content = choice.delta.content.unwrap_or_default();
                        let reasoning = choice
                            .delta
                            .reasoning_content
                            .or(choice.delta.reasoning)
                            .unwrap_or_default();
                        if (!content.is_empty() || !reasoning.is_empty())
                            && tx
                                .send(Ok(LlmEvent::Delta { content, reasoning }))
                                .await
                                .is_err()
                        {
                            return;
                        }
                        for tc in choice.delta.tool_calls {
                            let entry = tool_calls.entry(tc.index).or_default();
                            if let Some(id) = tc.id {
                                entry.id = id;
                            }
                            if let Some(func) = tc.function {
                                if let Some(name) = func.name {
                                    entry.name.push_str(&name);
                                }
                                if let Some(args) = func.arguments {
                                    entry.arguments.push_str(&args);
                                }
                            }
                        }
                    }
                }
            }
        }

        flush_chat_tool_calls(&tx, &mut tool_calls).await;
    });

    Ok(rx)
}

async fn flush_chat_tool_calls(
    tx: &mpsc::Sender<Result<LlmEvent, AppError>>,
    partial: &mut BTreeMap<usize, PartialToolCall>,
) {
    if partial.is_empty() {
        return;
    }
    let calls: Vec<ToolCall> = std::mem::take(partial)
        .into_values()
        .filter(|c| !c.name.is_empty())
        .map(|c| ToolCall {
            id: c.id,
            function: ToolFunctionCall {
                name: c.name,
                arguments: c.arguments,
            },
        })
        .collect();
    if !calls.is_empty() {
        let _ = tx
            .send(Ok(LlmEvent::ToolCalls {
                calls,
                reasoning_items: Vec::new(),
            }))
            .await;
    }
}

fn build_chat_payload(req: &ChatCompletionRequest<'_>) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(sys) = req.system_prompt.filter(|s| !s.trim().is_empty()) {
        messages.push(json!({ "role": "system", "content": sys }));
    }
    for msg in req.messages {
        messages.push(serialize_chat_message(msg));
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if let Some(effort) = req.reasoning.filter(|s| !s.trim().is_empty()) {
        body["reasoning_effort"] = Value::String(effort.to_owned());
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }

    body
}

fn serialize_chat_message(msg: &PreparedMessage) -> Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    let content_val = if msg.parts.is_empty() {
        if msg.role == Role::Assistant && msg.text.is_empty() && !msg.tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(msg.text.clone())
        }
    } else {
        let parts: Vec<Value> = msg
            .parts
            .iter()
            .map(|p| match p {
                LlmContentPart::Text(t) => json!({ "type": "text", "text": t }),
                LlmContentPart::ImageUrl(url) => {
                    json!({ "type": "image_url", "image_url": { "url": url } })
                }
            })
            .collect();
        Value::Array(parts)
    };

    let mut obj = json!({ "role": role, "content": content_val });

    if !msg.tool_calls.is_empty() {
        let calls: Vec<Value> = msg
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.function.name,
                        "arguments": tc.function.arguments,
                    }
                })
            })
            .collect();
        obj["tool_calls"] = Value::Array(calls);
    }

    if let Some(call_id) = &msg.tool_call_id {
        obj["tool_call_id"] = Value::String(call_id.clone());
    }

    obj
}

fn extract_api_error(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorEnvelope>(body)
        && let Some(msg) = parsed.error.and_then(|e| e.message)
    {
        return msg;
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty error body".to_owned()
    } else {
        trimmed.chars().take(300).collect()
    }
}

fn total_tokens(usage: &Value) -> Option<u64> {
    usage["total_tokens"].as_u64().or_else(|| {
        let input = usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64())?;
        let output = usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())?;
        Some(input.saturating_add(output))
    })
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn reads_responses_and_chat_completions_usage_without_estimating_tokens() {
        assert_eq!(total_tokens(&json!({"total_tokens": 123})), Some(123));
        assert_eq!(
            total_tokens(&json!({"input_tokens": 90, "output_tokens": 20})),
            Some(110)
        );
        assert_eq!(
            total_tokens(&json!({"prompt_tokens": 40, "completion_tokens": 10})),
            Some(50)
        );
        assert_eq!(total_tokens(&json!({"total_tokens": 0})), Some(0));
        assert_eq!(total_tokens(&Value::Null), None);
        assert_eq!(total_tokens(&json!({"input_tokens": 90})), None);
    }
}
