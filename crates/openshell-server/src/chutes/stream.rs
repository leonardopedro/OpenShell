// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Streaming (SSE) response handling for the Chutes E2EE proxy.
//!
//! Upstream responses arrive as `text/event-stream` carrying E2EE control
//! events (`e2e_init`, `e2e`, `e2e_error`) instead of plaintext `OpenAI`
//! chunks. [`SseDecryptor`] turns that stream back into plaintext:
//!
//! * `{"e2e_init": "<b64 ml-kem ct>"}` — derive the stream key (once).
//! * `{"e2e": "<b64 nonce||ct||tag}"}` — decrypt into raw SSE text, which is
//!   re-parsed as one or more plaintext `data:` lines.
//! * `{"e2e_error": ...}` — surfaced downstream as an SSE error event.
//!
//! Two consumers share the parser: the streaming proxy re-emits plaintext
//! SSE lines untouched, while [`accumulate_completion`] folds the decrypted
//! chunks into a single non-streaming `chat.completion` JSON for clients
//! that asked for `stream: false` (the proxy always asks upstream for
//! `stream: true`, matching the reference implementation).

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use serde_json::{Value, json};
use tracing::warn;

use super::e2ee::{E2eeError, E2eeSession};

/// One downstream-visible SSE item produced by [`SseDecryptor`].
#[derive(Debug)]
pub enum SseItem {
    /// Framing line (blank separator, comment, or non-`data` field).
    Other(String),
    /// A `data:` line: raw payload text plus its JSON value when parseable.
    Data { raw: String, value: Option<Value> },
    /// The terminal `data: [DONE]` marker.
    Done,
    /// A server-reported `e2e_error` event.
    E2eeError(Value),
}

/// Renders one item as a single SSE line (the following blank-line framing
/// arrives separately as [`SseItem::Other`]).
fn render(item: &SseItem) -> String {
    match item {
        SseItem::Other(line) => {
            if line.is_empty() {
                "\n".to_string()
            } else {
                format!("{line}\n")
            }
        }
        SseItem::Data { raw, .. } => format!("data: {raw}\n"),
        SseItem::Done => "data: [DONE]\n".to_string(),
        SseItem::E2eeError(value) => format!("data: {value}\n"),
    }
}

fn error_event_line(message: &str) -> String {
    let event = json!({"error": {"message": message, "type": "chutes_e2ee_proxy_error"}});
    format!("data: {event}\n\n")
}

/// Incremental SSE parser + E2EE stream decryptor.
pub struct SseDecryptor {
    session: E2eeSession,
    stream_key: Option<Vec<u8>>,
    leftover: String,
}

impl SseDecryptor {
    pub fn new(session: E2eeSession) -> Self {
        Self {
            session,
            stream_key: None,
            leftover: String::new(),
        }
    }

    /// Feed a chunk of upstream SSE text; returns the items that became
    /// complete. Decrypt failures are returned as an error string.
    pub fn feed(&mut self, text: &str) -> Result<Vec<SseItem>, String> {
        let mut combined = std::mem::take(&mut self.leftover);
        combined.push_str(text);

        let mut lines: Vec<&str> = combined.split('\n').collect();
        if !combined.ends_with('\n') {
            self.leftover = lines.pop().unwrap_or_default().to_string();
        }

        let mut items = Vec::new();
        for line in lines {
            self.process_line(line.trim_end_matches('\r'), &mut items)?;
        }
        Ok(items)
    }

    /// Process any trailing partial line at end of stream.
    pub fn flush(&mut self) -> Result<Vec<SseItem>, String> {
        if self.leftover.is_empty() {
            return Ok(Vec::new());
        }
        let line = std::mem::take(&mut self.leftover);
        let mut items = Vec::new();
        self.process_line(line.trim_end_matches('\r'), &mut items)?;
        Ok(items)
    }

    fn process_line(&mut self, line: &str, items: &mut Vec<SseItem>) -> Result<(), String> {
        if line.is_empty() || line.starts_with(':') {
            items.push(SseItem::Other(line.to_string()));
            return Ok(());
        }
        let Some(payload) = line.strip_prefix("data:") else {
            items.push(SseItem::Other(line.to_string()));
            return Ok(());
        };
        self.process_data_payload(payload.trim(), items)
    }

    fn process_data_payload(
        &mut self,
        payload: &str,
        items: &mut Vec<SseItem>,
    ) -> Result<(), String> {
        if payload == "[DONE]" {
            items.push(SseItem::Done);
            return Ok(());
        }

        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            items.push(SseItem::Data {
                raw: payload.to_string(),
                value: None,
            });
            return Ok(());
        };

