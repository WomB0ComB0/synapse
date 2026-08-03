//! Live integration test: opt-in heuristic rerank (recency + authority).
//!
//! When `retrieval.rerank` is set, the RRF-fused candidates are multiplied by a recency
//! (exp-decay by age) + authority (owned/curated document) boost before top-K. Because the boost
//! is on the RRF's natural scale, it reorders NEAR-ties (two identical-content docs sit at
//! adjacent RRF ranks) without overriding a large relevance gap. Off = pure RRF.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5471:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5471/postgres
//! cargo test --test rerank_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use axum::Router;
use common::{post_json, setup_with_db};
use serde_json::json;

async fn ingest(router: &Router, tenant: &str, doc_id: &str, content: &str) {
    let body = json!({
        "doc_id": doc_id,
        "tenant_id": tenant,
        "principal_id": "user_1",
        "team_scope": ["eng"],
        "source_uri": format!("uri://{doc_id}"),
        "content": content,
    });
    let (status, val) = post_json(router, "/documents.ingest", tenant, "user_1", body).await;
    assert_eq!(status, StatusCode::OK, "ingest {doc_id} failed: {val}");
}

async fn retrieve_doc_order(
    router: &Router,
    tenant: &str,
    query: &str,
    rerank: bool,
) -> Vec<String> {
    let (status, resp) = post_json(
        router,
        "/retrieve",
        tenant,
        "user_1",
        json!({
            "tenant_id": tenant, "principal_id": "user_1", "query": query,
            "retrieval": { "mode": "hybrid", "top_k": 10, "rerank": rerank }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "retrieve: {resp}");
    resp["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["doc_id"].as_str().unwrap().to_string())
        .collect()
}

fn pos(order: &[String], doc_id: &str) -> Option<usize> {
    order.iter().position(|d| d == doc_id)
}

#[tokio::test]
async fn rerank_boosts_recent_and_authoritative_docs() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";
    let (router, admin) = setup_with_db(&base_url, "rerank").await;

    // --- RECENCY: two docs with IDENTICAL content; make one 90 days old ---
    ingest(&router, t, "r-old", "alpha beta gamma delta recency probe").await;
    ingest(&router, t, "r-new", "alpha beta gamma delta recency probe").await;
    sqlx::query(
        "UPDATE chunks SET created_at = now() - interval '90 days' \
         WHERE tenant_id = $1 AND doc_id = 'r-old'",
    )
    .bind(t)
    .execute(&admin)
    .await
    .unwrap();

    let reranked = retrieve_doc_order(&router, t, "alpha beta gamma", true).await;
    let (p_new, p_old) = (pos(&reranked, "r-new"), pos(&reranked, "r-old"));
    assert!(
        p_new.is_some() && p_old.is_some(),
        "both docs retrieved: {reranked:?}"
    );
    assert!(
        p_new < p_old,
        "rerank ranks the NEWER doc above the older one: {reranked:?}"
    );
    println!("(recency) the newer of two identical-content docs is boosted above the older");

    // --- AUTHORITY: two docs with IDENTICAL content; make one OWNED (by the caller) ---
    ingest(
        &router,
        t,
        "a-owned",
        "epsilon zeta eta theta authority probe",
    )
    .await;
    ingest(
        &router,
        t,
        "a-public",
        "epsilon zeta eta theta authority probe",
    )
    .await;
    // Owned by the CALLER, so it stays readable (owner) AND is authoritative (has owners).
    sqlx::query(
        "UPDATE documents SET owners = ARRAY['user_1'] \
         WHERE tenant_id = $1 AND doc_id = 'a-owned'",
    )
    .bind(t)
    .execute(&admin)
    .await
    .unwrap();

    let reranked = retrieve_doc_order(&router, t, "epsilon zeta eta", true).await;
    let (p_owned, p_public) = (pos(&reranked, "a-owned"), pos(&reranked, "a-public"));
    assert!(
        p_owned.is_some() && p_public.is_some(),
        "both docs retrieved: {reranked:?}"
    );
    assert!(
        p_owned < p_public,
        "rerank ranks the OWNED doc above the public one: {reranked:?}"
    );
    println!("(authority) the owned/curated of two identical-content docs is boosted above the public one");

    // --- OFF: without rerank, both docs still retrieve (order is pure RRF, not asserted) ---
    let plain = retrieve_doc_order(&router, t, "alpha beta gamma", false).await;
    assert!(
        pos(&plain, "r-new").is_some() && pos(&plain, "r-old").is_some(),
        "rerank off still returns both docs: {plain:?}"
    );
    println!("(off) rerank disabled returns the same candidate set on pure RRF");

    admin.close().await;
    println!("rerank: recency + authority multiplicatively boost near-ties so a newer / owned doc outranks its identical-content peer; off = pure RRF.");
}
