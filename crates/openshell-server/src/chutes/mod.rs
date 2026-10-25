// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Chutes E2EE egress proxy.
//!
//! Exposes an OpenAI-compatible surface on the multiplexed gateway port so
//! sandboxed workloads can reach the Chutes TEE models with end-to-end
//! encryption, without each client reimplementing the protocol:
//!
//! * `POST /v1/chutes/chat/completions` — accepts a normal `OpenAI` chat
//!   request (`stream` either way), encrypts it to the target chute
//!   instance's ML-KEM-768 key, relays it through `POST /e2e/invoke`, and
//!   returns the decrypted JSON or plaintext SSE. This gateway only ever
//!   relays opaque bytes upstream; it cannot read the prompt or response.
//! * `GET /v1/chutes/models` — passthrough of `GET {llm_base}/models`.
//!
//! Protocol references: the `claurst` E2EE provider (live-tested) and the
//! `zentinel` ai-gateway agent; wire details live in [`e2ee`].
//!
//! Configuration is environment-driven (no gateway.toml schema change):
//!
//! | Variable | Default |
//! |---|---|
//! | `CHUTES_API_KEY` | — (fallback when the caller sends no `Authorization`) |
//! | `CHUTES_API_BASE` | `https://api.chutes.ai` |
//! | `CHUTES_LLM_BASE` | `https://llm.chutes.ai/v1` |
//! | `CHUTES_E2E_INVOKE` | `https://api.chutes.ai/e2e/invoke` |
//! | `CHUTES_TIMEOUT_SECS` | `600` |
//!
//! Auth: callers present their own Chutes key as
//! `Authorization: Bearer <key>`; the environment key is only a fallback
//! for deployments where the gateway holds a shared key.

mod client;
pub mod e2ee;
mod stream;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tracing::debug;

use client::{ChutesClient, ChutesError, InvokeOutcome};
use stream::{accumulate_completion, decrypt_blob, decrypted_stream_body};

const DEFAULT_API_BASE: &str = "https://api.chutes.ai";
const DEFAULT_LLM_BASE: &str = "https://llm.chutes.ai/v1";
const DEFAULT_E2E_INVOKE: &str = "https://api.chutes.ai/e2e/invoke";
const DEFAULT_TIMEOUT_SECS: u64 = 600;

const SSE_CONTENT_TYPE: &str = "text/event-stream; charset=utf-8";

/// Upstream endpoints and fallbacks for the Chutes proxy.
#[derive(Debug, Clone)]
pub struct ChutesConfig {
    /// Base URL for the Chutes control API (discovery).
    pub api_base: String,
    /// Base URL for the OpenAI-compatible LLM API (model list).
    pub llm_base: String,
    /// URL of the E2EE invoke endpoint.
    pub e2e_invoke: String,
    /// Total timeout per upstream request, in seconds.
    pub timeout_secs: u64,
    /// API key used when the caller sends no `Authorization` header.
    pub default_api_key: Option<String>,
}

impl Default for ChutesConfig {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
            llm_base: DEFAULT_LLM_BASE.to_string(),
            e2e_invoke: DEFAULT_E2E_INVOKE.to_string(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            default_api_key: None,
        }
    }
}

impl ChutesConfig {
    /// Build the configuration from `CHUTES_*` environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        fn env(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
        Self {
            api_base: env("CHUTES_API_BASE").unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            llm_base: env("CHUTES_LLM_BASE").unwrap_or_else(|| DEFAULT_LLM_BASE.to_string()),
            e2e_invoke: env("CHUTES_E2E_INVOKE").unwrap_or_else(|| DEFAULT_E2E_INVOKE.to_string()),
            timeout_secs: env("CHUTES_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
            default_api_key: env("CHUTES_API_KEY"),
        }
    }
}

#[derive(Clone)]
struct ChutesState {
    config: Arc<ChutesConfig>,
    client: Arc<ChutesClient>,
}

impl ChutesState {
    fn new(config: ChutesConfig) -> Self {
        let client = ChutesClient::new(&config);
        Self {
            config: Arc::new(config),
            client: Arc::new(client),
        }
    }
}

/// Build the Chutes proxy router from the environment (mounted by
/// `http_router` on the gateway's multiplexed port).
pub fn router() -> Router {
    router_with_config(ChutesConfig::from_env())
}

/// Build the Chutes proxy router from an explicit configuration.
pub fn router_with_config(config: ChutesConfig) -> Router {
    let state = ChutesState::new(config);
    Router::new()
        .route("/v1/chutes/chat/completions", post(chat_completions))
        .route("/v1/chutes/models", get(models))
        .with_state(state)
}