        if let Some(init) = value.get("e2e_init").and_then(Value::as_str) {
            self.stream_key = Some(
                self.session
                    .decrypt_stream_init(init)
                    .map_err(|e| format!("e2e_init decrypt failed: {e}"))?,
            );
            return Ok(());
        }

        if let Some(chunk) = value.get("e2e").and_then(Value::as_str) {
            let Some(key) = self.stream_key.as_deref() else {
                warn!("received e2e chunk before e2e_init; dropping");
                return Ok(());
            };
            let plaintext = self
                .session
                .decrypt_stream_chunk(chunk, key)
                .map_err(|e| format!("e2e chunk decrypt failed: {e}"))?;
            let text = String::from_utf8_lossy(&plaintext);
            let mut nested = Vec::new();
            for line in text.split('\n') {
                self.process_line(line.trim_end_matches('\r'), &mut nested)?;
            }
            items.extend(nested);
            return Ok(());
        }

        if let Some(error) = value.get("e2e_error") {
            items.push(SseItem::E2eeError(error.clone()));
            return Ok(());
        }

        items.push(SseItem::Data {
            raw: payload.to_string(),
            value: Some(value),
        });
        Ok(())
    }
}

type BoxedByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

struct DecryptStreamState {
    upstream: BoxedByteStream,
    decryptor: SseDecryptor,
    pending: VecDeque<String>,
    finished: bool,
}

/// Wrap an upstream E2EE SSE response into a plaintext SSE response body.
pub fn decrypted_stream_body(session: E2eeSession, response: reqwest::Response) -> Body {
    let state = DecryptStreamState {
        upstream: Box::pin(response.bytes_stream()),
        decryptor: SseDecryptor::new(session),
        pending: VecDeque::new(),
        finished: false,
    };

    let stream = futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(text) = state.pending.pop_front() {
                return Some((Ok::<Bytes, std::io::Error>(Bytes::from(text)), state));
            }
            if state.finished {
                return None;
            }
            match state.upstream.next().await {
                Some(Ok(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk);
                    match state.decryptor.feed(&text) {
                        Ok(items) => {
                            state.pending.extend(items.iter().map(render));
                        }
                        Err(message) => {
                            state.finished = true;
                            state.pending.push_back(error_event_line(&message));
                        }
                    }
                }
                Some(Err(error)) => {
                    warn!(%error, "upstream e2ee stream read failed");
                    state.finished = true;
                    state
                        .pending
                        .push_back(error_event_line(&format!("upstream stream error: {error}")));
                }
                None => {
                    state.finished = true;
                    match state.decryptor.flush() {
                        Ok(items) => {
                            state.pending.extend(items.iter().map(render));
                        }
                        Err(message) => {
                            state.pending.push_back(error_event_line(&message));
                        }
                    }
                }
            }
        }
    });

    Body::from_stream(stream)
}

/// Fold a decrypted SSE stream into one non-streaming `chat.completion`
/// JSON object (for clients that requested `stream: false`).
///
/// `request_model` is the model name from the original client request, used
/// when no upstream chunk carries one.
pub async fn accumulate_completion(
    session: E2eeSession,
    mut response: reqwest::Response,
    request_model: &str,
) -> Result<Value, String> {
    let mut builder = CompletionBuilder::default();
    let mut decryptor = SseDecryptor::new(session);

    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let text = String::from_utf8_lossy(&chunk);
                for item in decryptor.feed(&text)? {
                    if handle_item(&mut builder, item)? {
                        return builder.finish(request_model);
                    }
                }
            }
            Ok(None) => {
                for item in decryptor.flush()? {
                    if handle_item(&mut builder, item)? {
                        return builder.finish(request_model);
                    }
                }
                break;
            }
            Err(error) => return Err(format!("upstream stream error: {error}")),
        }
    }

    builder.finish(request_model)
}

/// Upper bound on streamed tool-call indexes accepted into one completion —
/// a bogus `index` must not size the accumulator.
const MAX_TOOL_CALLS: usize = 64;

