//! DB-gated golden-set retrieval quality and ACL leakage evaluation.

mod common;

use axum::http::StatusCode;
use common::{app_pool, apply_schema, post_json, router_for, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;

async fn ingest(router: &axum::Router, doc_id: &str, content: &str, owners: &[&str]) {
    let (status, response) = post_json(
        router,
        "/documents.ingest",
        "tenant_a",
        "eval-author",
        json!({
            "doc_id": doc_id,
            "tenant_id": "tenant_a",
            "principal_id": "eval-author",
            "source_uri": format!("eval://{doc_id}"),
            "title": doc_id,
            "owners": owners,
            "content": content
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ingest {doc_id}: {response}");
}

fn ranked_doc_ids(response: &Value) -> Vec<&str> {
    response["results"]
        .as_array()
        .expect("retrieval results")
        .iter()
        .filter_map(|result| result["doc_id"].as_str())
        .collect()
}

#[tokio::test]
async fn golden_set_meets_recall_mrr_and_acl_leakage_gates() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset; skipping DB-gated retrieval evaluation");
        return;
    };

    let test_db = TestDb::create(&base_url, "retrieval_eval").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let router = router_for(app_pool(&test_db.url, &test_db.role).await, &test_db.url);

    let corpus = [
        (
            "database-failover",
            "PostgreSQL database failover runbook: promote the replica, verify WAL replay, and restore service.",
        ),
        (
            "jwt-rotation",
            "JWT signing key rotation procedure: publish JWKS, overlap old and new keys, then retire the old kid.",
        ),
        (
            "gemini-budget",
            "Gemini embedding budget operations: monitor batchEmbedContents tokens, quotas, and retry pressure.",
        ),
        (
            "payroll-close",
            "Payroll month-end close checklist: reconcile benefits, taxes, deductions, and direct deposits.",
        ),
    ];
    for (doc_id, content) in corpus {
        ingest(&router, doc_id, content, &[]).await;
    }
    ingest(
        &router,
        "restricted-acquisition",
        "Project Nightjar confidential acquisition valuation and board approval package.",
        &["alice"],
    )
    .await;

    let golden = [
        ("replica WAL failover", "database-failover"),
        ("JWKS signing key kid rotation", "jwt-rotation"),
        ("Gemini embedding quota tokens", "gemini-budget"),
        ("payroll taxes deductions", "payroll-close"),
    ];
    let mut hits = 0usize;
    let mut reciprocal_rank = 0.0_f64;
    for (query, expected_doc) in golden {
        let (status, response) = post_json(
            &router,
            "/retrieve",
            "tenant_a",
            "eval-reader",
            json!({
                "tenant_id": "tenant_a",
                "principal_id": "eval-reader",
                "query": query,
                "retrieval": {"mode": "hybrid", "top_k": 3}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "query {query}: {response}");
        let ranked = ranked_doc_ids(&response);
        if let Some(index) = ranked.iter().position(|doc_id| *doc_id == expected_doc) {
            hits += 1;
            reciprocal_rank += 1.0 / (index + 1) as f64;
        }
    }

    let recall_at_3 = hits as f64 / golden.len() as f64;
    let mrr = reciprocal_rank / golden.len() as f64;
    assert!(recall_at_3 >= 0.75, "Recall@3 regression: {recall_at_3:.3}");
    assert!(mrr >= 0.75, "MRR regression: {mrr:.3}");

    let (status, response) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "eval-reader",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "eval-reader",
            "query": "Nightjar acquisition valuation board",
            "retrieval": {"mode": "hybrid", "top_k": 10}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(
        !ranked_doc_ids(&response).contains(&"restricted-acquisition"),
        "retrieval leaked an owner-restricted document"
    );
}
