//! Retrieval service — hybrid, permission-aware search.
//!
//! Retrieval artifacts (embeddings, lexical index, graph) are **derived** from
//! the canonical document store and are always rebuildable. The public entry
//! point is [`hybrid::search`]; embeddings are produced via the
//! [`embed::Embedder`] trait so the model can be swapped/versioned.

pub mod chunk;
pub mod embed;
pub mod hybrid;

use crate::config::{Config, EmbeddingProvider};
use embed::{EmbedderImpl, GeminiEmbedder, MockEmbedder, OpenAiEmbedder};

/// Embedding dimensionality used across ingest + retrieve.
///
/// Must match the `vector(1536)` column in migration `0005` and the default
/// `text-embedding-3-small` model recorded on each chunk. Ingest and query
/// vectors MUST share this dimension so cosine distance is comparable.
pub const EMBEDDING_DIM: usize = 1536;

/// Build the process-wide embedder from configuration.
///
/// Uses the configured real provider when an embedding API key is set, else the deterministic
/// [`MockEmbedder`] (dev/CI or explicit degraded mode). The choice is made ONCE at startup and
/// stored in [`crate::state::AppState`] so ingest and retrieve always share one embedder — real and
/// mock vectors must never mix in the same `vector(1536)` column. Always dimensioned to
/// [`EMBEDDING_DIM`].
pub fn default_embedder(config: &Config) -> EmbedderImpl {
    match (config.embedding_provider, config.openai_api_key.as_ref()) {
        (EmbeddingProvider::Gemini, Some(key)) => EmbedderImpl::Gemini(GeminiEmbedder::new(
            config.embedding_base_url.clone(),
            key.clone(),
            config.embedding_model.clone(),
            EMBEDDING_DIM,
            config.embedding_max_batch,
            config.embedding_max_retries,
            config.embedding_timeout_secs,
        )),
        (EmbeddingProvider::OpenAi, Some(key)) => EmbedderImpl::OpenAi(OpenAiEmbedder::new(
            config.embedding_base_url.clone(),
            key.clone(),
            config.embedding_model.clone(),
            EMBEDDING_DIM,
            config.embedding_max_batch,
            config.embedding_max_retries,
            config.embedding_timeout_secs,
        )),
        _ => EmbedderImpl::Mock(MockEmbedder::new(EMBEDDING_DIM)),
    }
}
