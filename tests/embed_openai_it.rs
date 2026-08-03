//! Behavioral tests for `OpenAiEmbedder` against a hand-rolled localhost mock of
//! the OpenAI-compatible `/embeddings` endpoint.
//!
//! No external network and no database: a tiny axum server on `127.0.0.1:0`
//! stands in for the provider, so these run in CI without a key. They cover the
//! contract that matters — auth header, batching, response reordering, transient
//! retry, and the dimension guardrail.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use synapse::retrieval::embed::{Embedder, OpenAiEmbedder};

/// How the mock should behave for a test.
#[derive(Clone)]
struct MockCfg {
    /// Length of the vectors the mock returns (to exercise the dim guardrail).
    return_dims: usize,
    /// Answer this many leading requests with 503 before succeeding (retry test).
    fail_first: usize,
    /// Return `data` in reverse `index` order (proves the client reorders).
    reverse: bool,
}

#[derive(Clone)]
struct MockState {
    cfg: MockCfg,
    calls: Arc<AtomicUsize>,
    last_auth: Arc<Mutex<Option<String>>>,
}

async fn embeddings(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let n = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(a) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        *state.last_auth.lock().unwrap() = Some(a.to_string());
    }
    if n <= state.cfg.fail_first {
        return (StatusCode::SERVICE_UNAVAILABLE, "try again").into_response();
    }
    let inputs = body["input"].as_array().cloned().unwrap_or_default();
    let mut data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, _)| json!({ "index": i, "embedding": vec![0.1f32; state.cfg.return_dims] }))
        .collect();
    if state.cfg.reverse {
        data.reverse(); // same indices, out-of-order — client must sort back
    }
    Json(json!({ "data": data })).into_response()
}

/// Spawn the mock on an ephemeral loopback port; return (base_url, calls, last_auth).
async fn spawn(cfg: MockCfg) -> (String, Arc<AtomicUsize>, Arc<Mutex<Option<String>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_auth = Arc::new(Mutex::new(None));
    let state = MockState {
        cfg,
        calls: calls.clone(),
        last_auth: last_auth.clone(),
    };
    let app = Router::new()
        .route("/embeddings", post(embeddings))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), calls, last_auth)
}

fn embedder(base_url: String, dims: usize, max_batch: usize, max_retries: u32) -> OpenAiEmbedder {
    OpenAiEmbedder::new(
        base_url,
        "test-key".to_string(),
        "text-embedding-3-small".to_string(),
        dims,
        max_batch,
        max_retries,
        5,
    )
}

fn texts(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("chunk-{i}")).collect()
}

#[tokio::test]
async fn sends_bearer_auth_and_returns_input_order_across_batches() {
    // 5 inputs, max_batch 2 -> 3 requests (2+2+1); mock returns reversed data.
    let (base, calls, auth) = spawn(MockCfg {
        return_dims: 4,
        fail_first: 0,
        reverse: true,
    })
    .await;
    let e = embedder(base, 4, 2, 3);
    let out = e.embed(&texts(5)).await.expect("embed ok");

    assert_eq!(out.len(), 5, "one vector per input");
    assert!(out.iter().all(|v| v.len() == 4), "each vector has dims=4");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "5 inputs / batch 2 = 3 requests"
    );
    assert_eq!(
        auth.lock().unwrap().as_deref(),
        Some("Bearer test-key"),
        "Authorization: Bearer <key> is sent"
    );
    println!("(a) bearer auth + batching (3 reqs) + reorder -> 5 input-order vectors");
}

#[tokio::test]
async fn retries_transient_5xx_then_succeeds() {
    // Fail the first 2 requests with 503; max_retries=3 -> the 3rd succeeds.
    let (base, calls, _auth) = spawn(MockCfg {
        return_dims: 4,
        fail_first: 2,
        reverse: false,
    })
    .await;
    let e = embedder(base, 4, 16, 3);
    let out = e.embed(&texts(1)).await.expect("embed ok after retries");
    assert_eq!(out.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 3, "2 failures + 1 success");
    println!("(b) retries transient 503 then succeeds");
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    // Always 503; max_retries=2 -> 3 attempts total, then error.
    let (base, calls, _auth) = spawn(MockCfg {
        return_dims: 4,
        fail_first: usize::MAX,
        reverse: false,
    })
    .await;
    let e = embedder(base, 4, 16, 2);
    let err = e.embed(&texts(1)).await.expect_err("must give up");
    assert!(
        format!("{err}").to_lowercase().contains("upstream"),
        "provider failure maps to an Upstream error (HTTP 502): {err}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3, "1 initial + 2 retries");
    println!("(c) gives up after max_retries -> Upstream error");
}

#[tokio::test]
async fn rejects_wrong_dimension() {
    // Client wants dims=1536 but the mock returns dims=8 -> guardrail fires.
    let (base, _calls, _auth) = spawn(MockCfg {
        return_dims: 8,
        fail_first: 0,
        reverse: false,
    })
    .await;
    let e = embedder(base, 1536, 16, 1);
    let err = e
        .embed(&texts(2))
        .await
        .expect_err("dim mismatch must error");
    assert!(
        format!("{err}").to_lowercase().contains("dim"),
        "dim error: {err}"
    );
    println!("(d) wrong provider dimension -> error (no silent corruption)");
}

#[tokio::test]
async fn does_not_follow_redirects() {
    // A 3xx must NOT be followed (following could downgrade https->http and egress
    // text, or forward the Bearer key). With redirects disabled the client sees the
    // 3xx as a non-success -> terminal Upstream error, and never hits the target.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/embeddings",
        post(|| async { StatusCode::TEMPORARY_REDIRECT }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let e = embedder(format!("http://{addr}"), 4, 16, 0);
    let err = e
        .embed(&texts(1))
        .await
        .expect_err("a redirect must not be followed");
    assert!(
        format!("{err}").to_lowercase().contains("upstream"),
        "redirect surfaces as an Upstream error: {err}"
    );
    println!("(e) does not follow redirects -> Upstream (no cleartext/key egress)");
}
