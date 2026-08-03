//! Text embedding abstraction + providers.

use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Produces dense vector embeddings for text.
///
/// Embeddings are a **derived, rebuildable** artifact — never a source of
/// truth. The concrete model is versioned
/// ([`crate::config::Config::embedding_model`]) and recorded on each
/// [`crate::domain::Chunk`] so the index can be rebuilt deterministically.
///
/// Vectors are represented as `Vec<f32>`; they bind into Postgres via
/// `pgvector::Vector` at the storage/query boundary (see
/// [`crate::retrieval::hybrid`]).
#[allow(async_fn_in_trait)]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts into dense vectors (one per input, in input order).
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Dimensionality of the vectors this embedder produces.
    fn dimensions(&self) -> usize;
}

/// The process-wide embedder, chosen ONCE at startup
/// ([`crate::retrieval::default_embedder`]).
///
/// Static enum dispatch (not `Arc<dyn Embedder>`) because the trait uses native
/// `async fn` and so is not dyn-compatible — and it needs no `async-trait` dep.
/// The choice is fixed for the process so real and mock vectors never mix in the
/// same `vector(1536)` column (which would silently corrupt cosine ranking).
pub enum EmbedderImpl {
    /// Deterministic, dependency-free — used when no provider key is configured.
    Mock(MockEmbedder),
    /// Real OpenAI-compatible provider.
    OpenAi(OpenAiEmbedder),
    /// Native Gemini embeddings provider.
    Gemini(GeminiEmbedder),
}

impl Embedder for EmbedderImpl {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        match self {
            EmbedderImpl::Mock(m) => m.embed(texts).await,
            EmbedderImpl::OpenAi(o) => o.embed(texts).await,
            EmbedderImpl::Gemini(g) => g.embed(texts).await,
        }
    }

    fn dimensions(&self) -> usize {
        match self {
            EmbedderImpl::Mock(m) => m.dimensions(),
            EmbedderImpl::OpenAi(o) => o.dimensions(),
            EmbedderImpl::Gemini(g) => g.dimensions(),
        }
    }
}

/// Deterministic, dependency-free embedder for tests and local development.
///
/// Hashes token bytes into fixed-size buckets and L2-normalizes. Not
/// semantically meaningful — just stable and cheap.
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    dims: usize,
}

impl MockEmbedder {
    /// Create a mock embedder producing `dims`-dimensional vectors.
    pub fn new(dims: usize) -> Self {
        Self { dims: dims.max(1) }
    }
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self { dims: 8 }
    }
}

impl Embedder for MockEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let out = texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dims];
                for (i, b) in t.bytes().enumerate() {
                    v[i % self.dims] += b as f32;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in &mut v {
                        *x /= norm;
                    }
                }
                v
            })
            .collect();
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

/// Real embedder backed by an OpenAI-compatible `/embeddings` endpoint.
///
/// Batches inputs, pins `dimensions` so the returned vectors always match the
/// `vector(1536)` column, validates the response (count + per-vector length,
/// reordered to input order), and retries transient failures (429/5xx/transport)
/// with exponential backoff. Never logs the API key or raw provider bodies.
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dims: usize,
    max_batch: usize,
    max_retries: u32,
}

