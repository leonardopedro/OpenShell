// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Transport-neutral Chutes E2EE proxy core.
//!
//! Two frontends serve the same routes: the gateway's axum router and the
//! standalone Pingora edge (`openshell-chutes-edge`). Everything that decides
//! *what* the proxy answers lives here, in terms of status codes, JSON values
//! and upstream responses, so the two HTTP stacks cannot drift.

use serde_json::{Value, json};
use tracing::debug;

use super::client::{ChutesClient, ChutesError, InvokeOutcome};
use super::e2ee::E2eeSession;
use super::stream::{accumulate_completion, decrypt_blob};

pub const SSE_CONTENT_TYPE: &str = "text/event-stream; charset=utf-8";

pub const MISSING_KEY_MESSAGE: &str = "missing Chutes API key: send `Authorization: Bearer <CHUTES_API_KEY>` or configure CHUTES_API_KEY on the gateway";

/// Route table shared by both frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    ChatCompletions,
    Models,
    /// A known path reached with the wrong method.
    MethodNotAllowed,
    NotFound,
}

pub fn route(method: &str, path: &str) -> Route {
    match path {
        "/v1/chutes/chat/completions" if method == "POST" => Route::ChatCompletions,
        "/v1/chutes/models" if method == "GET" => Route::Models,
        "/v1/chutes/chat/completions" | "/v1/chutes/models" => Route::MethodNotAllowed,
        _ => Route::NotFound,
    }
}

/// The proxy's uniform error envelope.
pub fn error_body(message: &str) -> Value {
    json!({"error": {"message": message, "type": "chutes_proxy_error"}})
}

/// A status + JSON body the frontend renders as-is.
#[derive(Debug, Clone)]
pub struct ErrorReply {
    pub status: u16,
    pub value: Value,
}

impl ErrorReply {
    pub fn new(status: u16, message: &str) -> Self {
        Self {
            status,
            value: error_body(message),
        }
    }
}

/// A buffered upstream answer, forwarded verbatim.
pub enum BufferedReply {
    /// Body plus the upstream content type, if it had one.
    Body {
        status: u16,
        body: Vec<u8>,
        content_type: Option<String>,
    },
    Error(ErrorReply),
}

/// The original `model` field plus the rewritten upstream payload.
#[derive(Debug)]
pub struct ChatRequest {
    pub model: String,
    pub client_stream: bool,
    pub outbound_json: String,
}

/// Validate a client chat payload and build the upstream one.
///
/// The protocol always rides on an upstream SSE stream; clients that asked for
/// `stream: false` get the stream accumulated back into one `chat.completion`
/// object (matches the live-tested claurst behavior).
pub fn prepare_chat(payload: &Value) -> Result<ChatRequest, ErrorReply> {
    if !payload.is_object() {
        return Err(ErrorReply::new(
            400,
            "request body must be a JSON object",
        ));
    }
    let Some(model) = payload.get("model").and_then(Value::as_str) else {
        return Err(ErrorReply::new(
            400,
            "request body must include a string `model` field",
        ));
    };
    let model = model.to_string();
    let client_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut outbound = payload.clone();
    outbound["model"] =
        Value::String(model.strip_prefix("chutes/").unwrap_or(&model).to_string());
    outbound["stream"] = Value::Bool(true);

    Ok(ChatRequest {
        model,
        client_stream,
        outbound_json: outbound.to_string(),
    })
}

/// An upstream SSE response that must be decrypted chunk by chunk.
pub struct ChatStream {
    pub status: u16,
    pub session: E2eeSession,
    pub response: reqwest::Response,
}

/// What a successful (or failed) chat call produced.
pub enum ChatOutcome {
    /// Upstream SSE that the frontend must decrypt chunk by chunk.
    Stream(Box<ChatStream>),
    /// One complete JSON document.
    Json { status: u16, value: Value },
    /// A ready-made SSE document (streaming was requested but upstream sent a
    /// single non-SSE body).
    Sse { status: u16, text: String },
    /// Forwarded non-2xx upstream response, verbatim.
    Buffered(BufferedReply),
    /// Locally generated error.
    Error(ErrorReply),
}

fn map_error(error: ChutesError) -> BufferedReply {
    match error {
        ChutesError::Upstream {
            status,
            body,
            content_type,
        } => BufferedReply::Body {
            status,
            body: body.into_bytes(),
            content_type,
        },
        other => {
            tracing::error!(error = %other, "chutes upstream request failed");
            BufferedReply::Error(ErrorReply::new(502, &other.to_string()))
        }
    }
}

