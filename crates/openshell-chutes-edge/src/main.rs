// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `openshell-chutes-edge` — a Pingora frontend for the Chutes E2EE proxy.
//!
//! This is the standalone edge deployment of the proxy that otherwise rides the
//! gateway's multiplexed port. Pingora terminates client HTTP; every route is
//! answered locally from [`openshell_server::chutes::api`], so there is no
//! plaintext upstream to forward to. The upstream hop is still the E2EE
//! `POST /e2e/invoke` call made by [`ChutesClient`] with the same SPKI pinning,
//! no-redirect policy and proxy policy as the gateway build.
//!
//! ```sh
//! CHUTES_API_KEY=... CHUTES_EDGE_LISTEN=127.0.0.1:8090 openshell-chutes-edge
//! curl -sN http://127.0.0.1:8090/v1/chutes/models -H "Authorization: Bearer $KEY"
//! ```
//!
//! Configuration is environment-only, shared with the gateway module:
//! `CHUTES_API_KEY`, `CHUTES_API_BASE`, `CHUTES_LLM_BASE`,
//! `CHUTES_E2E_INVOKE`, `CHUTES_TIMEOUT_SECS`, `CHUTES_TLS_SPKI_PINS`,
//! `CHUTES_PROXY`, plus `CHUTES_EDGE_LISTEN` (default `127.0.0.1:8090`).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use openshell_server::chutes::api::{self, BufferedReply, ChatOutcome, ChatStream, Route};
use openshell_server::chutes::stream::{self as sse, SseDecryptor};
use openshell_server::chutes::{ChutesClient, ChutesConfig};
use pingora_core::prelude::*;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session, http_proxy_service};
use serde_json::Value;
use tracing::info;

/// Pingora's default is a per-process server; the edge keeps no per-connection
/// state between `request_filter` and the response it writes itself.
struct EdgeCtx;

/// Upper bound on a client chat payload. Prompts are text; anything larger is
/// either a mistake or an attempt to make the edge buffer without bound.
const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;

const DEFAULT_LISTEN: &str = "127.0.0.1:8090";

struct ChutesEdge {
    config: Arc<ChutesConfig>,
    client: Arc<ChutesClient>,
}

impl ChutesEdge {
    fn new(config: ChutesConfig) -> Self {
        let client = ChutesClient::new(&config);
        Self {
            config: Arc::new(config),
            client: Arc::new(client),
        }
    }

    async fn chat(&self, session: &mut Session) -> pingora_core::Result<()> {
        let raw_auth = session
            .req_header()
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let Some(api_key) = resolve_api_key(&self.config, raw_auth.as_deref()) else {
            return write_json(
                session,
                401,
                &api::error_body(api::MISSING_KEY_MESSAGE),
            )
            .await;
        };

        let body = match read_body(session).await {
            Ok(body) => body,
            Err(error) => {
                return write_json(session, error.status(), &api::error_body(error.message()))
                    .await;
            }
        };
        let payload: Value = match serde_json::from_slice(&body) {
            Ok(payload) => payload,
            Err(error) => {
                return write_json(
                    session,
                    400,
                    &api::error_body(&format!("invalid JSON body: {error}")),
                )
                .await;
            }
        };

        match api::chat(&self.client, &api_key, &payload).await {
            ChatOutcome::Stream(stream) => {
                let ChatStream {
                    status,
                    session: e2ee_session,
                    response,
                } = *stream;
                write_stream(session, status, e2ee_session, response).await
            }
            ChatOutcome::Json { status, value } => write_json(session, status, &value).await,
            ChatOutcome::Sse { status, text } => {
                write_bytes(session, status, Some(api::SSE_CONTENT_TYPE), text.into_bytes()).await
            }
            ChatOutcome::Buffered(reply) => write_buffered(session, reply).await,
            ChatOutcome::Error(error) => write_json(session, error.status, &error.value).await,
        }
    }