impl OpenAiEmbedder {
    /// Construct the embedder. Building the HTTP client can only fail on a
    /// misconfigured TLS backend, which is a legitimate startup fault — hence
    /// `expect` (this runs once at boot).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        dims: usize,
        max_batch: usize,
        max_retries: u32,
        timeout_secs: u64,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(10))
            // Do NOT follow redirects: a well-behaved embeddings API never 3xx's,
            // and following one could downgrade https->http (cleartext egress of
            // document/query text) or forward the Bearer key on a same-host
            // redirect. Fail closed — a 3xx becomes an Upstream error instead.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build reqwest client (TLS backend?)");
        Self {
            client,
            // trim() first so a copy-pasted trailing space doesn't defeat the
            // slash trim; then drop trailing '/' so "{base}/embeddings" is clean.
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            api_key,
            model,
            dims: dims.max(1),
            max_batch: max_batch.max(1),
            max_retries,
        }
    }

    /// Embed one batch (<= max_batch inputs) with bounded retry/backoff.
    async fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", self.base_url);
        let body = request_body(&self.model, self.dims, batch);
        let mut attempt: u32 = 0;
        loop {
            match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        // Read the body separately from parsing: a transport error
                        // mid-stream (reset/truncated after the 200) is transient and
                        // retried like a pre-response transport error; only genuinely
                        // malformed JSON is terminal.
                        match resp.bytes().await {
                            Ok(bytes) => {
                                let parsed: EmbeddingsResponse = serde_json::from_slice(&bytes)
                                    .map_err(|e| {
                                        Error::Upstream(format!(
                                            "failed to decode embeddings response: {e}"
                                        ))
                                    })?;
                                return parse_embeddings_response(parsed, batch.len(), self.dims);
                            }
                            Err(_) if attempt < self.max_retries => {
                                let wait = backoff(attempt);
                                attempt += 1;
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            Err(e) => {
                                return Err(Error::Upstream(format!(
                                    "embeddings body read failed: {e}"
                                )));
                            }
                        }
                    }
                    // Retry transient failures; 4xx (auth/bad request) are terminal.
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < self.max_retries {
                        let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    // Never echo the body — it may carry provider-side detail.
                    return Err(Error::Upstream(format!(
                        "embeddings provider returned HTTP {}",
                        status.as_u16()
                    )));
                }
                Err(e) => {
                    if attempt < self.max_retries {
                        let wait = backoff(attempt);
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(Error::Upstream(format!("embeddings request failed: {e}")));
                }
            }
        }
    }
}

impl Embedder for OpenAiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.max_batch) {
            out.extend(self.embed_batch(batch).await?);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

/// Real embedder backed by Gemini's native `models.batchEmbedContents` endpoint.
///
/// Uses `x-goog-api-key`, sends `embedContentConfig.outputDimensionality`, and validates that Gemini
/// returns exactly the `vector(1536)` shape Synapse stores. The response order is documented to match
/// the request order, so no provider index is needed.
pub struct GeminiEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dims: usize,
    max_batch: usize,
    max_retries: u32,
}

impl GeminiEmbedder {
    /// Construct the embedder. Building the HTTP client can only fail on a
    /// misconfigured TLS backend, which is a legitimate startup fault.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        dims: usize,
        max_batch: usize,
        max_retries: u32,
        timeout_secs: u64,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build reqwest client (TLS backend?)");
        Self {
            client,
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            api_key,
            model,
            dims: dims.max(1),
            max_batch: max_batch.max(1),
            max_retries,
        }
    }

    /// Embed one Gemini batch (<= max_batch inputs) with bounded retry/backoff.
    async fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>> {
        let model = gemini_model_resource(&self.model);
        let url = format!("{}/{}:batchEmbedContents", self.base_url, model);
        let body = gemini_batch_request_body(&model, self.dims, batch);
        let mut attempt: u32 = 0;
        loop {
            match self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.bytes().await {
                            Ok(bytes) => {
                                let parsed: GeminiBatchEmbeddingsResponse =
                                    serde_json::from_slice(&bytes).map_err(|e| {
                                        Error::Upstream(format!(
                                            "failed to decode Gemini embeddings response: {e}"
                                        ))
                                    })?;
                                return parse_gemini_embeddings_response(
                                    parsed,
                                    batch.len(),
                                    self.dims,
                                );
                            }
                            Err(_) if attempt < self.max_retries => {
                                let wait = backoff(attempt);
                                attempt += 1;
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            Err(e) => {
                                return Err(Error::Upstream(format!(
                                    "Gemini embeddings body read failed: {e}"
                                )));
                            }
                        }
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < self.max_retries {
                        let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(Error::Upstream(format!(
                        "Gemini embeddings provider returned HTTP {}",
                        status.as_u16()
                    )));
                }
                Err(e) => {
                    if attempt < self.max_retries {
                        let wait = backoff(attempt);
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(Error::Upstream(format!(
                        "Gemini embeddings request failed: {e}"
                    )));
                }
            }
        }
    }
}

