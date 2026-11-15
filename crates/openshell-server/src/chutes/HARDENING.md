# Chutes E2EE client — TLS/pinning and nonce-replay hardening review

Scope: `chutes/client.rs`, `chutes/e2ee.rs`, `chutes/stream.rs` (the upstream
side of the proxy). Level 2 review — beyond "does it use TLS at all" to "what
does the E2EE layer actually protect against, given how the trust anchors and
nonces are handled".

Reference parity: the `zentinel` (`agents/ai-gateway`) and `claurst`
implementations have the same properties listed under *Gaps*; nothing here is a
regression introduced by this port, but the gaps are worth closing.

## Level 1 — baseline (currently satisfied)

- **Transport.** `reqwest::Client::builder()` with rustls and the platform root
  store. No `danger_accept_invalid_certs`, no `danger_accept_invalid_hostnames`,
  no custom verifier. Hostname and chain are verified by webpki.
- **Timeouts.** `connect_timeout(10s)` and an overall request timeout
  (`CHUTES_TIMEOUT_SECS`), so a stalled handshake cannot pin a task forever.
- **Request nonce discipline.** Nonces are single-use, so `discover_instances`
  runs fresh for every invoke attempt and is never cached; the 403 nonce retry
  re-discovers rather than reusing the rejected nonce.
- **Session isolation.** Every attempt creates a new ephemeral ML-KEM-768
  keypair and a new request encapsulation, so a response blob is bound to one
  session and old blobs cannot be replayed into a new one.
- **No plaintext fallback.** The encrypted invoke is the only path; there is no
  branch that sends the prompt unencrypted when E2EE is unavailable.

## Level 2 — gaps found

Ordered by impact. Severity assumes an attacker who can present a
CA-valid certificate for `api.chutes.ai` (corporate TLS interception, a
misissuing CA, or DNS/BGP hijack combined with any of those).

### H1. No pinning or attestation check — E2EE trust collapses to Web PKI (high)

Discovery (`GET /e2e/instances/{chute_id}`) returns the instance's
`e2e_pubkey`. That key is the *only* thing standing between the prompt and a
network intermediary: whoever controls the response controls which ML-KEM key
the prompt gets encrypted to, and can then decrypt it, re-encrypt to the real
instance, and relay the answer. TLS protects that response, so the effective
trust anchor is the CA set, not the TEE.

Mitigations, cheapest first:

1. **SPKI pin** (implemented): `CHUTES_TLS_SPKI_PINS` takes comma-separated
   base64 SHA-256 digests of the leaf `SubjectPublicKeyInfo`, enforced by the
   verifier in `tls.rs`. Chain and hostname validation still run first, so a
   pin can only narrow what the platform roots already accept. Unset means
   "trust the platform roots" — the default for compatibility with the
   reference implementations. Malformed pins are a startup error rather than
   a silent downgrade.
2. **Pubkey TOFU** (not implemented): remember `chute_id -> instance_id -> e2e_pubkey` on first
   successful use and warn or fail when discovery later returns a different key
   for the same instance id. This does not stop a first-use MITM but turns
   silent long-term interception into a detectable event.
3. **Attestation** (not implemented): when Chutes exposes a signed attestation/report digest for
   the instance, verify it binds `e2e_pubkey` to the expected enclave
   measurement, and keep it in the discovery cache alongside the key.

### H2. Redirects and proxy env vars can move credentials off-host (medium)

**Implemented.** The client sets `redirect(reqwest::redirect::Policy::none())`,
so a `30x` from the API host can no longer move `Authorization` (or the
encrypted body) to another origin. Endpoints are validated at client
construction: `api_base`, `llm_base` and `e2e_invoke` must be `https`, with
`http` accepted only for loopback hosts (which keeps the wiremock-based tests
and local upstreams working).

Proxy use is now explicit: `CHUTES_PROXY` accepts `none` (the default, which
calls `no_proxy()` and ignores `HTTP(S)_PROXY`), `env` (honour the environment
like a generic HTTP client), or a proxy URL that all upstream requests go
through. A proxy the operator did not choose therefore never sits inside the
"end-to-end" path.

### H3. Stream chunks are neither deduplicated nor ordered (medium)

Each `{"e2e": ...}` chunk is ChaCha20-Poly1305 under the stream key with a fresh
random 96-bit nonce, but `SseDecryptor` decrypts and forwards whatever arrives.
A relay can therefore:

- **replay** a chunk — the same ciphertext decrypts again and its text is
  emitted twice (duplicated tokens, duplicated tool-call deltas);
- **drop or reorder** chunks — truncation and scrambled output are accepted as
  normal, since nothing binds a chunk to its position;
- **inject** a chunk from an earlier part of the same stream.

The AEAD stops forgery but not reordering.

**Partially implemented.** `SseDecryptor` now keeps a `HashSet` of accepted
chunk nonces and fails the stream on a duplicate (`e2e stream chunk replay
detected`), which turns token duplication by a relay into a visible error.
Dropping and reordering are still accepted, because the protocol carries no
sequence number or chunk counter; revisit if the server adds one.

### H4. No zeroization of session secrets (low)

`E2eeSession` holds the ephemeral ML-KEM secret key and the request shared
secret as plain byte arrays; they are freed but not cleared. Recommend
`zeroize` on `Drop` (and `zeroize` for the derived AEAD keys, which currently
live on the stack only).

### H5. 403 "nonce" heuristic is attacker-triggerable (low)

The retry condition is `status == 403 && body.contains("nonce")` on a
lowercased body. Anything that can make the invoke return such a 403 forces a
second discovery round and a second encrypted invoke — doubling upstream cost
per request. Prefer a structured error code from the upstream error body, and
cap retries at the current single retry (already the case).

### H6. Minor

- `nonce_expires_in` is parsed and logged but not used; a long
  `CHUTES_TIMEOUT_SECS` can produce an invoke whose nonce expired in flight,
  which then costs a full retry. Consider clamping the effective timeout to
  `nonce_expires_in` when available.
- `model_cache` is an unbounded per-process `HashMap` keyed by caller-supplied
  model strings; a caller cycling model names grows it without limit. Bound it
  or key it on a fixed model allowlist.
- `confidential_compute: false` only logs a warning. If the gateway is relied
  on for confidentiality guarantees, make this a fail-closed config option.
- Upstream error bodies are logged (truncated to 200 bytes) and returned to the
  caller verbatim; they are server-side JSON, not prompt content, but confirm
  no echo of the decrypted request ever lands in one.

## Not a finding

- Random 96-bit ChaCha nonces per chunk are within AEAD safe-use limits for the
  message counts one stream produces; the issue in H3 is replay/ordering, not
  collision.
- Request nonce reuse across attempts does not occur: each attempt calls
  discovery again, and only `nonces[0]` of a fresh response is used.
