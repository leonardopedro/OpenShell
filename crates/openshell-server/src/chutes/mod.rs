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
//! | `CHUTES_TLS_SPKI_PINS` | — (base64 SHA-256 SPKI pins for the upstream chain) |
//! | `CHUTES_PROXY` | `none` (`env` honours `HTTP(S)_PROXY`, or a proxy URL) |
//!
//! Auth: callers present their own Chutes key as
//! `Authorization: Bearer <key>`; the environment key is only a fallback
//! for deployments where the gateway holds a shared key.

pub mod api;
mod client;
pub mod e2ee;
pub mod stream;
mod tls;
#[cfg(test)]
mod tests;

pub use client::ChutesClient;

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use stream::decrypted_stream_body;

const DEFAULT_API_BASE: &str = "https://api.chutes.ai";
const DEFAULT_LLM_BASE: &str = "https://llm.chutes.ai/v1";
const DEFAULT_E2E_INVOKE: &str = "https://api.chutes.ai/e2e/invoke";
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// How the upstream client treats proxies.
///
/// `None` is the default: the E2EE hop should not be silently routed through
/// whatever proxy the process environment happens to describe. Operators opt
/// in explicitly, either by pointing at a proxy URL or by saying they want
/// the environment honoured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyPolicy {
    /// Ignore proxy environment variables entirely.
    None,
    /// Honour `HTTP(S)_PROXY` / `NO_PROXY` like a generic HTTP client.
    Environment,
    /// Send every upstream request through this proxy URL.
    Url(String),
}

impl ProxyPolicy {
    /// Parse a `CHUTES_PROXY` value: `none`/unset, `env`, or a proxy URL.
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            None | Some("" | "none") => Self::None,
            Some("env" | "environment") => Self::Environment,
            Some(url) => Self::Url(url.to_string()),
        }
    }
}

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
    /// Accepted leaf `SubjectPublicKeyInfo` pins for the upstream TLS chain.
    /// Empty means "trust the platform roots" (the default).
    pub tls_spki_pins: Vec<tls::SpkiPin>,
    /// Proxy policy for upstream requests.
    pub proxy: ProxyPolicy,
}

impl Default for ChutesConfig {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
            llm_base: DEFAULT_LLM_BASE.to_string(),
            e2e_invoke: DEFAULT_E2E_INVOKE.to_string(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            default_api_key: None,
            tls_spki_pins: Vec::new(),
            proxy: ProxyPolicy::None,
        }
    }
}

/// Parse `CHUTES_TLS_SPKI_PINS` (comma-separated base64 SHA-256 digests).
///
/// A malformed pin is a startup configuration error: silently trusting the
/// platform roots instead would defeat the point of configuring pins.
fn parse_spki_pins(raw: &str) -> Vec<tls::SpkiPin> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            tls::decode_pin(entry)
                .unwrap_or_else(|error| panic!("invalid CHUTES_TLS_SPKI_PINS entry: {error}"))
        })
        .collect()
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
            tls_spki_pins: env("CHUTES_TLS_SPKI_PINS")
                .map_or_else(Vec::new, |raw| parse_spki_pins(&raw)),
            proxy: ProxyPolicy::parse(env("CHUTES_PROXY").as_deref()),
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

fn status_code(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(api::error_body(message))).into_response()
}

fn render_buffered(reply: api::BufferedReply) -> Response {
    match reply {
        api::BufferedReply::Body {
            status,
            body,
            content_type,
        } => {
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status_code(status);
            if let Some(value) = content_type
                .as_deref()
                .and_then(|ct| HeaderValue::from_str(ct).ok())
            {
                response.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            response
        }
        api::BufferedReply::Error(error) => {
            (status_code(error.status), Json(error.value)).into_response()
        }
    }
}

/// The caller's Chutes key: bearer header first, environment fallback second.
fn caller_api_key(state: &ChutesState, headers: &HeaderMap) -> Option<String> {
    bearer_token(headers).or_else(|| state.config.default_api_key.clone())
}

/// `POST /v1/chutes/chat/completions` — encrypt, invoke, decrypt.
async fn chat_completions(
    State(state): State<ChutesState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Response {
    let Some(api_key) = caller_api_key(&state, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, api::MISSING_KEY_MESSAGE);
    };

    match api::chat(&state.client, &api_key, &payload).await {
        api::ChatOutcome::Stream(stream) => {
            let api::ChatStream {
                status,
                session,
                response,
            } = *stream;
            let mut response = Response::new(decrypted_stream_body(session, response));
            *response.status_mut() = status_code(status);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(api::SSE_CONTENT_TYPE),
            );
            response
        }
        api::ChatOutcome::Json { status, value } => {
            (status_code(status), Json(value)).into_response()
        }
        api::ChatOutcome::Sse { status, text } => (
            status_code(status),
            [(header::CONTENT_TYPE, HeaderValue::from_static(api::SSE_CONTENT_TYPE))],
            text,
        )
            .into_response(),
        api::ChatOutcome::Buffered(reply) => render_buffered(reply),
        api::ChatOutcome::Error(error) => {
            (status_code(error.status), Json(error.value)).into_response()
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
        return error_response(StatusCode::UNAUTHORIZED, api::MISSING_KEY_MESSAGE);
    };
    let query = request.uri().query().map(str::to_owned);
    render_buffered(api::models(&state.client, &api_key, query.as_deref()).await)
}