impl Embedder for GeminiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.max_batch) {
            out.extend(self.embed_batch(batch).await?);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

/// Normalize a Gemini model id to the REST resource form used in paths and request bodies.
fn gemini_model_resource(model: &str) -> String {
    let trimmed = model.trim().trim_start_matches('/');
    if trimmed.starts_with("models/") {
        trimmed.to_string()
    } else {
        format!("models/{trimmed}")
    }
}

/// Request body for Gemini `models.batchEmbedContents`.
fn gemini_batch_request_body(model: &str, dims: usize, inputs: &[String]) -> serde_json::Value {
    let requests: Vec<_> = inputs
        .iter()
        .map(|text| {
            serde_json::json!({
                "model": model,
                "content": { "parts": [{ "text": text }] },
                "embedContentConfig": { "outputDimensionality": dims },
            })
        })
        .collect();
    serde_json::json!({ "requests": requests })
}

#[derive(Debug, Deserialize)]
struct GeminiBatchEmbeddingsResponse {
    embeddings: Vec<GeminiEmbedding>,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

/// Validate Gemini's response into request-order vectors.
fn parse_gemini_embeddings_response(
    resp: GeminiBatchEmbeddingsResponse,
    expected: usize,
    dims: usize,
) -> Result<Vec<Vec<f32>>> {
    if resp.embeddings.len() != expected {
        return Err(Error::Upstream(format!(
            "Gemini embeddings response had {} items, expected {expected}",
            resp.embeddings.len()
        )));
    }
    let mut out = Vec::with_capacity(expected);
    for embedding in resp.embeddings {
        if embedding.values.len() != dims {
            return Err(Error::Upstream(format!(
                "Gemini embedding has {} dims, expected {dims}",
                embedding.values.len()
            )));
        }
        if !embedding.values.iter().all(|x| x.is_finite()) {
            return Err(Error::Upstream(
                "Gemini embedding contains non-finite values (NaN/Inf)".into(),
            ));
        }
        out.push(embedding.values);
    }
    Ok(out)
}

/// Request body for `POST /embeddings`. Pinning `dimensions` forces the provider
/// to return `dims`-length vectors even for models that default to a larger size.
fn request_body(model: &str, dims: usize, inputs: &[String]) -> serde_json::Value {
    serde_json::json!({ "model": model, "input": inputs, "dimensions": dims })
}

/// Exponential backoff: 200ms, 400ms, 800ms, … (capped at 5s).
fn backoff(attempt: u32) -> Duration {
    let ms = 200u64.saturating_mul(1u64 << attempt.min(5));
    Duration::from_millis(ms.min(5_000))
}

/// Honor a `Retry-After: <seconds>` header when present (429 responses).
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s.min(30)))
}

/// One `{ index, embedding }` datum from the provider.
#[derive(Debug, Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

/// The `/embeddings` response envelope (only the fields we consume).
#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