/// Caller's Chutes key from `Authorization: Bearer <key>`, if any.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ").map_or("", str::trim);
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": "chutes_proxy_error"}})),
    )
        .into_response()
}

const MISSING_KEY_MESSAGE: &str = "missing Chutes API key: send `Authorization: Bearer <CHUTES_API_KEY>` or configure CHUTES_API_KEY on the gateway";

/// The caller's Chutes key: bearer header first, environment fallback second.
fn caller_api_key(state: &ChutesState, headers: &HeaderMap) -> Option<String> {
    bearer_token(headers).or_else(|| state.config.default_api_key.clone())
}

/// `POST /v1/chutes/chat/completions` — encrypt, invoke, decrypt.
async fn chat_completions(
    State(state): State<ChutesState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let Some(api_key) = caller_api_key(&state, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, MISSING_KEY_MESSAGE);
    };

    if !payload.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
        );
    }
    let Some(model) = payload.get("model").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "request body must include a string `model` field",
        );
    };
    let model = model.to_string();
    let client_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // The protocol always rides on an upstream SSE stream; clients that
    // asked for `stream: false` get the stream accumulated back into one
    // `chat.completion` object (matches the live-tested claurst behavior).
    let mut outbound = payload.clone();
    outbound["model"] = Value::String(model.strip_prefix("chutes/").unwrap_or(&model).to_string());
    outbound["stream"] = Value::Bool(true);
    let outbound_json = outbound.to_string();

    debug!(model = %model, client_stream, "chutes e2ee invoke");
    match state.client.invoke(&model, &outbound_json, &api_key).await {
        Ok(outcome) => render_upstream(outcome, client_stream, &model).await,
        Err(error) => map_error(error),
    }
}

/// Turn a successful upstream invoke into the client-facing response.
async fn render_upstream(
    outcome: InvokeOutcome,
    client_stream: bool,
    request_model: &str,
) -> Response {
    let InvokeOutcome { session, response } = outcome;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let is_sse = content_type
        .as_deref()
        .is_some_and(|ct| ct.contains("text/event-stream"));

    if is_sse {
        if client_stream {
            let mut response = Response::new(decrypted_stream_body(session, response));
            *response.status_mut() = status;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                SSE_CONTENT_TYPE.parse().expect("static header"),
            );
            return response;
        }
        return match accumulate_completion(session, response, request_model).await {
            Ok(completion) => (status, Json(completion)).into_response(),
            Err(error) => error_response(
                StatusCode::BAD_GATEWAY,
                &format!("e2ee stream accumulation failed: {error}"),
            ),
        };
    }

    // Non-SSE 2xx: either a plaintext JSON body (passthrough) or an
    // encrypted response blob.
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("failed reading upstream response: {error}"),
            );
        }
    };
    let opened = serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| format!("not plaintext JSON: {e}"))
        .or_else(|_| decrypt_blob(&session, &bytes).map_err(|e| e.to_string()));
    match opened {
        Ok(value) if client_stream => sse_single_event(value, status),
        Ok(value) => (status, Json(value)).into_response(),
        Err(error) => error_response(
            StatusCode::BAD_GATEWAY,
            &format!("failed opening upstream response: {error}"),
        ),
    }
}

/// Render one JSON value as a single-event SSE stream for a client that
/// requested `stream: true` but received a non-SSE upstream body.
fn sse_single_event(value: Value, status: StatusCode) -> Response {
    let body = format!("data: {value}\n\ndata: [DONE]\n\n");
    (status, [(header::CONTENT_TYPE, SSE_CONTENT_TYPE)], body).into_response()
}

/// Map client/upstream errors onto a response, forwarding upstream
/// non-2xx statuses and bodies verbatim.
fn map_error(error: ChutesError) -> Response {
    match error {
        ChutesError::Upstream {
            status,
            body,
            content_type,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            if let Some(value) = content_type
                .as_deref()
                .and_then(|ct| HeaderValue::from_str(ct).ok())
            {
                response.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            response
        }
        other => {
            tracing::error!(error = %other, "chutes upstream request failed");
            error_response(StatusCode::BAD_GATEWAY, &other.to_string())
        }
    }
}

/// `GET /v1/chutes/models` — passthrough of the upstream model list.
async fn models(
    State(state): State<ChutesState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let Some(api_key) = caller_api_key(&state, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, MISSING_KEY_MESSAGE);
    };
    let query = request.uri().query();
    match state.client.get_models(&api_key, query).await {
        Ok(upstream) => {
            let status =
                StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("application/json"));
            let body = match upstream.bytes().await {
                Ok(bytes) => Body::from(bytes),
                Err(error) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        &format!("failed reading upstream response: {error}"),
                    );
                }
            };
            let mut response = Response::new(body);
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
            response
        }
        Err(error) => map_error(error),
    }
}
