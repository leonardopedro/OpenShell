// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for the Chutes E2EE proxy.
//!
//! The upstream stub speaks the real protocol (ML-KEM decapsulation and
//! re-encryption via [`server_sim::ChuteInstance`]), so these tests exercise
//! the full handler → client → crypto → SSE paths without hitting the
//! network. A `#[ignore]`d live test runs the same handler against the real
//! Chutes API when `CHUTES_API_KEY` is set.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::chutes::e2ee::server_sim::ChuteInstance;

struct DiscoveryResponder {
    instance: Arc<ChuteInstance>,
}

impl Respond for DiscoveryResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let body = json!({
            "instances": [{
                "instance_id": "inst-test",
                "e2e_pubkey": self.instance.pubkey_b64(),
                "nonces": [self.instance.next_nonce()],
            }],
            "nonce_expires_in": 55,
        });
        ResponseTemplate::new(200).set_body_json(&body)
    }
}

struct InvokeResponder {
    instance: Arc<ChuteInstance>,
    calls: Arc<AtomicUsize>,
    fail_first_nonce: bool,
}

const TEST_SSE: &str = concat!(
    "data: {\"id\":\"cmpl-t\",\"model\":\"m-tee\",\"created\":7,",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
    "\"content\":\"Hello from tee\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
    "data: [DONE]\n\n",
);

/// Headers and content type every invoke must carry; checked by both stubs.
fn missing_required_header(req: &Request) -> Option<String> {
    for name in [
        "authorization",
        "x-chute-id",
        "x-instance-id",
        "x-e2e-nonce",
        "x-e2e-stream",
        "x-e2e-path",
    ] {
        if !req.headers.contains_key(name) {
            return Some(format!("missing header {name}"));
        }
    }
    let octet_stream = req
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("octet-stream"));
    if !octet_stream {
        return Some("content-type not octet-stream".to_string());
    }
    None
}

impl Respond for InvokeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if let Some(error) = missing_required_header(req) {
            return ResponseTemplate::new(400).set_body_string(error);
        }

        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_first_nonce && call == 0 {
            return ResponseTemplate::new(403)
                .set_body_json(json!({"error": {"message": "nonce expired"}}));
        }

        let request_json = match self.instance.decrypt_request(&req.body) {
            Ok(json) => json,
            Err(error) => {
                return ResponseTemplate::new(500).set_body_string(format!("decrypt: {error}"));
            }
        };
        // The proxy must strip the `chutes/` prefix and force stream mode.
        let payload: Value = serde_json::from_str(&request_json).expect("json");
        if payload["stream"] != Value::Bool(true) {
            return ResponseTemplate::new(500).set_body_string("stream not forced true");
        }

        let (init, key) = ChuteInstance::stream_init(&request_json);
        let chunk = ChuteInstance::stream_chunk(&key, TEST_SSE).expect("chunk");
        let body =
            format!("data: {{\"e2e_init\":\"{init}\"}}\n\ndata: {{\"e2e\":\"{chunk}\"}}\n\n");
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }
}

/// Serves the non-SSE flavor of a successful invoke: an opaque encrypted
/// response blob the proxy must open before answering the client.
struct BlobInvokeResponder {
    instance: Arc<ChuteInstance>,
}

impl Respond for BlobInvokeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if let Some(error) = missing_required_header(req) {
            return ResponseTemplate::new(400).set_body_string(error);
        }
        let request_json = match self.instance.decrypt_request(&req.body) {
            Ok(json) => json,
            Err(error) => {
                return ResponseTemplate::new(500).set_body_string(format!("decrypt: {error}"));
            }
        };
        let response_json = json!({
            "id": "cmpl-blob",
            "object": "chat.completion",
            "created": 11,
            "model": "m-tee",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "blob hello"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
        });
        let blob = match ChuteInstance::encrypt_response(&request_json, &response_json.to_string())
        {
            Ok(blob) => blob,
            Err(error) => {
                return ResponseTemplate::new(500).set_body_string(format!("encrypt: {error}"));
            }
        };
        ResponseTemplate::new(200).set_body_raw(blob, "application/octet-stream")
    }
}

async fn setup_upstream_blob() -> MockServer {
    let server = MockServer::start().await;
    let instance = Arc::new(ChuteInstance::new());

    Mock::given(method("GET"))
        .and(path_regex(r"^/e2e/instances/[^/]+$"))
        .respond_with(DiscoveryResponder {
            instance: instance.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/e2e/invoke$"))
        .respond_with(BlobInvokeResponder { instance })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/models$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "m-tee", "chute_id": "ch1", "confidential_compute": true}]
        })))
        .mount(&server)
        .await;

    server
}

async fn setup_upstream(fail_first_nonce: bool) -> (MockServer, Arc<AtomicUsize>) {
    let server = MockServer::start().await;
    let instance = Arc::new(ChuteInstance::new());
    let calls = Arc::new(AtomicUsize::new(0));

    Mock::given(method("GET"))
        .and(path_regex(r"^/e2e/instances/[^/]+$"))
        .respond_with(DiscoveryResponder {
            instance: instance.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/e2e/invoke$"))
        .respond_with(InvokeResponder {
            instance,
            calls: calls.clone(),
            fail_first_nonce,
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/models$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "m-tee", "chute_id": "ch1", "confidential_compute": true}]
        })))
        .mount(&server)
        .await;

    (server, calls)
}

