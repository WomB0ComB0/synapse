//! Integration test for the ingest -> retrieve vertical slice.
//!
//! **DB-gated:** the whole test is skipped (returns early) unless `DATABASE_URL`
//! is set, so CI stays database-free and green. To run it locally against a
//! pgvector Postgres:
//!
//! ```bash
//! docker run --rm -d --name synapse-pgvec -e POSTGRES_PASSWORD=postgres \
//!     -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! # Each test provisions its OWN throwaway database, so they run in parallel:
//! cargo test --test retrieve_it -- --nocapture
//! ```
//!
//! What it proves:
//! - (a) `/retrieve` returns the relevant chunk for tenant A;
//! - (b) a query issued as tenant A never returns tenant B's chunks — enforced by
//!   Postgres **Row-Level Security**, not by application-side filtering (the app
//!   pool runs as a non-superuser role, so RLS actually bites);
//! - (c) for a paraphrased query with no lexical overlap, **hybrid** surfaces the
//!   relevant chunk that **pure lexical** misses entirely.
//!
//! Note: `MockEmbedder` is deterministic, not semantic, so (c) demonstrates the
//! *pipeline* property (the vector arm contributes a candidate the lexical arm
//! cannot), which is the mechanism hybrid retrieval exists to provide.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use synapse::config::Config;
use synapse::state::AppState;

/// A throwaway, per-test database + app role so the DB-gated tests are fully
/// isolated and can run in parallel.
///
/// `pg_extension` is a **database-global** catalog and each test's
/// `apply_schema` drops + recreates the extensions (they live in `public`), so
/// two tests sharing one database race on it
/// (`duplicate key value violates unique constraint "pg_extension_name_index"`).
/// Giving each test its own database sidesteps that entirely; roles are
/// **cluster-global**, so the app role is likewise per-test-unique.
struct TestDb {
    /// URL of the freshly-created per-test database.
    url: String,
    /// Non-superuser app role the pool `SET ROLE`s to, so RLS is enforced.
    role: String,
}

impl TestDb {
    /// Create `synapse_it_<slug>` (dropping any leftover from a prior run) on the
    /// server named by `base_url`, returning its URL and a per-test app role name.
    async fn create(base_url: &str, slug: &str) -> TestDb {
        let db_name = format!("synapse_it_{slug}");
        let role = format!("synapse_app_{slug}");

        // CREATE/DROP DATABASE can't run inside a transaction and must be issued
        // from a different (maintenance) database — use the one in `base_url`.
        let maint = PgPoolOptions::new()
            .max_connections(1)
            .connect(base_url)
            .await
            .expect("connect maintenance db");
        // FORCE (pg13+) evicts any lingering connections from a crashed prior run.
        sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
            .execute(&maint)
            .await
            .expect("drop leftover test db");
        sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&maint)
            .await
            .expect("create test db");
        maint.close().await;

        TestDb {
            url: swap_db(base_url, &db_name),
            role,
        }
    }
}

/// Replace the database name (last path segment) of a Postgres URL, preserving
/// any `?query` suffix.
fn swap_db(base_url: &str, db_name: &str) -> String {
    let (head, query) = match base_url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base_url, None),
    };
    let head = match head.rfind('/') {
        Some(i) => format!("{}/{db_name}", &head[..i]),
        None => format!("{head}/{db_name}"),
    };
    match query {
        Some(q) => format!("{head}?{q}"),
        None => head,
    }
}

/// Reset the schema, apply every migration in order, create the given app
/// `role`, and seed the two tenants. Runs as the superuser `admin` pool
/// (bypasses RLS for provisioning, exactly as the migration 0009 comments
/// prescribe).
async fn apply_schema(admin: &PgPool, role: &str) {
    sqlx::raw_sql("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;")
        .execute(admin)
        .await
        .expect("reset public schema");

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort();
    for f in &files {
        let sql = std::fs::read_to_string(f).expect("read migration");
        sqlx::raw_sql(&sql)
            .execute(admin)
            .await
            .unwrap_or_else(|e| panic!("migration {} failed: {e}", f.display()));
    }

    // Non-superuser application role. We use SET ROLE from the superuser login
    // (see `app_pool`), so NOLOGIN is fine; being non-superuser is what matters.
    sqlx::raw_sql(&format!(
        "DROP ROLE IF EXISTS {role};
         CREATE ROLE {role} NOLOGIN;
         GRANT USAGE ON SCHEMA public TO {role};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {role};
         GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {role};
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO {role};"
    ))
    .execute(admin)
    .await
    .expect("create app role");

    sqlx::raw_sql(
        "INSERT INTO tenants (tenant_id, name)
         VALUES ('tenant_a','Tenant A'), ('tenant_b','Tenant B')
         ON CONFLICT DO NOTHING;",
    )
    .execute(admin)
    .await
    .expect("seed tenants");
}

