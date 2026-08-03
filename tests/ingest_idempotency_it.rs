//! Live integration test: idempotent document ingest (opt-in request-hash dedupe).
//!
//! With `INGEST_IDEMPOTENCY_ENABLED=true`, re-ingesting a `doc_id` with a BYTE-IDENTICAL request (same
//! content AND metadata/owners/ACL) is a no-op REPLAY — it skips re-chunking + the expensive
//! re-embedding + all chunk writes, and returns `status: "replayed"`. Any changed field — content OR
//! metadata OR ACL — re-ingests as usual (replacing chunks + applying the metadata/ACL) and refreshes
//! the fingerprint. With the toggle OFF, every ingest re-writes (the pre-existing behavior).
//!
//! The fingerprint keys on the WHOLE request (not content alone) so enabling idempotency can never
//! silently drop a metadata update or an ACL grant/revoke — `reingest_with_changed_metadata_or_acl_*`
//! guards that. The no-op is proved WITHOUT timing assumptions: a replay never touches `chunks`, so
//! `max(created_at)` for the doc is byte-identical afterward; a real re-ingest replaces the rows and
//! advances it.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5483:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5483/postgres
//! cargo test --test ingest_idempotency_it -- --nocapture
//! ```

mod common;

use axum::Router;
use common::{app_pool, apply_schema, post_json, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use synapse::config::Config;
use synapse::state::AppState;

fn router_for(pool: PgPool, database_url: &str, ingest_idempotency: bool) -> Router {
    let config = Config {
        production_mode: false,
        database_url: database_url.to_string(),
        bind_addr: "0.0.0.0:8080".to_string(),
        db_max_connections: 20,
        db_acquire_timeout_secs: 10,
        max_request_body_bytes: 16 * 1024 * 1024,
        request_timeout_secs: 180,
        max_in_flight_requests: 256,
        embedding_model: "text-embedding-3-small".to_string(),
        embedding_provider: synapse::config::EmbeddingProvider::Mock,
        openai_api_key: None,
        embedding_base_url: "https://api.openai.com/v1".to_string(),
        embedding_max_batch: 96,
        embedding_timeout_secs: 30,
        embedding_max_retries: 3,
        otel_endpoint: None,
        auth_jwt_secret: None,
        auth_jwt_public_key: None,
        auth_jwt_audience: None,
        auth_jwt_issuer: None,
        auth_jwks_url: None,
        auth_jwks_timeout_secs: 10,
        auth_jwks_min_refetch_secs: 60,
        auth_revocation_enabled: false,
        abac_context_ownership: false,
        embedding_model_consistency: false,
        retrieval_mmr_lambda: 0.5,
        rate_limit_enabled: false,
        rate_limit_tenant_rps: 10.0,
        rate_limit_burst: 20.0,
        ingest_idempotency_enabled: ingest_idempotency,
        mcp_endpoint: None,
        mcp_auth_token: None,
        mcp_auth_token_file: None,
        mcp_scopes: Vec::new(),
        mcp_allowed_hosts: Vec::new(),
        mcp_timeout_secs: 30,
        mcp_max_retries: 2,
        worker_enabled: false,
        worker_poll_secs: 30,
        worker_stale_secs: 300,
    };
    synapse::app(AppState::new(pool, config))
}

async fn ingest(router: &Router, tenant: &str, doc_id: &str, content: &str) -> Value {
    let body = json!({
        "doc_id": doc_id,
        "tenant_id": tenant,
        "team_scope": ["eng"],
        "source_uri": format!("uri://{doc_id}"),
        "content": content,
    });
    let (status, r) = post_json(router, "/documents.ingest", tenant, "ingester", body).await;
    assert_eq!(status, axum::http::StatusCode::OK, "ingest {doc_id}: {r}");
    r
}

/// `max(created_at)` over a doc's chunks — changes iff the chunks were (re)written.
async fn max_chunk_ts(admin: &PgPool, tenant: &str, doc_id: &str) -> chrono::DateTime<chrono::Utc> {
    let (ts,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT max(created_at) FROM chunks WHERE tenant_id = $1 AND doc_id = $2")
            .bind(tenant)
            .bind(doc_id)
            .fetch_one(admin)
            .await
            .unwrap();
    ts.expect("doc has at least one chunk")
}

/// POST an arbitrary ingest `body`; assert 200 and return the response JSON.
async fn ingest_raw(router: &Router, tenant: &str, body: Value) -> Value {
    let (status, r) = post_json(router, "/documents.ingest", tenant, "ingester", body).await;
    assert_eq!(status, axum::http::StatusCode::OK, "ingest: {r}");
    r
}

/// The persisted `documents.team_scope` for a doc (proves a metadata update actually landed).
async fn team_scope_of(admin: &PgPool, tenant: &str, doc_id: &str) -> Vec<String> {
    let (scope,): (Vec<String>,) =
        sqlx::query_as("SELECT team_scope FROM documents WHERE tenant_id = $1 AND doc_id = $2")
            .bind(tenant)
            .bind(doc_id)
            .fetch_one(admin)
            .await
            .unwrap();
    scope
}

/// Whether a normalized `document_acl` READ grant exists (proves an ACL sync actually landed).
async fn acl_grant_exists(admin: &PgPool, tenant: &str, doc_id: &str, grantee: &str) -> bool {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM document_acl \
         WHERE tenant_id = $1 AND doc_id = $2 AND grantee_type = 'group' AND grantee_id = $3",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(grantee)
    .fetch_one(admin)
    .await
    .unwrap();
    n > 0
}

#[tokio::test]
async fn ingest_replays_identical_content_and_reingests_changed_content() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "ingestidem").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let enabled = router_for(
        app_pool(&test_db.url, &test_db.role).await,
        &test_db.url,
        true,
    );
    let disabled = router_for(
        app_pool(&test_db.url, &test_db.role).await,
        &test_db.url,
        false,
    );

    let c1 = "Database failover runbook. When the primary is unavailable, promote the replica and restore service through failover.";

    // (1) First ingest -> "ingested" + N chunks.
    let r = ingest(&enabled, t, "doc1", c1).await;
    assert_eq!(r["status"], "ingested", "first ingest");
    let n = r["chunks_ingested"].as_u64().unwrap();
    assert!(n >= 1, "expected >=1 chunk, got {n}");
    let ts1 = max_chunk_ts(&admin, t, "doc1").await;

    // (2) Re-ingest IDENTICAL content -> "replayed", same count, chunks UNTOUCHED (no re-embed).
    let r = ingest(&enabled, t, "doc1", c1).await;
    assert_eq!(r["status"], "replayed", "identical re-ingest is a replay");
    assert_eq!(
        r["chunks_ingested"].as_u64().unwrap(),
        n,
        "replay reports the existing chunk count"
    );
    assert_eq!(
        max_chunk_ts(&admin, t, "doc1").await,
        ts1,
        "replay did NOT rewrite chunks (skipped re-embed)"
    );

    // (3) Re-ingest CHANGED content -> "ingested", chunks replaced (created_at advances).
    let c2 = "Kubernetes autoscaling and billing dashboards. Cost metrics, invoices, and quota alerts for the platform team.";
    let r = ingest(&enabled, t, "doc1", c2).await;
    assert_eq!(r["status"], "ingested", "changed content re-ingests");
    let ts3 = max_chunk_ts(&admin, t, "doc1").await;
    assert!(ts3 > ts1, "changed content rewrote the chunks");

    // (4) Re-ingest the NEW content again -> "replayed" (the fingerprint was refreshed to c2).
    let r = ingest(&enabled, t, "doc1", c2).await;
    assert_eq!(
        r["status"], "replayed",
        "identical re-ingest of the updated content is a replay"
    );
    assert_eq!(
        max_chunk_ts(&admin, t, "doc1").await,
        ts3,
        "replay of the updated content is a no-op"
    );

    // (5) Toggle OFF: the SAME content re-ingests every time (no idempotency, rewrites chunks).
    let r = ingest(&disabled, t, "doc1", c2).await;
    assert_eq!(
        r["status"], "ingested",
        "with idempotency off, every ingest re-writes"
    );
    assert!(
        max_chunk_ts(&admin, t, "doc1").await > ts3,
        "idempotency-off re-ingest rewrote the chunks"
    );

    admin.close().await;
    println!("ingest idempotency: identical content -> replay (chunks untouched, no re-embed); changed content -> re-ingest; toggle off -> always re-ingest.");
}