async fn spawn_proxy_with(config: ChutesConfig) -> String {
    // reqwest's rustls backend needs a process crypto provider; production
    // gateways install it during startup (same helper other server tests use).
    openshell_crypto::tls::ensure_default_provider();
    let router = router_with_config(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve proxy");
    });
    format!("http://{addr}")
}

fn upstream_config(server_uri: &str, default_api_key: Option<&str>) -> ChutesConfig {
    ChutesConfig {
        api_base: server_uri.to_string(),
        llm_base: format!("{server_uri}/v1"),
        e2e_invoke: format!("{server_uri}/e2e/invoke"),
        timeout_secs: 30,
        default_api_key: default_api_key.map(str::to_owned),
    }
}

fn chat_request(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 64,
    })
}

async fn post_chat(proxy: &str, body: &Value, auth: bool) -> reqwest::Response {
    let mut request = reqwest::Client::new().post(format!("{proxy}/v1/chutes/chat/completions"));
    if auth {
        request = request.header(header::AUTHORIZATION, "Bearer caller-supplied-key");
    }
    request.json(body).send().await.expect("send chat request")
}

#[tokio::test]
async fn proxy_decrypts_streaming_response() {
    let (server, _calls) = setup_upstream(false).await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", true), false).await;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");
    assert_eq!(status, 200, "body: {body}");
    assert!(content_type.contains("text/event-stream"), "{content_type}");
    assert!(body.contains("Hello from tee"), "body: {body}");
    assert!(body.contains("data: [DONE]"), "body: {body}");
    assert!(!body.contains("e2e_init"), "body: {body}");
    assert!(!body.contains("\"e2e\":\""), "body: {body}");
}

#[tokio::test]
async fn proxy_accumulates_non_streaming_request() {
    let (server, _calls) = setup_upstream(false).await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", false), false).await;
    assert_eq!(response.status(), 200);
    let completion: Value = response.json().await.expect("json completion");

    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["model"], "m-tee");
    assert_eq!(completion["created"], 7);
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        "Hello from tee"
    );
    assert_eq!(completion["choices"][0]["message"]["role"], "assistant");
    assert_eq!(completion["choices"][0]["finish_reason"], "stop");
    assert_eq!(completion["usage"]["total_tokens"], 5);
}

#[tokio::test]
async fn proxy_retries_nonce_rejected_invoke() {
    let (server, calls) = setup_upstream(true).await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", true), false).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("Hello from tee"), "body: {body}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one 403 + one retry");
}

#[tokio::test]
async fn proxy_rejects_missing_api_key() {
    let (server, _calls) = setup_upstream(false).await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), None)).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", true), false).await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn proxy_prefers_caller_supplied_key() {
    let (server, _calls) = setup_upstream(false).await;
    // No environment fallback; the caller's bearer header must be enough.
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), None)).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", true), true).await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn models_endpoint_passes_upstream_list_through() {
    let (server, _calls) = setup_upstream(false).await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = reqwest::Client::new()
        .get(format!("{proxy}/v1/chutes/models"))
        .send()
        .await
        .expect("send models request");
    assert_eq!(response.status(), 200);
    let list: Value = response.json().await.expect("json list");
    assert_eq!(list["data"][0]["id"], "m-tee");
}

#[tokio::test]
async fn unknown_model_maps_to_bad_gateway() {
    let (server, _calls) = setup_upstream(false).await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = post_chat(&proxy, &chat_request("chutes/does-not-exist", true), false).await;
    assert_eq!(response.status(), 502);
    let error: Value = response.json().await.expect("json error");
    assert_eq!(error["error"]["type"], "chutes_proxy_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("not found")),
        "{error}"
    );
}

#[tokio::test]
async fn proxy_opens_encrypted_blob_for_json_client() {
    let server = setup_upstream_blob().await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", false), false).await;
    let status = response.status();
    let body = response.text().await.expect("body");
    assert_eq!(status, 200, "body: {body}");

    let completion: Value = serde_json::from_str(&body).expect("json completion");
    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["choices"][0]["message"]["content"], "blob hello");
    assert_eq!(completion["usage"]["total_tokens"], 3);
}

#[tokio::test]
async fn proxy_wraps_encrypted_blob_in_sse_for_stream_client() {
    let server = setup_upstream_blob().await;
    let proxy = spawn_proxy_with(upstream_config(&server.uri(), Some("env-key"))).await;

    let response = post_chat(&proxy, &chat_request("chutes/m-tee", true), false).await;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");
    assert_eq!(status, 200, "body: {body}");
    assert!(content_type.contains("text/event-stream"), "{content_type}");
    assert!(body.contains("blob hello"), "body: {body}");
    assert!(body.contains("data: [DONE]"), "body: {body}");
    assert!(!body.contains("e2e_init"), "body: {body}");
}

#[tokio::test]
#[ignore = "requires CHUTES_API_KEY and network access to api.chutes.ai"]
async fn live_e2ee_stream_roundtrip() {
    let key = std::env::var("CHUTES_API_KEY")
        .expect("CHUTES_API_KEY must be set to run the live chutes test");
    let config = ChutesConfig {
        default_api_key: Some(key),
        ..ChutesConfig::default()
    };
    let proxy = spawn_proxy_with(config).await;

    let response = post_chat(
        &proxy,
        &chat_request("chutes/zai-org/GLM-5.1-TEE", true),
        false,
    )
    .await;
    assert_eq!(response.status(), 200, "live invoke must succeed");

    let body = response.text().await.expect("body");
    assert!(body.contains("data:"), "body: {body}");
    assert!(!body.contains("e2e_init"), "body must be decrypted: {body}");
}