/// Build the app's pool. Every connection `SET ROLE`s to the non-superuser app
/// role so the RLS policies from migration 0009 are actually enforced.
async fn app_pool(database_url: &str, role: &str) -> PgPool {
    let role = role.to_string();
    PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _meta| {
            let role = role.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET ROLE {role}"))
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("connect app pool")
}

fn router_for(pool: PgPool, database_url: &str) -> Router {
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
        ingest_idempotency_enabled: false,
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

async fn post_json(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("x-principal-id", principal)
                .header("x-tenant-id", tenant)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, val)
}

async fn ingest(router: &Router, tenant: &str, doc_id: &str, content: &str) {
    let body = json!({
        "doc_id": doc_id,
        "tenant_id": tenant,
        "team_scope": ["eng"],
        "source_uri": format!("uri://{doc_id}"),
        "content": content,
    });
    let (status, val) = post_json(router, "/documents.ingest", tenant, "user_1", body).await;
    assert_eq!(status, StatusCode::OK, "ingest {doc_id} failed: {val}");
    assert_eq!(val["doc_id"], doc_id);
    assert!(
        val["chunks_ingested"].as_u64().unwrap() >= 1,
        "expected >=1 chunk for {doc_id}, got {val}"
    );
}

/// Extract the `doc_id` of each result, in ranked order.
fn doc_ids(resp: &Value) -> Vec<String> {
    resp["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["doc_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn ingest_retrieve_hybrid_with_rls_isolation() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };

    // --- provisioning (superuser), into this test's own throwaway database ---
    let test_db = TestDb::create(&base_url, "hybrid").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin pool");
    apply_schema(&admin, &test_db.role).await;

    // --- the app, running as the non-superuser (RLS-enforced) role ---
    let pool = app_pool(&test_db.url, &test_db.role).await;
    let router = router_for(pool, &test_db.url);

    // Tenant A: a relevant runbook + an unrelated HR distractor.
    ingest(
        &router,
        "tenant_a",
        "A_runbook",
        "# Incident Response\n\nThis runbook covers database outage recovery. When the \
         primary database becomes unavailable, promote the replica and restore service \
         through failover.",
    )
    .await;
    ingest(
        &router,
        "tenant_a",
        "A_hr",
        "Our paid time off policy grants employees vacation days each quarter based on tenure.",
    )
    .await;
    // Tenant B: a secret only tenant B should ever see.
    ingest(
        &router,
        "tenant_b",
        "B_secret",
        "Project Pineapple is a confidential acquisition codename for tenant B leadership only.",
    )
    .await;

    // --- (a) relevant chunk comes back for tenant A ---
    let (status, resp) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "user_1",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "user_1",
            "query": "database outage runbook",
            "retrieval": { "mode": "hybrid", "top_k": 5 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = doc_ids(&resp);
    println!("(a) hybrid 'database outage runbook' -> {ids:?}");
    assert!(!ids.is_empty(), "expected relevant results for tenant A");
    assert_eq!(ids[0], "A_runbook", "top hit should be the runbook");
    assert!(resp["trace_id"].is_string());

    // --- (b) tenant isolation: tenant A never sees tenant B's chunk ---
    let (status, resp_a) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "user_1",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "user_1",
            "query": "pineapple confidential acquisition",
            "retrieval": { "mode": "hybrid", "top_k": 10 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids_a = doc_ids(&resp_a);
    // Same query as tenant B DOES surface the secret — proving (b) is RLS, not absence.
    let (_status, resp_b) = post_json(
        &router,
        "/retrieve",
        "tenant_b",
        "user_2",
        json!({
            "tenant_id": "tenant_b",
            "principal_id": "user_2",
            "query": "pineapple confidential acquisition",
            "retrieval": { "mode": "hybrid", "top_k": 10 }
        }),
    )
    .await;
    let ids_b = doc_ids(&resp_b);
    println!("(b) tenant_A sees {ids_a:?}; tenant_B sees {ids_b:?}");
    assert!(
        !ids_a.iter().any(|d| d == "B_secret"),
        "RLS FAILED: tenant A leaked tenant B's chunk: {ids_a:?}"
    );
    assert!(
        ids_b.iter().any(|d| d == "B_secret"),
        "tenant B should retrieve its own secret: {ids_b:?}"
    );
    println!("(b) RLS isolation OK: tenant_A did NOT retrieve B_secret; tenant_B did.");

    // --- (c) hybrid beats pure-lexical for a paraphrased (no-overlap) query ---
    let paraphrase = "server downtime switchover cluster maintenance";
    let (_s, hybrid) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "user_1",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "user_1",
            "query": paraphrase,
            "retrieval": { "mode": "hybrid", "top_k": 5 }
        }),
    )
    .await;
    let (_s, lexical) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "user_1",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "user_1",
            "query": paraphrase,
            "retrieval": { "mode": "lexical", "top_k": 5 }
        }),
    )
    .await;
    let hybrid_ids = doc_ids(&hybrid);
    let lexical_ids = doc_ids(&lexical);
    println!("(c) paraphrase hybrid -> {hybrid_ids:?}; lexical -> {lexical_ids:?}");
    assert!(
        hybrid_ids.iter().any(|d| d == "A_runbook"),
        "hybrid should surface the runbook via the vector arm: {hybrid_ids:?}"
    );
    assert!(
        !lexical_ids.iter().any(|d| d == "A_runbook"),
        "pure lexical should miss the no-overlap paraphrase: {lexical_ids:?}"
    );
    println!("(c) hybrid beat pure-lexical for the paraphrase.");
}