/// One streamed tool call, folded across the delta chunks that carry it.
#[derive(Default)]
struct ToolCallAccum {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Accumulated state for one non-streaming completion.
#[derive(Default)]
struct CompletionBuilder {
    id: Option<String>,
    model: Option<String>,
    created: Option<u64>,
    role: Option<String>,
    content: String,
    reasoning: String,
    tool_calls: Vec<ToolCallAccum>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    saw_chunk: bool,
}

impl CompletionBuilder {
    fn merge_chunk(&mut self, chunk: &Value) {
        self.saw_chunk = true;
        if let Some(id) = chunk.get("id").and_then(Value::as_str) {
            self.id.get_or_insert_with(|| id.to_string());
        }
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            self.model.get_or_insert_with(|| model.to_string());
        }
        if let Some(created) = chunk.get("created").and_then(Value::as_u64) {
            self.created.get_or_insert(created);
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(role) = delta.get("role").and_then(Value::as_str) {
            self.role.get_or_insert_with(|| role.to_string());
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.content.push_str(text);
        }
        for field in ["reasoning_content", "reasoning_text", "reasoning"] {
            if let Some(text) = delta.get(field).and_then(Value::as_str) {
                self.reasoning.push_str(text);
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map_or(0, |raw| usize::try_from(raw).unwrap_or(usize::MAX));
                if index >= MAX_TOOL_CALLS {
                    warn!(index, "dropping tool call with out-of-range index");
                    continue;
                }
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(ToolCallAccum::default());
                }
                let slot = &mut self.tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.id.get_or_insert_with(|| id.to_string());
                }
                if slot.name.is_empty()
                    && let Some(name) = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                {
                    slot.name = name.to_string();
                }
                if let Some(args) = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    slot.arguments.push_str(args);
                }
            }
        }
    }

    fn finish(self, request_model: &str) -> Result<Value, String> {
        if !self.saw_chunk {
            return Err("e2ee stream completed without any chunks".to_string());
        }
        let created = self.created.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default()
        });
        let mut message = json!({
            "role": self.role.unwrap_or_else(|| "assistant".to_string()),
            "content": self.content,
        });
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(self.reasoning);
        }
        if !self.tool_calls.is_empty() {
            let calls: Vec<Value> = self
                .tool_calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    json!({
                        "id": call.id.clone().unwrap_or_else(|| format!("call-{index}")),
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments,
                        },
                    })
                })
                .collect();
            message["tool_calls"] = Value::Array(calls);
        }
        let model = self.model.unwrap_or_else(|| request_model.to_string());
        Ok(json!({
            "id": self.id.unwrap_or_else(|| "chatcmpl-openshell".to_string()),
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": self.finish_reason.unwrap_or_else(|| "stop".to_string()),
            }],
            "usage": self.usage.unwrap_or(Value::Null),
        }))
    }
}

/// Process one decrypted item; returns `true` when `[DONE]` was seen.
fn handle_item(builder: &mut CompletionBuilder, item: SseItem) -> Result<bool, String> {
    match item {
        SseItem::Data {
            value: Some(value), ..
        } => {
            builder.merge_chunk(&value);
            Ok(false)
        }
        SseItem::Data { raw, value: None } => {
            warn!(payload = %raw, "non-JSON data line in e2ee stream");
            Ok(false)
        }
        SseItem::Done => Ok(true),
        SseItem::E2eeError(error) => Err(format!("server e2ee error: {error}")),
        SseItem::Other(_) => Ok(false),
    }
}

