// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Chutes upstream client: model resolution, E2EE instance discovery, and
//! the encrypted invoke call.
//!
//! Behavior follows the live-tested `claurst` reference:
//!
//! * `GET {llm_base}/models` resolves a model name (optionally `chutes/`
//!   prefixed) to its `chute_id`, cached for the process lifetime.
//! * `GET {api_base}/e2e/instances/{chute_id}` discovers instances; each
//!   instance carries a base64 ML-KEM public key and single-use nonces
//!   (~55s expiry), so discovery runs fresh for every invoke and is never
//!   cached.
//! * `POST {e2e_invoke}` sends the encrypted blob with the nonce headers;
//!   a `403` whose body mentions the nonce is retried once with a fresh
//!   discovery round.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

use super::ChutesConfig;
use super::ProxyPolicy;
use super::e2ee::{self, E2eeSession};

/// One discoverable chute instance.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceInfo {
    pub instance_id: String,
    pub e2e_pubkey: String,
    pub nonces: Vec<String>,
}

/// Discovery response for `GET /e2e/instances/{chute_id}`.
#[derive(Debug, Deserialize)]
pub struct InstancesResponse {
    pub instances: Vec<InstanceInfo>,
    /// Seconds until the issued nonces expire.
    #[allow(dead_code)]
    pub nonce_expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct ModelDetail {
    id: String,
    chute_id: String,
    #[allow(dead_code)]
    confidential_compute: bool,
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelDetail>,
}

/// Errors surfaced by the upstream client.
#[derive(Debug, thiserror::Error)]
pub enum ChutesError {
    /// Discovery/model-resolution/protocol failure against the Chutes API.
    #[error("chutes API error: {0}")]
    Api(String),
    /// Crypto failure while building or opening an E2EE blob.
    #[error("crypto error: {0}")]
    Crypto(#[from] e2ee::E2eeError),
    /// Transport failure talking to the Chutes API.
    #[error("request error: {0}")]
    Request(#[from] reqwest::Error),
    /// The upstream invoke endpoint returned a non-2xx status.
    #[error("upstream status {status}")]
    Upstream {
        status: u16,
        body: String,
        content_type: Option<String>,
    },
}

/// Outcome of a successful (HTTP 2xx) invoke: the session needed to open the
/// response, and the raw response for the handler to consume.
pub struct InvokeOutcome {
    pub session: E2eeSession,
    pub response: reqwest::Response,
}

pub struct ChutesClient {
    http: reqwest::Client,
    api_base: String,
    llm_base: String,
    e2e_invoke: String,
    model_cache: Mutex<HashMap<String, String>>,
}

/// Reject endpoints that would put the API key or the E2EE handshake on a
/// cleartext, off-host hop. Loopback `http` is allowed so tests and local
/// upstreams do not need TLS.
fn secure_endpoint(name: &str, raw: &str) -> String {
    let url = url::Url::parse(raw)
        .unwrap_or_else(|error| panic!("{name} is not a valid URL ({raw}): {error}"));
    let loopback = url
        .host_str()
        .is_some_and(|host| host == "localhost" || host == "::1" || host.starts_with("127."));
    match url.scheme() {
        "https" => raw.to_string(),
        "http" if loopback => raw.to_string(),
        scheme => panic!("{name} must use https (or http on loopback), got {scheme}: {raw}"),
    }
}

impl ChutesClient {
    pub fn new(config: &ChutesConfig) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            // Never follow a redirect: a 30x from the API host could move the
            // bearer token, and the E2EE handshake body is bound to one host.
            .redirect(reqwest::redirect::Policy::none());
        builder = match &config.proxy {
            // Explicitly ignore HTTP(S)_PROXY: a proxy the operator did not
            // choose should not sit in the "end-to-end" path.
            ProxyPolicy::None => builder.no_proxy(),
            ProxyPolicy::Environment => builder,
            ProxyPolicy::Url(url) => builder.proxy(reqwest::Proxy::all(url).unwrap_or_else(
                |error| panic!("CHUTES_PROXY is not a valid proxy URL ({url}): {error}"),
            )),
        };
        if !config.tls_spki_pins.is_empty() {
            let tls = super::tls::pinned_client_config(&config.tls_spki_pins)
                .unwrap_or_else(|error| panic!("failed to build pinned chutes tls config: {error}"));
            builder = builder.use_preconfigured_tls(tls);
        }
        let http = builder
            .build()
            .expect("failed to build chutes http client");
        Self {
            http,
            api_base: secure_endpoint("CHUTES_API_BASE", &config.api_base),
            llm_base: secure_endpoint("CHUTES_LLM_BASE", &config.llm_base),
            e2e_invoke: secure_endpoint("CHUTES_E2E_INVOKE", &config.e2e_invoke),
            model_cache: Mutex::new(HashMap::new()),
        }
    }

    fn auth(api_key: &str) -> String {
        format!("Bearer {api_key}")
    }

    /// Passthrough fetch of the upstream OpenAI-compatible model list.
    pub async fn get_models(
        &self,
        api_key: &str,
        query: Option<&str>,
    ) -> Result<reqwest::Response, ChutesError> {
        let mut url = format!("{}/models", self.llm_base);
        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, Self::auth(api_key))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChutesError::Upstream {
                status: status.as_u16(),
                body,
                content_type: Some("application/json".to_string()),
            });
        }
        Ok(resp)
    }

    /// Resolve `model` (optionally `chutes/`-prefixed) to a chute id.
    pub async fn resolve_chute_id(
        &self,
        model: &str,
        api_key: &str,
    ) -> Result<String, ChutesError> {
        if let Some(id) = self
            .model_cache
            .lock()
            .expect("model cache poisoned")
            .get(model)
        {
            return Ok(id.clone());
        }

        let url = format!("{}/models", self.llm_base);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, Self::auth(api_key))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ChutesError::Api(format!("model lookup failed: {status}")));
        }
        let list: ModelListResponse = resp
            .json()
            .await
            .map_err(|e| ChutesError::Api(format!("model list parse failed: {e}")))?;

        let wanted = model.strip_prefix("chutes/").unwrap_or(model);
        let found = list
            .data
            .iter()
            .find(|m| m.id == wanted || m.id == model)
            .ok_or_else(|| ChutesError::Api(format!("model {model} not found")))?;
        if !found.confidential_compute {
            warn!(model = %model, "model not marked confidential_compute");
        }

        self.model_cache
            .lock()
            .expect("model cache poisoned")
            .insert(model.to_string(), found.chute_id.clone());
        Ok(found.chute_id.clone())
    }

    /// Fresh discovery for `chute_id` — nonces are single-use, so this is
    /// called once per invoke attempt and never cached.
    async fn discover_instances(
        &self,
        chute_id: &str,
        api_key: &str,
    ) -> Result<InstancesResponse, ChutesError> {
        let url = format!("{}/e2e/instances/{chute_id}", self.api_base);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, Self::auth(api_key))
            .header(reqwest::header::CACHE_CONTROL, "no-cache, no-store")
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ChutesError::Api("chute does not support E2EE".to_string()));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChutesError::Api(format!(
                "discovery failed: {status} - {body}"
            )));
        }
        let info: InstancesResponse = resp
            .json()
            .await
            .map_err(|e| ChutesError::Api(format!("discovery parse failed: {e}")))?;
        debug!(
            chute_id = %chute_id,
            num_instances = info.instances.len(),
            nonce_expires = info.nonce_expires_in,
            "e2ee instances discovered"
        );
        Ok(info)
    }

    /// Encrypt `payload_json` and invoke the E2EE endpoint, retrying once on
    /// a nonce-related `403` with a fresh discovery round.
    pub async fn invoke(
        &self,
        model: &str,
        payload_json: &str,
        api_key: &str,
    ) -> Result<InvokeOutcome, ChutesError> {
        let chute_id = self.resolve_chute_id(model, api_key).await?;

        let mut last_upstream: Option<ChutesError> = None;
        for attempt in 1..=2u8 {
            let instances = self.discover_instances(&chute_id, api_key).await?;
            let instance = instances
                .instances
                .first()
                .ok_or_else(|| ChutesError::Api("no E2EE instances available".to_string()))?;
            let nonce = instance
                .nonces
                .first()
                .ok_or_else(|| ChutesError::Api("no nonces available for instance".to_string()))?;
            let instance_pk = e2ee::parse_instance_pubkey(&instance.e2e_pubkey)?;

            let session = E2eeSession::new(&instance_pk);
            let blob = session.encrypt_request(payload_json)?;

            let resp = self
                .http
                .post(&self.e2e_invoke)
                .header(reqwest::header::AUTHORIZATION, Self::auth(api_key))
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header("X-Chute-Id", &chute_id)
                .header("X-Instance-Id", &instance.instance_id)
                .header("X-E2E-Nonce", nonce)
                .header("X-E2E-Stream", "true")
                .header("X-E2E-Path", "/v1/chat/completions")
                .body(blob)
                .send()
                .await?;

            if resp.status().is_success() {
                return Ok(InvokeOutcome {
                    session,
                    response: resp,
                });
            }

            let status = resp.status();
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = resp.text().await.unwrap_or_default();

            let nonce_retry =
                status == reqwest::StatusCode::FORBIDDEN && body.to_lowercase().contains("nonce");
            if nonce_retry && attempt < 2 {
                warn!(
                    attempt,
                    body = &body[..body.len().min(200)],
                    "e2ee invoke 403, retrying with fresh discovery"
                );
                last_upstream = Some(ChutesError::Upstream {
                    status: status.as_u16(),
                    body: body.clone(),
                    content_type,
                });
                continue;
            }
            return Err(ChutesError::Upstream {
                status: status.as_u16(),
                body,
                content_type,
            });
        }

        Err(last_upstream
            .unwrap_or_else(|| ChutesError::Api("e2ee invoke failed after retries".to_string())))
    }
}
