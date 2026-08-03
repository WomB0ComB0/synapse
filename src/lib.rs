//! # Synapse — the Organizational Brain
//!
//! Synapse is the **governed layer** that sits between AI agents / team
//! workflows and an organization's knowledge, tools, and policies. It gives
//! agents:
//!
//! 1. permission-aware retrieval (hybrid: pgvector + lexical + rerank),
//! 2. a versioned **skill registry**,
//! 3. bounded, PII-minimizing **context/memory**,
//! 4. a user/team/org **context service**,
//! 5. a policy-guarded **tool/connector (MCP) gateway**, and
//! 6. durable **workflow orchestration** — all behind a single
//!    **Policy & Access Gateway** with audit + observability.
//!
//! ## Reference architecture (7 MVP components)
//!
//! ```text
//! Users/Agents
//!     -> Policy & Access Gateway
//!         -> Skill Registry
//!         -> Context Service
//!         -> Retrieval Service  -> Document/Chunk store + Vector index (pgvector) [+ Graph]
//!         -> Tool/Connector Gateway (MCP)
//!         -> Workflow Orchestrator
//! Ingestion pipeline -> Vector + Graph + Context
//! Everything -> Audit / Observability (OpenTelemetry)
//! ```
//!
//! ## Design principles (threaded through the code)
//!
//! - Keep the brain **outside** any one agent (shared, attributable service).
//! - Separate the **canonical source-of-truth** from **derived** retrieval
//!   artifacts (embeddings/indexes are rebuildable).
//! - Make ACLs **queryable** (namespace/shard/row-level, not UI-only).
//! - Support **sync** request/response *and* **durable async** runs.
//! - Hybrid retrieval first, graph later.
//! - **Version** skills / prompts / chunking / embedding-model / eval-sets.
//! - Separate **personal memory** from org knowledge; minimize PII.
//! - Require **audit + approval** before autonomous writes.
//!
//! The binary (`src/main.rs`) wires config + telemetry + a DB pool + this
//! router and serves it; integration tests build [`app`] directly against a
//! lazily-connected pool (no live database required).

pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod context_service;
pub mod db;
pub mod domain;
pub mod error;
pub mod ingestion;
pub mod mcp;
pub mod orchestration;
pub mod ratelimit;
pub mod retrieval;
pub mod skills;
pub mod state;
pub mod telemetry;
pub mod tools;

/// Build the Synapse `axum` application router from shared [`state::AppState`].
///
/// This is the single composition root used by both the binary and the
/// integration tests, so the wiring is exercised identically everywhere.
pub fn app(state: state::AppState) -> axum::Router {
    api::router(state)
}