/// Validate + normalize a provider response into input-order vectors.
///
/// Guards against silent corruption: the count must match the batch, every
/// vector must be exactly `dims` long and **finite** (no NaN/±Inf — an
/// out-of-range JSON number casts to `f32::INFINITY` without erroring, and
/// pgvector would later reject it as a 500 rather than the correct 502), and
/// `index` must densely cover `0..expected` after sorting (so vectors end up in
/// INPUT order, not the provider's return order).
fn parse_embeddings_response(
    mut resp: EmbeddingsResponse,
    expected: usize,
    dims: usize,
) -> Result<Vec<Vec<f32>>> {
    if resp.data.len() != expected {
        return Err(Error::Upstream(format!(
            "embeddings response had {} items, expected {expected}",
            resp.data.len()
        )));
    }
    resp.data.sort_by_key(|d| d.index);
    let mut out = Vec::with_capacity(expected);
    for (i, datum) in resp.data.into_iter().enumerate() {
        if datum.index != i {
            return Err(Error::Upstream(
                "embeddings response indices are not a dense 0..n sequence".into(),
            ));
        }
        if datum.embedding.len() != dims {
            return Err(Error::Upstream(format!(
                "embedding has {} dims, expected {dims}",
                datum.embedding.len()
            )));
        }
        if !datum.embedding.iter().all(|x| x.is_finite()) {
            return Err(Error::Upstream(
                "embedding contains non-finite values (NaN/Inf)".into(),
            ));
        }
        out.push(datum.embedding);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_model_resource_accepts_short_or_resource_name() {
        assert_eq!(
            gemini_model_resource("gemini-embedding-2"),
            "models/gemini-embedding-2"
        );
        assert_eq!(
            gemini_model_resource("models/gemini-embedding-2"),
            "models/gemini-embedding-2"
        );
    }

    #[test]
    fn gemini_request_body_pins_model_text_and_output_dimensions() {
        let inputs = vec!["a".to_string(), "b".to_string()];
        let body = gemini_batch_request_body("models/gemini-embedding-2", 1536, &inputs);
        assert_eq!(body["requests"][0]["model"], "models/gemini-embedding-2");
        assert_eq!(body["requests"][0]["content"]["parts"][0]["text"], "a");
        assert_eq!(
            body["requests"][0]["embedContentConfig"]["outputDimensionality"],
            1536
        );
        assert_eq!(body["requests"][1]["content"]["parts"][0]["text"], "b");
    }

    #[test]
    fn parse_gemini_response_validates_count_dimension_and_finiteness() {
        let ok = GeminiBatchEmbeddingsResponse {
            embeddings: vec![GeminiEmbedding {
                values: vec![0.0, 1.0],
            }],
        };
        assert_eq!(
            parse_gemini_embeddings_response(ok, 1, 2).unwrap(),
            vec![vec![0.0, 1.0]]
        );

        let wrong_dim = GeminiBatchEmbeddingsResponse {
            embeddings: vec![GeminiEmbedding { values: vec![0.0] }],
        };
        assert!(parse_gemini_embeddings_response(wrong_dim, 1, 2).is_err());

        let bad = GeminiBatchEmbeddingsResponse {
            embeddings: vec![GeminiEmbedding {
                values: vec![f32::INFINITY, 0.0],
            }],
        };
        assert!(parse_gemini_embeddings_response(bad, 1, 2).is_err());
    }

    #[test]
    fn request_body_pins_model_input_and_dimensions() {
        let inputs = vec!["a".to_string(), "b".to_string()];
        let body = request_body("text-embedding-3-small", 1536, &inputs);
        assert_eq!(body["model"], "text-embedding-3-small");
        assert_eq!(body["dimensions"], 1536);
        assert_eq!(body["input"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn parse_reorders_by_index_into_input_order() {
        // Provider returned index 1 before index 0.
        let resp = EmbeddingsResponse {
            data: vec![
                EmbeddingDatum {
                    index: 1,
                    embedding: vec![1.0, 1.0],
                },
                EmbeddingDatum {
                    index: 0,
                    embedding: vec![0.0, 0.0],
                },
            ],
        };
        let out = parse_embeddings_response(resp, 2, 2).unwrap();
        assert_eq!(out, vec![vec![0.0, 0.0], vec![1.0, 1.0]]);
    }

    #[test]
    fn parse_rejects_wrong_count() {
        let resp = EmbeddingsResponse {
            data: vec![EmbeddingDatum {
                index: 0,
                embedding: vec![0.0; 3],
            }],
        };
        assert!(parse_embeddings_response(resp, 2, 3).is_err());
    }

    #[test]
    fn parse_rejects_wrong_dimension() {
        // Asking for dims=1536 but the provider returned a 3-dim vector.
        let resp = EmbeddingsResponse {
            data: vec![EmbeddingDatum {
                index: 0,
                embedding: vec![0.0, 0.0, 0.0],
            }],
        };
        assert!(parse_embeddings_response(resp, 1, 1536).is_err());
    }

    #[test]
    fn parse_rejects_index_gap() {
        let resp = EmbeddingsResponse {
            data: vec![
                EmbeddingDatum {
                    index: 0,
                    embedding: vec![0.0],
                },
                EmbeddingDatum {
                    index: 2,
                    embedding: vec![0.0],
                },
            ],
        };
        assert!(parse_embeddings_response(resp, 2, 1).is_err());
    }

    #[test]
    fn parse_rejects_non_finite() {
        // A finite-but-out-of-range JSON number deserializes to f32::INFINITY with
        // no error; the finiteness guard must reject NaN/Inf (else pgvector 500s
        // instead of the correct 502).
        for bad in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            let resp = EmbeddingsResponse {
                data: vec![EmbeddingDatum {
                    index: 0,
                    embedding: vec![bad, 0.0],
                }],
            };
            assert!(
                parse_embeddings_response(resp, 1, 2).is_err(),
                "must reject non-finite {bad}"
            );
        }
    }

    #[tokio::test]
    async fn mock_embedder_is_stable_and_dimensioned() {
        let m = MockEmbedder::new(1536);
        assert_eq!(m.dimensions(), 1536);
        let a = m.embed(&["hello".to_string()]).await.unwrap();
        let b = m.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].len(), 1536);
    }
}