fn single_event_sse(value: &Value) -> String {
    format!("data: {value}\n\ndata: [DONE]\n\n")
}

/// Encrypt the payload, invoke the upstream E2EE endpoint, and classify the
/// response for the frontend.
pub async fn chat(client: &ChutesClient, api_key: &str, payload: &Value) -> ChatOutcome {
    let prepared = match prepare_chat(payload) {
        Ok(prepared) => prepared,
        Err(error) => return ChatOutcome::Error(error),
    };

    debug!(
        model = %prepared.model,
        client_stream = prepared.client_stream,
        "chutes e2ee invoke"
    );
    let outcome = match client
        .invoke(&prepared.model, &prepared.outbound_json, api_key)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return ChatOutcome::Buffered(map_error(error)),
    };

    let InvokeOutcome { session, response } = outcome;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let is_sse = content_type
        .as_deref()
        .is_some_and(|ct| ct.contains("text/event-stream"));

    if is_sse {
        if prepared.client_stream {
            return ChatOutcome::Stream(Box::new(ChatStream {
                status,
                session,
                response,
            }));
        }
        return match accumulate_completion(session, response, &prepared.model).await {
            Ok(completion) => ChatOutcome::Json {
                status,
                value: completion,
            },
            Err(error) => ChatOutcome::Error(ErrorReply::new(
                502,
                &format!("e2ee stream accumulation failed: {error}"),
            )),
        };
    }

    // Non-SSE 2xx: either a plaintext JSON body (passthrough) or an encrypted
    // response blob.
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ChatOutcome::Error(ErrorReply::new(
                502,
                &format!("failed reading upstream response: {error}"),
            ));
        }
    };
    let opened = serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| format!("not plaintext JSON: {e}"))
        .or_else(|_| decrypt_blob(&session, &bytes).map_err(|e| e.to_string()));
    match opened {
        Ok(value) if prepared.client_stream => ChatOutcome::Sse {
            status,
            text: single_event_sse(&value),
        },
        Ok(value) => ChatOutcome::Json { status, value },
        Err(error) => ChatOutcome::Error(ErrorReply::new(
            502,
            &format!("failed opening upstream response: {error}"),
        )),
    }
}

/// Fetch the upstream model list.
pub async fn models(client: &ChutesClient, api_key: &str, query: Option<&str>) -> BufferedReply {
    match client.get_models(api_key, query).await {
        Ok(upstream) => {
            let status = upstream.status().as_u16();
            let content_type = upstream
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map_or_else(
                    || Some("application/json".to_string()),
                    |ct| Some(ct.to_owned()),
                );
            match upstream.bytes().await {
                Ok(bytes) => BufferedReply::Body {
                    status,
                    body: bytes.to_vec(),
                    content_type,
                },
                Err(error) => BufferedReply::Error(ErrorReply::new(
                    502,
                    &format!("failed reading upstream response: {error}"),
                )),
            }
        }
        Err(error) => map_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_is_exact() {
        assert_eq!(
            route("POST", "/v1/chutes/chat/completions"),
            Route::ChatCompletions
        );
        assert_eq!(route("GET", "/v1/chutes/models"), Route::Models);
        assert_eq!(
            route("GET", "/v1/chutes/chat/completions"),
            Route::MethodNotAllowed
        );
        assert_eq!(
            route("POST", "/v1/chutes/models"),
            Route::MethodNotAllowed
        );
        assert_eq!(route("GET", "/v1/chutes/other"), Route::NotFound);
    }

    #[test]
    fn prepare_chat_strips_the_chutes_prefix_and_forces_streaming() {
        let payload = json!({"model": "chutes/zai-org/GLM-5.1-TEE", "stream": false});
        let prepared = prepare_chat(&payload).expect("valid payload");
        assert_eq!(prepared.model, "chutes/zai-org/GLM-5.1-TEE");
        assert!(!prepared.client_stream);
        let outbound: Value = serde_json::from_str(&prepared.outbound_json).unwrap();
        assert_eq!(outbound["model"], "zai-org/GLM-5.1-TEE");
        assert_eq!(outbound["stream"], true);
        // The caller's object is untouched.
        assert_eq!(payload["model"], "chutes/zai-org/GLM-5.1-TEE");
        assert_eq!(payload["stream"], false);
    }

    #[test]
    fn prepare_chat_rejects_malformed_payloads() {
        let err = prepare_chat(&json!([1, 2])).unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.value["error"]["message"].as_str().unwrap().contains("JSON object"));

        let err = prepare_chat(&json!({"stream": true})).unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.value["error"]["message"].as_str().unwrap().contains("model"));
    }
}
