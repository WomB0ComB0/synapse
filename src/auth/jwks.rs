//! JWKS-endpoint RS256 verification: rotating asymmetric public keys fetched over HTTPS.
//!
//! Unlike the statically-configured [`crate::config::Config::auth_jwt_public_key`], the verifying key
//! SET is fetched from a JWKS endpoint and refreshed on rotation. A bearer token's `kid` header
//! selects the key. The service holds only PUBLIC keys, so — like the static RS256 mode — it CANNOT
//! mint tokens, and RS256 is pinned (closing `alg: none` and the RS256->HS256 confusion attack).
//!
//! **Lazy cache + throttled refetch-on-unknown-`kid`** (the chosen rotation model): keys are cached
//! in memory and fetched lazily on first use. A token whose `kid` is not cached triggers at most one
//! refetch per [`JwksVerifier::min_refetch`] window — so a burst of unknown-`kid` tokens can't hammer
//! the endpoint (an unknown-`kid` DoS), while a genuine rotation is picked up within the window. The
//! refetch is single-flighted by a fetch-gate mutex, so a concurrent burst collapses to a SINGLE
//! fetch (no thundering herd) — WITHOUT holding the key-cache lock across the network await, so
//! cached-`kid` verification is never blocked by an in-flight fetch. Verification is fail-closed: any
//! missing/unknown key, fetch failure, or bad signature is [`Error::Unauthorized`] — and a cached
//! `kid` keeps verifying (never blocked, never failed) even during a JWKS outage (only new/rotated
//! `kid`s need the endpoint).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode_header, Algorithm, DecodingKey};
use tokio::sync::{Mutex, RwLock};

use crate::error::Error;

use super::{bearer_token, verify_token, Principal};

/// A JWKS document larger than this is refused — a well-formed JWK Set is a few KB; this caps a
/// pathological/compromised endpoint (the request timeout bounds a slow one).
const MAX_JWKS_BYTES: usize = 1 << 20; // 1 MiB

/// Parse a fetched JWK Set into verifying keys by `kid` (done ONCE per fetch, off the per-request hot
/// path). A JWK with no `kid` (unselectable) or one that isn't a usable key is DROPPED — a token for
/// that `kid` then fails closed (`401`), same as an unknown key.
fn parse_keys(set: JwkSet) -> HashMap<String, DecodingKey> {
    let mut keys = HashMap::new();
    for jwk in set.keys {
        if let Some(kid) = jwk.common.key_id.clone() {
            if let Ok(key) = DecodingKey::from_jwk(&jwk) {
                keys.insert(kid, key);
            }
        }
    }
    keys
}

/// Verifies bearer tokens against RS256 public keys fetched from a JWKS endpoint, with a lazy,
/// throttled, rotation-aware in-memory cache. Built once at startup and shared via `Arc`.
pub struct JwksVerifier {
    /// The JWKS endpoint URL (validated `https://` at config load).
    url: String,
    /// rustls HTTP client (no redirects — a JWKS fetch must never be silently redirected elsewhere).
    client: reqwest::Client,
    /// Minimum interval between refetches (throttles unknown-`kid` refetches).
    min_refetch: Duration,
    /// The pre-parsed verifying keys + when they were last fetched.
    cache: RwLock<JwksCache>,
    /// Single-flight gate: serializes refetches so a concurrent unknown-`kid` burst collapses to ONE
    /// fetch, WITHOUT holding `cache` across the network await (so cached reads stay unblocked).
    fetch_gate: Mutex<()>,
}

#[derive(Default)]
struct JwksCache {
    /// Pre-parsed verifying keys by `kid`, built ONCE per fetch — `DecodingKey::from_jwk` (which
    /// base64url-decodes the RSA components) is kept OFF the per-request hot path. A `kid` absent
    /// here is unknown/unusable and fails closed.
    keys: HashMap<String, DecodingKey>,
    /// When we last ATTEMPTED a fetch (set on success AND failure, so a failing endpoint is also
    /// throttled). `None` means "never attempted" — the first unknown-`kid` request always fetches.
    last_fetch: Option<Instant>,
}