/// Per-tenant doc identity: the SAME `doc_id` ("runbook") ingested into two
/// different tenants with different content must
///   (a) BOTH upsert without a unique-constraint collision — proving the
///       composite `(tenant_id, doc_id)` key (globally-unique doc_id would 409),
///   (b) stay isolated at retrieval time — tenant A's `/retrieve` for that doc
///       returns ONLY tenant A's content, never tenant B's (composite key +
///       tenant-keyed chunk→document join + RLS).
///
/// Provisions its own throwaway database via `TestDb` (see the module docs), so
/// it is fully isolated from the other DB test and safe to run in parallel.
#[tokio::test]
async fn same_doc_id_across_tenants_is_isolated() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };

    // --- provisioning (superuser), into this test's own throwaway database ---
    let test_db = TestDb::create(&base_url, "same_doc").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin pool");
    apply_schema(&admin, &test_db.role).await;

    // --- the app, running as the non-superuser (RLS-enforced) role ---
    let pool = app_pool(&test_db.url, &test_db.role).await;
    let router = router_for(pool, &test_db.url);

    // Same doc_id, two tenants, DIFFERENT content marked with unique sentinels.
    let a_content = "# Runbook\n\nTenant A failover procedure. Sentinel token \
                     alpha_sentinel_aaa: when the primary database becomes unavailable, \
                     promote the standby replica and restore service.";
    let b_content = "# Runbook\n\nTenant B confidential plan. Sentinel token \
                     bravo_sentinel_bbb: codename pineapple, for tenant B leadership only.";

    // --- (a) BOTH upserts of the SAME doc_id succeed — no cross-tenant collision ---
    ingest(&router, "tenant_a", "runbook", a_content).await;
    ingest(&router, "tenant_b", "runbook", b_content).await;
    println!(
        "(a) same doc_id 'runbook' ingested into BOTH tenant_a and tenant_b — \
         no unique-constraint collision (per-tenant identity holds)."
    );

    // --- (b) tenant A retrieves the shared doc_id and sees ONLY its own content ---
    let (status, resp_a) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "user_1",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "user_1",
            "query": "runbook failover replica database",
            "retrieval": { "mode": "hybrid", "top_k": 10 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results_a = resp_a["results"].as_array().unwrap();
    assert!(
        !results_a.is_empty(),
        "tenant A expected results for the shared doc_id"
    );
    let ids_a = doc_ids(&resp_a);
    assert!(
        ids_a.iter().any(|d| d == "runbook"),
        "tenant A should retrieve its own 'runbook': {ids_a:?}"
    );
    let text_a: String = results_a
        .iter()
        .map(|r| r["text"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        text_a.contains("alpha_sentinel_aaa"),
        "tenant A should see its OWN content: {text_a}"
    );
    assert!(
        !text_a.contains("bravo_sentinel_bbb"),
        "LEAK: tenant A retrieved tenant B's content for the shared doc_id: {text_a}"
    );
    // Every 'runbook' hit for tenant A must carry tenant A's text, never tenant B's.
    for r in results_a {
        if r["doc_id"].as_str() == Some("runbook") {
            let t = r["text"].as_str().unwrap();
            assert!(
                t.contains("Tenant A") || t.contains("alpha_sentinel_aaa"),
                "the 'runbook' chunk for tenant A must be tenant A's content: {t}"
            );
            assert!(
                !t.contains("bravo_sentinel_bbb"),
                "LEAK: tenant A's 'runbook' chunk carried tenant B content: {t}"
            );
        }
    }
    println!("(b) tenant_A 'runbook' -> {ids_a:?}; content isolated (no tenant_B sentinel).");

    // Positive control: tenant B's SAME doc_id returns tenant B's content, not A's.
    let (_s, resp_b) = post_json(
        &router,
        "/retrieve",
        "tenant_b",
        "user_2",
        json!({
            "tenant_id": "tenant_b",
            "principal_id": "user_2",
            "query": "runbook pineapple codename leadership",
            "retrieval": { "mode": "hybrid", "top_k": 10 }
        }),
    )
    .await;
    let results_b = resp_b["results"].as_array().unwrap();
    let text_b: String = results_b
        .iter()
        .map(|r| r["text"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        text_b.contains("bravo_sentinel_bbb"),
        "tenant B should retrieve its OWN 'runbook' content: {text_b}"
    );
    assert!(
        !text_b.contains("alpha_sentinel_aaa"),
        "LEAK: tenant B retrieved tenant A's content for the shared doc_id: {text_b}"
    );
    println!(
        "(b) tenant_B 'runbook' content isolated (no tenant_A sentinel). \
         Per-tenant doc identity + isolation proven."
    );
}