/// Open an encrypted non-streaming response blob into a JSON value.
pub fn decrypt_blob(session: &E2eeSession, blob: &[u8]) -> Result<Value, E2eeError> {
    let plaintext = session.decrypt_response(blob)?;
    serde_json::from_slice(&plaintext)
        .map_err(|e| E2eeError::DecryptionFailed(format!("decrypted response is not JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chutes::e2ee::parse_instance_pubkey;
    use crate::chutes::e2ee::server_sim::ChuteInstance;

    fn instance_session() -> (ChuteInstance, E2eeSession, String) {
        let instance = ChuteInstance::new();
        let pk = parse_instance_pubkey(&instance.pubkey_b64()).expect("parse pk");
        let session = E2eeSession::new(&pk);
        let request = r#"{"model":"m","stream":true}"#;
        let blob = session.encrypt_request(request).expect("encrypt");
        let decrypted = instance.decrypt_request(&blob).expect("decrypt");
        (instance, session, decrypted)
    }

    #[test]
    fn decrypts_stream_events_into_plaintext_sse() {
        let (_instance, session, decrypted) = instance_session();
        let (init, key) = ChuteInstance::stream_init(&decrypted);

        let plaintext_sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}],",
            "\"usage\":{\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let chunk = ChuteInstance::stream_chunk(&key, plaintext_sse).expect("chunk");

        let mut decryptor = SseDecryptor::new(session);
        let upstream =
            format!("data: {{\"e2e_init\":\"{init}\"}}\n\ndata: {{\"e2e\":\"{chunk}\"}}\n\n");
        let items = decryptor.feed(&upstream).expect("feed");
        let rendered: String = items.iter().map(render).collect();

        assert!(rendered.contains("\"Hello\""), "rendered: {rendered}");
        assert!(rendered.contains("\" world\""), "rendered: {rendered}");
        assert!(rendered.contains("data: [DONE]"), "rendered: {rendered}");
        assert!(!rendered.contains("e2e_init"), "rendered: {rendered}");
        assert!(!rendered.contains("\"e2e\":\""), "rendered: {rendered}");
    }

    #[test]
    fn passes_through_unencrypted_data_lines() {
        let (_, session, _) = instance_session();
        let mut decryptor = SseDecryptor::new(session);
        let items = decryptor
            .feed("data: {\"id\":\"plain\"}\n\ndata: [DONE]\n\n")
            .expect("feed");
        assert!(matches!(items[0], SseItem::Data { .. }));
        assert!(matches!(items[2], SseItem::Done));
    }

    #[test]
    fn drops_chunk_before_init() {
        let (_, session, _) = instance_session();
        let mut decryptor = SseDecryptor::new(session);
        let items = decryptor
            .feed("data: {\"e2e\":\"AAAA\"}\n\n")
            .expect("feed");
        assert!(
            !items.iter().any(|i| matches!(i, SseItem::Data { .. })),
            "pre-init chunk must not surface as data"
        );
    }

    #[test]
    fn surfaces_server_e2e_error_events() {
        let (_, session, _) = instance_session();
        let mut decryptor = SseDecryptor::new(session);
        let items = decryptor
            .feed("data: {\"e2e_error\":{\"message\":\"boom\"}}\n\n")
            .expect("feed");
        assert!(matches!(items[0], SseItem::E2eeError(_)));
    }

    #[test]
    fn accumulates_chunks_into_completion_json() {
        let (_instance, session, decrypted) = instance_session();
        let (init, key) = ChuteInstance::stream_init(&decrypted);
        let c1 = ChuteInstance::stream_chunk(
                &key,
                "data: {\"id\":\"cmpl-1\",\"model\":\"m\",\"created\":7,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi \"},\"finish_reason\":null}]}\n\n",
            )
            .expect("c1");
        let c2 = ChuteInstance::stream_chunk(
            &key,
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"there\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n",
            )
            .expect("c2");

        // Drive CompletionBuilder through the same item pipeline the network
        // path uses, by feeding rendered SSE back through a decryptor-free
        // path: reuse accumulate via a local in-memory response is not
        // possible with reqwest::Response, so drive handle_item directly.
        let mut decryptor = SseDecryptor::new(session);
        let upstream = format!(
            "data: {{\"e2e_init\":\"{init}\"}}\n\ndata: {{\"e2e\":\"{c1}\"}}\n\ndata: {{\"e2e\":\"{c2}\"}}\n\n"
        );
        let mut builder = CompletionBuilder::default();
        let mut done = false;
        for item in decryptor.feed(&upstream).expect("feed") {
            if handle_item(&mut builder, item).expect("handle") {
                done = true;
            }
        }
        assert!(done, "must see [DONE]");
        let completion = builder.finish("req-model").expect("finish");
        assert_eq!(completion["id"], "cmpl-1");
        assert_eq!(completion["object"], "chat.completion");
        assert_eq!(completion["created"], 7);
        assert_eq!(completion["choices"][0]["message"]["content"], "Hi there");
        assert_eq!(completion["choices"][0]["finish_reason"], "stop");
        assert_eq!(completion["usage"]["total_tokens"], 5);
    }

    #[test]
    fn accumulates_tool_calls_and_reasoning_across_chunks() {
        let (_instance, session, decrypted) = instance_session();
        let (init, key) = ChuteInstance::stream_init(&decrypted);

        let c1 = ChuteInstance::stream_chunk(
            &key,
            concat!(
                "data: {\"id\":\"cmpl-tc\",\"model\":\"m\",\"created\":9,",
                "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
                "\"reasoning_text\":\"thinking...\",",
                "\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",",
                "\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},",
                "\"finish_reason\":null}]}\n\n",
            ),
        )
        .expect("c1");
        let c2 = ChuteInstance::stream_chunk(
            &key,
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[",
                r#"{"index":0,"function":{"arguments":"{\"location\":\"Paris\"}"}}]},"#,
                "\"finish_reason\":\"tool_calls\"}],",
                "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            ),
        )
        .expect("c2");

        let mut decryptor = SseDecryptor::new(session);
        let upstream = format!(
            "data: {{\"e2e_init\":\"{init}\"}}\n\ndata: {{\"e2e\":\"{c1}\"}}\n\ndata: {{\"e2e\":\"{c2}\"}}\n\ndata: [DONE]\n\n"
        );
        let mut builder = CompletionBuilder::default();
        let mut done = false;
        for item in decryptor.feed(&upstream).expect("feed") {
            if handle_item(&mut builder, item).expect("handle") {
                done = true;
            }
        }
        assert!(done, "must see [DONE]");

        let completion = builder.finish("req-model").expect("finish");
        let message = &completion["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "thinking...");
        assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");

        let calls = message["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"location":"Paris"}"#);
        assert_eq!(completion["usage"]["total_tokens"], 3);
    }
}