impl JwksVerifier {
    /// Build the verifier (no network I/O — keys are fetched lazily on first use). Mirrors the
    /// embedder/MCP client conventions: bounded request + connect timeouts, redirects disabled.
    pub fn new(url: String, timeout_secs: u64, min_refetch_secs: u64) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build reqwest client (TLS backend?)");
        Self {
            url,
            client,
            min_refetch: Duration::from_secs(min_refetch_secs),
            cache: RwLock::new(JwksCache::default()),
            fetch_gate: Mutex::new(()),
        }
    }

    /// Verify a bearer token against the (possibly-refetched) JWKS and return the [`Principal`] from
    /// its signed claims. Fail-closed on any failure.
    pub async fn principal_from_bearer(
        &self,
        headers: &HeaderMap,
        audience: Option<&str>,
        issuer: Option<&str>,
    ) -> Result<Principal, Error> {
        let token = bearer_token(headers).ok_or(Error::Unauthorized)?;
        // Read the (still-unverified) header only to select the key by `kid`. `kid` is REQUIRED: a
        // rotating multi-key JWKS is ambiguous without it.
        let header = decode_header(token).map_err(|_| Error::Unauthorized)?;
        // Defense in depth: reject anything but RS256 at the header before selecting a key. The final
        // `decode` also pins RS256, but this rejects `alg: none` / an HS256-confusion attempt early.
        if header.alg != Algorithm::RS256 {
            return Err(Error::Unauthorized);
        }
        let kid = header.kid.as_deref().ok_or(Error::Unauthorized)?;

        // Fast path: the `kid` is already cached — an O(1) map lookup + a cheap key clone under a
        // brief read lock, never blocked by an in-flight refetch (which never holds the cache lock
        // across its network await), no cryptographic parse, no network.
        if let Some(key) = self.cached_key(kid).await {
            return verify_token(token, &key, Algorithm::RS256, audience, issuer);
        }
        // Slow path: unknown `kid` — throttled refetch under the write lock, then verify (or 401).
        let key = self.refetch_and_get(kid).await?;
        verify_token(token, &key, Algorithm::RS256, audience, issuer)
    }

    /// Look up the pre-parsed [`DecodingKey`] for `kid`, WITHOUT fetching. `None` means "not cached"
    /// (the caller may then refetch). O(1) map lookup + a cheap clone — no cryptographic parse.
    async fn cached_key(&self, kid: &str) -> Option<DecodingKey> {
        self.cache.read().await.keys.get(kid).cloned()
    }

    /// Single-flight refetch for an unknown `kid`: serialize via the fetch-gate (so a concurrent
    /// burst collapses to ONE fetch) but do the network `await` WITHOUT holding the cache lock, so
    /// cached-`kid` reads are never blocked by an in-flight fetch. Returns the key for `kid`, or
    /// `Unauthorized` if it is still absent (unknown key) or the fetch failed (fail-closed).
    async fn refetch_and_get(&self, kid: &str) -> Result<DecodingKey, Error> {
        // Only one fetcher at a time. While we hold this gate we do NOT hold `cache` across the
        // network await, so other requests' cached-`kid` reads proceed unblocked.
        let _gate = self.fetch_gate.lock().await;
        // Re-check + throttle under a BRIEF read lock: a fetcher ahead of us on the gate may have
        // just populated our `kid`, or fetched recently enough that we should not fetch again.
        {
            let cache = self.cache.read().await;
            if let Some(key) = cache.keys.get(kid) {
                return Ok(key.clone());
            }
            if cache
                .last_fetch
                .is_some_and(|t| t.elapsed() < self.min_refetch)
            {
                // Throttled: within the min-refetch window, so don't hit the endpoint — fail closed.
                return Err(Error::Unauthorized);
            }
        }
        // Fetch WITHOUT holding the cache lock (the whole point of the gate).
        let fetched = self.fetch_jwks().await;
        // Store under a brief write lock, then resolve the key for `kid`.
        let mut cache = self.cache.write().await;
        match fetched {
            Ok(set) => {
                // Parse every key ONCE here, off the per-request hot path; a rotation replaces the
                // whole set (a `kid` dropped from the endpoint stops verifying, as intended).
                cache.keys = parse_keys(set);
                cache.last_fetch = Some(Instant::now());
            }
            Err(e) => {
                // Record the ATTEMPT so a failing endpoint is throttled too, then fail closed. Any
                // previously-cached keys remain usable for their own `kid`s (we don't clear on error).
                cache.last_fetch = Some(Instant::now());
                tracing::warn!(error = %e, "JWKS refetch failed; rejecting unknown-kid token");
            }
        }
        cache.keys.get(kid).cloned().ok_or(Error::Unauthorized)
    }

    /// Fetch + parse the JWKS document. Errors (transport, non-2xx, oversize, malformed) are returned
    /// for the caller to log and fail closed on.
    async fn fetch_jwks(&self) -> Result<JwkSet, anyhow::Error> {
        let mut resp = self
            .client
            .get(&self.url)
            .header("accept", "application/json")
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("JWKS endpoint returned HTTP {}", resp.status());
        }
        // Reject an advertised oversize body BEFORE buffering anything.
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_JWKS_BYTES as u64)
        {
            anyhow::bail!("JWKS Content-Length exceeds {MAX_JWKS_BYTES} bytes");
        }
        // Stream with a hard cap so a missing/lying Content-Length can't exhaust memory: bail as soon
        // as the accumulated body would exceed the limit, without buffering the rest.
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if buf.len() + chunk.len() > MAX_JWKS_BYTES {
                anyhow::bail!("JWKS document exceeds {MAX_JWKS_BYTES} bytes");
            }
            buf.extend_from_slice(&chunk);
        }
        let set: JwkSet = serde_json::from_slice(&buf)
            .map_err(|e| anyhow::anyhow!("JWKS document is not a valid JWK Set: {e}"))?;
        Ok(set)
    }
}
