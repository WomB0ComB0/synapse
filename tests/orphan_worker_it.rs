//! Live integration test: the crash-recovery worker reconciles runs orphaned by a driver
//! crash (executor PR5). Runs stuck in `running` past the staleness bound are discovered
//! ACROSS tenants (via the SECURITY DEFINER function, migration 0016) and reconciled: a run
//! left mid tool-execution is FAILED safely; a run stranded between steps is DRIVEN forward;
//! a run that is still fresh is LEFT ALONE.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5465:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5465/postgres
//! cargo test --test orphan_worker_it -- --nocapture
//! ```

mod common;

use common::{app_pool, apply_schema, TestDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use synapse::mcp::ConnectorImpl;
use synapse::orchestration::worker;

/// Seed a `running` run (via the superuser admin pool, so any tenant is writable) whose
/// `updated_at` is `age_secs` in the past, and return its id. `age_secs = 0` is a FRESH run.
async fn seed_run(admin: &PgPool, tenant: &str, run_type: &str, age_secs: i64) -> Uuid {
    // Ensure the initiating principal exists so the composite runs->principals FK holds.
    sqlx::query(
        "INSERT INTO principals (tenant_id, principal_id) VALUES ($1, 'agent') \
         ON CONFLICT DO NOTHING",
    )
    .bind(tenant)
    .execute(admin)
    .await
    .expect("seed principal");
    // The touch trigger is BEFORE UPDATE only, so an explicit `updated_at` on INSERT stands.
    let (run_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO runs (tenant_id, principal_id, run_type, workflow_id, status, updated_at) \
         VALUES ($1, 'agent', $2, $2, 'running', now() - make_interval(secs => $3)) \
         RETURNING run_id",
    )
    .bind(tenant)
    .bind(run_type)
    .bind(age_secs)
    .fetch_one(admin)
    .await
    .expect("seed run");
    run_id
}

/// Seed a `run_steps` row.
async fn seed_step(
    admin: &PgPool,
    tenant: &str,
    run_id: Uuid,
    step_index: i32,
    name: &str,
    kind: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO run_steps \
            (run_id, tenant_id, step_index, name, kind, status, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(run_id)
    .bind(tenant)
    .bind(step_index)
    .bind(name)
    .bind(kind)
    .bind(status)
    .bind(format!("{run_id}-{step_index}"))
    .execute(admin)
    .await
    .expect("seed step");
}

/// Seed a pre-call `approved` `tool_executions` ledger intent for a run.
async fn seed_tool_intent(admin: &PgPool, tenant: &str, run_id: Uuid, tool_id: &str) {
    sqlx::query(
        "INSERT INTO tool_executions (tenant_id, principal_id, run_id, tool_id, status) \
         VALUES ($1, 'agent', $2, $3, 'approved')",
    )
    .bind(tenant)
    .bind(run_id)
    .bind(tool_id)
    .execute(admin)
    .await
    .expect("seed tool intent");
}

async fn run_status(admin: &PgPool, run_id: Uuid) -> String {
    let (s,): (String,) = sqlx::query_as("SELECT status FROM runs WHERE run_id = $1")
        .bind(run_id)
        .fetch_one(admin)
        .await
        .unwrap();
    s
}

async fn step_status(admin: &PgPool, run_id: Uuid, step_index: i32) -> String {
    let (s,): (String,) =
        sqlx::query_as("SELECT status FROM run_steps WHERE run_id = $1 AND step_index = $2")
            .bind(run_id)
            .bind(step_index)
            .fetch_one(admin)
            .await
            .unwrap();
    s
}

async fn tool_exec_status(admin: &PgPool, run_id: Uuid) -> String {
    let (s,): (String,) = sqlx::query_as("SELECT status FROM tool_executions WHERE run_id = $1")
        .bind(run_id)
        .fetch_one(admin)
        .await
        .unwrap();
    s
}

#[tokio::test]
async fn worker_reconciles_orphaned_runs_across_tenants() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };

    let test_db = TestDb::create(&base_url, "orphanworker").await;
    let admin = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    // The worker runs with the RLS-enforcing app role; cross-tenant discovery works ONLY via
    // the SECURITY DEFINER function, so exercising it through the app pool is the real test.
    let pool = app_pool(&test_db.url, &test_db.role).await;
    // No connector needed: the fail-safe path never calls it, and the drive-forward run is all
    // `transform` steps (completed inline).
    let connector = ConnectorImpl::Disabled;

    // --- Seed four runs (all `running`) --------------------------------------------------
    // (a) tenant_a, crashed mid tool-execution: leading step still `running`.
    let a = seed_run(&admin, "tenant_a", "tool.auto", 3600).await;
    seed_step(&admin, "tenant_a", a, 0, "deploy", "tool", "running").await;
    seed_tool_intent(&admin, "tenant_a", a, "deploy.prod").await;
    // (b) tenant_b, same shape — proves cross-tenant discovery in ONE pass.
    let b = seed_run(&admin, "tenant_b", "tool.auto", 3600).await;
    seed_step(&admin, "tenant_b", b, 0, "deploy", "tool", "running").await;
    seed_tool_intent(&admin, "tenant_b", b, "deploy.prod").await;
    // (c) tenant_a, crashed BETWEEN steps: step 0 done, step 1 `pending` (all transforms).
    let c = seed_run(&admin, "tenant_a", "tform", 3600).await;
    seed_step(&admin, "tenant_a", c, 0, "prep", "transform", "completed").await;
    seed_step(&admin, "tenant_a", c, 1, "work", "transform", "pending").await;
    // (d) tenant_a, FRESH (updated_at = now()): a live in-flight run that must be LEFT ALONE.
    let d = seed_run(&admin, "tenant_a", "tool.auto", 0).await;
    seed_step(&admin, "tenant_a", d, 0, "deploy", "tool", "running").await;
    seed_tool_intent(&admin, "tenant_a", d, "deploy.prod").await;

    // --- Reconcile: stale threshold 60s discovers a/b/c (aged 1h) but NOT d (fresh) ------
    let reconciled = worker::reconcile_runs(&pool, &connector, 60)
        .await
        .expect("reconcile pass");
    assert_eq!(reconciled, 3, "leased + acted on a, b, c (not the fresh d)");

    // (a) failed safely: run, step, and the dangling ledger intent are all `failed`.
    assert_eq!(
        run_status(&admin, a).await,
        "failed",
        "(a) orphaned run failed"
    );
    assert_eq!(
        step_status(&admin, a, 0).await,
        "failed",
        "(a) orphaned step failed"
    );
    assert_eq!(
        tool_exec_status(&admin, a).await,
        "failed",
        "(a) ledger intent finalized failed"
    );

    // (b) same, in a DIFFERENT tenant — the SECURITY DEFINER discovery spans tenants.
    assert_eq!(
        run_status(&admin, b).await,
        "failed",
        "(b) cross-tenant orphan failed"
    );
    assert_eq!(
        step_status(&admin, b, 0).await,
        "failed",
        "(b) cross-tenant step failed"
    );
    assert_eq!(
        tool_exec_status(&admin, b).await,
        "failed",
        "(b) cross-tenant intent failed"
    );

    // (c) driven forward to completion (the between-steps crash is safely recoverable).
    assert_eq!(
        run_status(&admin, c).await,
        "completed",
        "(c) stranded run driven to completion"
    );
    assert_eq!(
        step_status(&admin, c, 1).await,
        "completed",
        "(c) pending step completed"
    );

    // (d) untouched: a fresh (non-stale) run is never reconciled out from under its live driver.
    assert_eq!(
        run_status(&admin, d).await,
        "running",
        "(d) fresh run left running"
    );
    assert_eq!(
        step_status(&admin, d, 0).await,
        "running",
        "(d) fresh step left running"
    );
    assert_eq!(
        tool_exec_status(&admin, d).await,
        "approved",
        "(d) fresh intent untouched"
    );

    // --- Idempotent: a second pass finds nothing new (a/b/c are terminal, d still fresh) --
    let again = worker::reconcile_runs(&pool, &connector, 60)
        .await
        .expect("second reconcile pass");
    assert_eq!(again, 0, "no runs left to reconcile");

    admin.close().await;
    pool.close().await;
    println!(
        "orphan worker: cross-tenant discovery via SECURITY DEFINER reconciles a run left \
         mid tool-execution (fail-safe), drives one stranded between steps to completion, and \
         leaves a fresh run untouched; the pass is idempotent."
    );
}
