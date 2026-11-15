// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the `openshell-chutes-edge` binary.
//!
//! Each test spawns the real server and points it at a wiremock upstream that
//! speaks the E2EE protocol: discovery hands out a real ML-KEM-768 public key,
//! and the invoke stub decapsulates the request and re-encrypts the response
//! using [`ChuteInstance`]. Nothing is mocked inside the edge, so the tests
//! cover routing, pingora's body handling, the upstream client, and the
//! decryption relay together.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openshell_server::chutes::e2ee::server_sim::ChuteInstance;
use serde_json::{Value, json};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TEST_SSE: &str = concat!(
    "data: {\"id\":\"cmpl-edge\",\"model\":\"m-tee\",\"created\":7,",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
    "\"content\":\"Hello from tee\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
    "data: [DONE]\n\n",
);

struct DiscoveryResponder {
    instance: Arc<ChuteInstance>,
}

impl Respond for DiscoveryResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let body = json!({
            "instances": [{
                "instance_id": "inst-edge",
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
}

impl Respond for InvokeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let request_json = match self.instance.decrypt_request(&req.body) {
            Ok(request_json) => request_json,
            Err(error) => {
                return ResponseTemplate::new(500).set_body_string(format!("decrypt: {error}"));
            }
        };
        let payload: Value = serde_json::from_str(&request_json).expect("json request");
        if payload["stream"] != Value::Bool(true) {
            return ResponseTemplate::new(500).set_body_string("stream not forced true");
        }

        let (init, key) = ChuteInstance::stream_init(&request_json);
        let chunk = match ChuteInstance::stream_chunk(&key, TEST_SSE) {
            Ok(chunk) => chunk,
            Err(error) => {
                return ResponseTemplate::new(500).set_body_string(format!("chunk: {error}"));
            }
        };
        let body =
            format!("data: {{\"e2e_init\":\"{init}\"}}\n\ndata: {{\"e2e\":\"{chunk}\"}}\n\n");
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }
}

/// Start a fake Chutes API: discovery, encrypted invoke, and the model list.
async fn start_upstream() -> MockServer {
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
        .respond_with(InvokeResponder { instance })
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

/// A running edge, killed when the test ends.
struct Edge {
    child: Child,
    base: String,
}

impl Drop for Edge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Spawn the built binary against `upstream`, with `CHUTES_API_KEY` set only
/// when `env_key` is given.
fn spawn_edge(upstream: &str, env_key: Option<&str>) -> Edge {
    // The test's own reqwest client needs a process crypto provider too.
    openshell_crypto::tls::ensure_default_provider();
    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_openshell-chutes-edge"));
    // The developer running the suite may export a live key or live endpoints;
    // the edge must only see the test configuration.
    for (name, _) in std::env::vars() {
        if name.starts_with("CHUTES_") {
            command.env_remove(&name);
        }
    }
    command
        .env("CHUTES_EDGE_LISTEN", format!("127.0.0.1:{port}"))
        .env("CHUTES_API_BASE", upstream)
        .env("CHUTES_LLM_BASE", format!("{upstream}/v1"))
        .env("CHUTES_E2E_INVOKE", format!("{upstream}/e2e/invoke"))
        .env("CHUTES_TIMEOUT_SECS", "30")
        .env("CHUTES_TLS_SPKI_PINS", "")
        .env("CHUTES_PROXY", "none")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(key) = env_key {
        command.env("CHUTES_API_KEY", key);
    }
    let child = command.spawn().expect("spawn openshell-chutes-edge");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "edge did not start listening on 127.0.0.1:{port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    Edge {
        child,
        base: format!("http://127.0.0.1:{port}"),
    }
}

fn chat_request() -> Value {
    json!({
        "model": "chutes/m-tee",
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 64,
    })
}

async fn post_chat(edge: &Edge, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chutes/chat/completions", edge.base))
        .json(body)
        .send()
        .await
        .expect("send chat request")
}

#[tokio::test]
async fn relays_a_decrypted_stream_from_an_e2ee_instance() {
    let upstream = start_upstream().await;
    let edge = spawn_edge(&upstream.uri(), Some("env-key"));

    let response = post_chat(&edge, &chat_request()).await;
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");

    assert_eq!(status, 200, "body: {body}");
    assert!(content_type.contains("text/event-stream"), "{content_type}");
    assert!(body.contains("Hello from tee"), "body: {body}");
    assert!(body.contains("data: [DONE]"), "body: {body}");
    assert!(!body.contains("e2e_init"), "body must be decrypted: {body}");
}

#[tokio::test]
async fn passes_the_upstream_model_list_through() {
    let upstream = start_upstream().await;
    let edge = spawn_edge(&upstream.uri(), Some("env-key"));

    let response = reqwest::Client::new()
        .get(format!("{}/v1/chutes/models", edge.base))
        .send()
        .await
        .expect("send models request");
    assert_eq!(response.status(), 200);
    let list: Value = response.json().await.expect("json list");
    assert_eq!(list["data"][0]["id"], "m-tee");
}

#[tokio::test]
async fn refuses_a_request_without_any_api_key() {
    let upstream = start_upstream().await;
    let edge = spawn_edge(&upstream.uri(), None);

    let response = post_chat(&edge, &chat_request()).await;
    assert_eq!(response.status(), 401);
    let error: Value = response.json().await.expect("json error");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("CHUTES_API_KEY")),
        "{error}"
    );
}

#[tokio::test]
async fn rejects_unknown_routes_and_wrong_methods() {
    let upstream = start_upstream().await;
    let edge = spawn_edge(&upstream.uri(), Some("env-key"));
    let client = reqwest::Client::new();

    let unknown = client
        .get(format!("{}/v1/not-a-route", edge.base))
        .send()
        .await
        .expect("send unknown route");
    assert_eq!(unknown.status(), 404);

    let wrong_method = client
        .get(format!("{}/v1/chutes/chat/completions", edge.base))
        .send()
        .await
        .expect("send wrong method");
    assert_eq!(wrong_method.status(), 405);
    assert_eq!(
        wrong_method
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );

    let wrong_method = client
        .post(format!("{}/v1/chutes/models", edge.base))
        .send()
        .await
        .expect("send wrong method");
    assert_eq!(wrong_method.status(), 405);
    assert_eq!(
        wrong_method
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok()),
        Some("GET")
    );
}

#[tokio::test]
async fn rejects_a_malformed_chat_body() {
    let upstream = start_upstream().await;
    let edge = spawn_edge(&upstream.uri(), Some("env-key"));

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chutes/chat/completions", edge.base))
        .body("{not json")
        .send()
        .await
        .expect("send malformed body");
    assert_eq!(response.status(), 400);
}