    async fn models(&self, session: &mut Session) -> pingora_core::Result<()> {
        let raw_auth = session
            .req_header()
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let Some(api_key) = resolve_api_key(&self.config, raw_auth.as_deref()) else {
            return write_json(
                session,
                401,
                &api::error_body(api::MISSING_KEY_MESSAGE),
            )
            .await;
        };
        let query = session.req_header().uri.query().map(str::to_owned);
        write_buffered(session, api::models(&self.client, &api_key, query.as_deref()).await).await
    }
}

#[async_trait]
impl ProxyHttp for ChutesEdge {
    type CTX = EdgeCtx;

    fn new_ctx(&self) -> Self::CTX {
        EdgeCtx
    }

    /// Every route is answered locally; there is nothing to forward to.
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        Err(Error::new_str(
            "chutes edge answers every route locally; no upstream peer",
        ))
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<bool> {
        let method = session.req_header().method.as_str().to_owned();
        let path = session.req_header().uri.path().to_owned();

        match api::route(&method, &path) {
            Route::ChatCompletions => self.chat(session).await?,
            Route::Models => self.models(session).await?,
            Route::MethodNotAllowed => {
                write_method_not_allowed(session, &path).await?;
            }
            Route::NotFound => {
                write_json(
                    session,
                    404,
                    &api::error_body("unknown chutes proxy route"),
                )
                .await?;
            }
        }
        Ok(true)
    }
}

/// Extract a bearer token from an `Authorization` header value.
fn bearer(value: Option<&str>) -> Option<String> {
    let value = value?;
    let token = value.strip_prefix("Bearer ").map_or("", str::trim);
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Caller's key first, the configured shared key second.
fn resolve_api_key(config: &ChutesConfig, raw_auth: Option<&str>) -> Option<String> {
    bearer(raw_auth).or_else(|| config.default_api_key.clone())
}

/// The method a known path accepts, for the `Allow` header on a 405.
fn allowed_method(path: &str) -> &'static str {
    if path == "/v1/chutes/models" {
        "GET"
    } else {
        "POST"
    }
}

enum BodyError {
    TooLarge(String),
    Read(String),
}

impl BodyError {
    fn status(&self) -> u16 {
        match self {
            Self::TooLarge(_) => 413,
            Self::Read(_) => 400,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::TooLarge(message) | Self::Read(message) => message,
        }
    }
}

async fn read_body(session: &mut Session) -> Result<Vec<u8>, BodyError> {
    let mut body = Vec::new();
    while let Some(chunk) = session
        .read_request_body()
        .await
        .map_err(|error| BodyError::Read(format!("failed reading request body: {error}")))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_REQUEST_BODY {
            return Err(BodyError::TooLarge(format!(
                "request body exceeds {MAX_REQUEST_BODY} bytes"
            )));
        }
    }
    Ok(body)
}

async fn write_buffered(
    session: &mut Session,
    reply: BufferedReply,
) -> pingora_core::Result<()> {
    match reply {
        BufferedReply::Body {
            status,
            body,
            content_type,
        } => write_bytes(session, status, content_type.as_deref(), body).await,
        BufferedReply::Error(error) => write_json(session, error.status, &error.value).await,
    }
}

async fn write_json(
    session: &mut Session,
    status: u16,
    value: &Value,
) -> pingora_core::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    write_bytes(session, status, Some("application/json"), body).await
}

/// Write one complete response with a `Content-Length`.
async fn write_bytes(
    session: &mut Session,
    status: u16,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> pingora_core::Result<()> {
    let mut header = ResponseHeader::build(status, None)?;
    if let Some(content_type) = content_type {
        header.insert_header("content-type", content_type)?;
    }
    header.insert_header("content-length", body.len().to_string())?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(Bytes::from(body)), true)
        .await
}