/// The fingerprint keys on the WHOLE request, so a re-ingest that keeps the CONTENT but changes
/// metadata/ACL must NOT replay — it must re-ingest and APPLY the change. Guards the confirmed review
/// finding that a content-only fingerprint would silently drop a metadata update or an ACL grant
/// (a security divergence from the toggle-off path).
#[tokio::test]
async fn reingest_with_changed_metadata_or_acl_is_not_a_replay() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "ingestidemmeta").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let enabled = router_for(
        app_pool(&test_db.url, &test_db.role).await,
        &test_db.url,
        true,
    );

    let content = "Shared incident bridge etiquette. Announce yourself, defer to the incident commander, keep the channel focused.";
    let body = |scope: Value, groups: Value| {
        json!({
            "doc_id": "doc-meta",
            "tenant_id": t,
            "team_scope": scope,
            "source_uri": "uri://doc-meta",
            "acl": { "users": [], "groups": groups, "inherit_from_source": false },
            "content": content,
        })
    };

    // (1) First ingest: team_scope ["eng"], grant read to group "team-a".
    let r = ingest_raw(&enabled, t, body(json!(["eng"]), json!(["team-a"]))).await;
    assert_eq!(r["status"], "ingested", "first ingest");
    let ts1 = max_chunk_ts(&admin, t, "doc-meta").await;
    assert!(
        acl_grant_exists(&admin, t, "doc-meta", "team-a").await,
        "team-a granted on first ingest"
    );

    // (2) Re-ingest IDENTICAL content but CHANGED metadata + ACL -> must re-ingest (NOT replay) and
    // apply BOTH: team_scope widens to ["eng","sales"] and group "team-b" is newly granted. A
    // content-only fingerprint would have replayed here and silently dropped both changes.
    let r = ingest_raw(
        &enabled,
        t,
        body(json!(["eng", "sales"]), json!(["team-a", "team-b"])),
    )
    .await;
    assert_eq!(
        r["status"], "ingested",
        "same content + changed metadata/ACL must NOT replay"
    );
    assert_eq!(
        team_scope_of(&admin, t, "doc-meta").await,
        vec!["eng", "sales"],
        "metadata update applied"
    );
    assert!(
        acl_grant_exists(&admin, t, "doc-meta", "team-b").await,
        "the new ACL grant landed (not silently dropped)"
    );
    assert!(
        max_chunk_ts(&admin, t, "doc-meta").await > ts1,
        "re-ingest rewrote the chunks"
    );

    // (3) Re-ingest a BYTE-IDENTICAL request -> now it replays (nothing changed).
    let ts3 = max_chunk_ts(&admin, t, "doc-meta").await;
    let r = ingest_raw(
        &enabled,
        t,
        body(json!(["eng", "sales"]), json!(["team-a", "team-b"])),
    )
    .await;
    assert_eq!(r["status"], "replayed", "byte-identical re-ingest replays");
    assert_eq!(
        max_chunk_ts(&admin, t, "doc-meta").await,
        ts3,
        "replay did not rewrite chunks"
    );

    admin.close().await;
    println!("ingest idempotency: request-hash keying — same content + changed metadata/ACL re-ingests and applies the change; byte-identical request replays.");
}