async fn write_method_not_allowed(
    session: &mut Session,
    path: &str,
) -> pingora_core::Result<()> {
    let body = serde_json::to_vec(&api::error_body("method not allowed")).unwrap_or_default();
    let mut header = ResponseHeader::build(405, None)?;
    header.insert_header("allow", allowed_method(path))?;
    header.insert_header("content-type", "application/json")?;
    header.insert_header("content-length", body.len().to_string())?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(Bytes::from(body)), true)
        .await
}

/// Decrypt the upstream SSE stream chunk by chunk and relay plaintext SSE.
///
/// A mid-stream failure can only be reported as an SSE error event: the 200
/// status and headers are already on the wire, exactly as in the axum frontend.
async fn write_stream(
    session: &mut Session,
    status: u16,
    e2ee_session: openshell_server::chutes::e2ee::E2eeSession,
    response: reqwest::Response,
) -> pingora_core::Result<()> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("content-type", api::SSE_CONTENT_TYPE)?;
    header.insert_header("cache-control", "no-cache")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;

    let mut decryptor = SseDecryptor::new(e2ee_session);
    let mut upstream = response.bytes_stream();
    let mut failed = false;
    while let Some(chunk) = upstream.next().await {
        let text = match chunk {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                failed = true;
                let line = sse::error_event_line(&format!("upstream stream error: {error}"));
                session
                    .write_response_body(Some(Bytes::from(line)), false)
                    .await?;
                break;
            }
        };
        match decryptor.feed(&text) {
            Ok(items) => {
                let mut out = String::new();
                for item in &items {
                    out.push_str(&sse::render(item));
                }
                if !out.is_empty() {
                    session
                        .write_response_body(Some(Bytes::from(out)), false)
                        .await?;
                }
            }
            Err(message) => {
                failed = true;
                let line = sse::error_event_line(&message);
                session
                    .write_response_body(Some(Bytes::from(line)), false)
                    .await?;
                break;
            }
        }
    }

    if !failed {
        match decryptor.flush() {
            Ok(items) => {
                let mut out = String::new();
                for item in &items {
                    out.push_str(&sse::render(item));
                }
                if !out.is_empty() {
                    session
                        .write_response_body(Some(Bytes::from(out)), false)
                        .await?;
                }
            }
            Err(message) => {
                let line = sse::error_event_line(&message);
                session
                    .write_response_body(Some(Bytes::from(line)), false)
                    .await?;
            }
        }
    }

    session.write_response_body(None, true).await
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // reqwest's rustls backend needs a process-wide provider before the first
    // ChutesClient is built, or the TLS builder panics.
    openshell_crypto::tls::ensure_default_provider();

    let listen =
        std::env::var("CHUTES_EDGE_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let config = ChutesConfig::from_env();
    let edge = ChutesEdge::new(config);

    let mut server = Server::new(None).expect("Pingora server init");
    server.bootstrap();

    let mut service = http_proxy_service(&server.configuration, edge);
    service.add_tcp(&listen);
    server.add_service(service);

    info!(%listen, "openshell-chutes-edge listening");
    server.run_forever();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_are_trimmed_and_empty_values_rejected() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc".to_string()));
        assert_eq!(bearer(Some("Bearer   abc  ")), Some("abc".to_string()));
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(None), None);
    }

    #[test]
    fn caller_key_falls_back_to_the_configured_key() {
        let config = ChutesConfig {
            default_api_key: Some("shared".to_string()),
            ..ChutesConfig::default()
        };
        assert_eq!(
            resolve_api_key(&config, Some("Bearer caller")),
            Some("caller".to_string())
        );
        assert_eq!(resolve_api_key(&config, None), Some("shared".to_string()));
    }

    #[test]
    fn method_not_allowed_names_the_supported_method() {
        assert_eq!(allowed_method("/v1/chutes/models"), "GET");
        assert_eq!(allowed_method("/v1/chutes/chat/completions"), "POST");
    }
}
