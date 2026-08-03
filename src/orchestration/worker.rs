//! Background worker: reconciles runs that need driving but have no live in-request driver —
//! crash orphans and due step retries.
//!
//! A run is driven in-request (`runs.start`/`runs.resume`); if that process dies mid-drive the
//! run is stranded in `running`, and a retriable step failure deliberately hands off to this
//! worker (rescheduled with a backoff). Each poll discovers such runs ACROSS tenants and:
//!
//! * **crashed mid tool-execution** (a leading `run_steps` row still `running`): the external
//!   side effect may or may not have happened and the tool is non-idempotent, so we neither
//!   assume success nor re-execute — we FAIL the run safely and finalize the dangling ledger
//!   intent. (At-least-once run execution is the documented contract; a fabricated success or
//!   a double-execution would both be worse than an honest failure.)
//! * **crashed between steps** (leading step `pending` with no scheduled retry, or all steps
//!   done but the run was never finalized): drive forward via [`executor::advance`].
//! * **a due step retry** (leading step `pending` whose `next_attempt_at` backoff has elapsed):
//!   drive forward — re-executes the rescheduled step (see [`executor`] retry/backoff).
//!
//! ### Cross-tenant discovery
//! The app role is RLS-enforcing (`FORCE RLS`) and sees only its own tenant, but drivable runs
//! span every tenant. Discovery goes through the SECURITY DEFINER function
//! `synapse_list_drivable_runs` (migration 0017), which runs as its owner (a superuser/BYPASSRLS
//! role) and returns only `(run_id, tenant_id)`. Each run is then RECONCILED inside a normal
//! [`tenant_tx`] under RLS, so the privilege boundary is a single auditable read.
//!
//! ### Safety against reconciling a LIVE run
//! The crash-recovery cases fire only once a run's `updated_at` (bumped by `trg_runs_touch`) is
//! older than `stale_secs`, which MUST exceed the longest tool-call duration (see
//! [`crate::config::Config::worker_stale_secs`]) — so a live driver mid out-of-tx tool call
//! (its `runs` row unlocked) is never failed. A DUE retry has a leading `pending` step (never
//! `running`), so it can't collide with an in-flight tool call, and is driven regardless of
//! staleness. `FOR UPDATE SKIP LOCKED` yields to an actively-driving in-request driver.

use std::sync::Arc;
use std::time::Duration;

use crate::db::tenant_tx;
use crate::error::Result;
use crate::ingestion;
use crate::mcp::ConnectorImpl;
use crate::orchestration::executor::{self, AdvanceOutcome};
use crate::retrieval::embed::EmbedderImpl;

/// Discover every run (across ALL tenants) that needs a background driver — a crash orphan
/// (stale `running`) OR a step whose retry backoff has elapsed — and reconcile each. Returns
/// the number of runs this pass actually LEASED and acted on (a run skipped because it is
/// locked, already advanced, not stale, or whose retry isn't due yet is not counted). A per-run
/// error is logged and does not abort the pass — one poisoned run must not stall the rest.
pub async fn reconcile_runs(
    db: &sqlx::PgPool,
    connector: &ConnectorImpl,
    stale_secs: i64,
) -> Result<usize> {
    // Cross-tenant discovery via the SECURITY DEFINER function (RLS-bypassing, read-only).
    let drivable: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT run_id, tenant_id FROM synapse_list_drivable_runs($1)")
            .bind(stale_secs)
            .fetch_all(db)
            .await?;

    let mut reconciled = 0usize;
    for (run_id, tenant) in drivable {
        match reconcile_one(db, &tenant, run_id, connector, stale_secs).await {
            Ok(true) => reconciled += 1,
            Ok(false) => {} // not leased (locked / recovered / not stale / retry not due)
            Err(e) => {
                tracing::warn!(error = %e, %run_id, tenant = %tenant, "run reconcile failed");
            }
        }
    }
    Ok(reconciled)
}

/// Reconcile a single candidate. Returns `Ok(true)` if this call leased the run and acted on
/// it, `Ok(false)` if the lease was declined (another driver/worker holds it, it already
/// advanced, it is not a stale crash, or its retry isn't due yet — all benign races).
async fn reconcile_one(
    db: &sqlx::PgPool,
    tenant: &str,
    run_id: uuid::Uuid,
    connector: &ConnectorImpl,
    stale_secs: i64,
) -> Result<bool> {
    let mut tx = tenant_tx(db, tenant).await?;

    // Lease: lock the `running` run (`SKIP LOCKED` yields to any concurrent driver/worker
    // instead of blocking) and compute its staleness under the lock. Staleness gates the CRASH
    // cases only — a due retry is driven regardless of `updated_at` (its in-request driver
    // deliberately handed off to the worker; it isn't "stale").
    let leased: Option<(bool,)> = sqlx::query_as(
        "SELECT (updated_at < now() - make_interval(secs => $2)) AS is_stale FROM runs \
         WHERE run_id = $1 AND status = 'running' FOR UPDATE SKIP LOCKED",
    )
    .bind(run_id)
    .bind(stale_secs)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((is_stale,)) = leased else {
        return Ok(false);
    };

    // The first non-terminal step by index (same ordering the executor advances), plus whether
    // a scheduled retry is DUE: `NULL` = no retry scheduled, `Some(true)` = backoff elapsed.
    let leading: Option<(i32, String, String, Option<bool>)> = sqlx::query_as(
        "SELECT step_index, name, status, \
                CASE WHEN next_attempt_at IS NULL THEN NULL ELSE (next_attempt_at <= now()) END \
                    AS retry_due \
         FROM run_steps \
         WHERE run_id = $1 AND status NOT IN ('completed', 'skipped') \
         ORDER BY step_index LIMIT 1",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;

    match leading
        .as_ref()
        .map(|(_, _, status, due)| (status.as_str(), *due))
    {
        // Crashed mid tool-execution: fail-safe ONLY when stale (the staleness window ensures a
        // live driver's out-of-tx connector call has elapsed — else we could fail a live run).
        Some(("running", _)) if is_stale => {
            let (step_index, name, ..) = leading.unwrap();
            fail_orphan_step(&mut tx, tenant, run_id, step_index, &name).await?;
            tx.commit().await?;
            tracing::warn!(%run_id, tenant, step_index, "reconciled orphan: failed a run left mid tool-execution");
            Ok(true)
        }
        // A DUE retry (backoff elapsed): drive it forward — re-executes the rescheduled step.
        Some(("pending", Some(true))) => {
            drive_forward(db, tenant, run_id, connector, tx).await?;
            tracing::info!(%run_id, tenant, "reconciled: drove a due step retry");
            Ok(true)
        }
        // Crashed BETWEEN steps (leading `pending`, no scheduled retry) — drive only when stale.
        Some(("pending", None)) if is_stale => {
            drive_forward(db, tenant, run_id, connector, tx).await?;
            tracing::info!(%run_id, tenant, "reconciled orphan: drove a run stranded between steps");
            Ok(true)
        }
        // All steps done but the run was never finalized (crashed before the terminal write).
        None if is_stale => {
            drive_forward(db, tenant, run_id, connector, tx).await?;
            tracing::info!(%run_id, tenant, "reconciled orphan: finalized a run with no pending steps");
            Ok(true)
        }
        // Anything else — a live run mid tool-execution (not yet stale), a retry not yet due, a
        // not-stale between-steps state, or an unexpected leading step — is left untouched (the
        // lease rolls back on drop).
        _ => Ok(false),
    }
}

/// Drive a leased run forward exactly as the in-request driver would: advance in-tx, and if it
/// pauses for a real tool execution, commit + `continue_run` OUTSIDE the tx. Retries are enabled
/// (the worker is the retry driver), so a further retriable failure reschedules another backoff.
async fn drive_forward(
    db: &sqlx::PgPool,
    tenant: &str,
    run_id: uuid::Uuid,
    connector: &ConnectorImpl,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<()> {
    match executor::advance(&mut tx, tenant, run_id, connector, true).await? {
        AdvanceOutcome::ExecuteTool(call) => {
            tx.commit().await?;
            executor::continue_run(db, tenant, run_id, connector, call, true).await?;
        }
        _ => {
            tx.commit().await?;
        }
    }
    Ok(())
}

/// Fail an orphaned run whose leading step was left `running` (an in-flight tool execution):
/// finalize the dangling ledger intent, fail the step + the run, and append ordered events.
async fn fail_orphan_step(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    step_index: i32,
    name: &str,
) -> Result<()> {
    let err = format!(
        "run reconciled by crash-recovery worker: tool step '{name}' was left in-flight \
         (its external outcome is unknown); failing the run for safety"
    );
    // Finalize the pre-call ledger intent (the `approved` row written before an auto tool call,
    // or a gated `pending` row) so no execution record is left dangling. COALESCE preserves any
    // result that a partially-completed finalize had already written.
    sqlx::query(
        "UPDATE tool_executions SET status = 'failed', result = COALESCE(result, $2) \
         WHERE run_id = $1 AND status IN ('approved', 'pending')",
    )
    .bind(run_id)
    .bind(sqlx::types::Json(&serde_json::json!({ "error": &err })))
    .execute(&mut **tx)
    .await?;
    // Fail the orphaned (leading `running`) step.
    sqlx::query(
        "UPDATE run_steps SET status = 'failed', error = $2 \
         WHERE run_id = $1 AND step_index = $3 AND status = 'running'",
    )
    .bind(run_id)
    .bind(&err)
    .bind(step_index)
    .execute(&mut **tx)
    .await?;
    // Fail the run.
    sqlx::query("UPDATE runs SET status = 'failed', error = $2 WHERE run_id = $1")
        .bind(run_id)
        .bind(&err)
        .execute(&mut **tx)
        .await?;
    executor::append_event(
        tx,
        tenant,
        run_id,
        "step_failed",
        &serde_json::json!({ "step_index": step_index, "name": name, "error": err, "reconciled": true }),
    )
    .await?;
    executor::append_event(
        tx,
        tenant,
        run_id,
        "failed",
        &serde_json::json!({ "error": err, "reconciled": true }),
    )
    .await?;
    Ok(())
}

/// Discover stale AD-HOC `approved` tool intents (`run_id IS NULL`) across ALL tenants — an
/// ad-hoc `/tool.execute` that crashed between the pre-call `approved` commit and the best-effort
/// finalize — and FAIL each safely. The tool is non-idempotent, so we neither re-invoke it nor
/// assume success (mirroring the run-level fail-safe); failing it also clears the poison key so a
/// same-key retry stops returning 409 forever. The RUNS reconciler ([`reconcile_runs`]) never
/// sees these (it only handles `run_id`-bearing rows). Returns the number failed this pass.
pub async fn reconcile_stuck_tool_intents(db: &sqlx::PgPool, stale_secs: i64) -> Result<usize> {
    let stuck: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, tenant_id FROM synapse_list_stuck_tool_intents($1)")
            .bind(stale_secs)
            .fetch_all(db)
            .await?;
    let mut failed = 0usize;
    for (id, tenant) in stuck {
        match fail_stuck_tool_intent(db, &tenant, id, stale_secs).await {
            Ok(true) => failed += 1,
            Ok(false) => {} // not leased (locked / already finalized / no longer stale)
            Err(e) => {
                tracing::warn!(error = %e, %id, tenant = %tenant, "stuck tool-intent reconcile failed")
            }
        }
    }
    Ok(failed)
}

/// Lease + fail one stale ad-hoc `approved` tool intent (RLS-scoped to its tenant). Returns
/// `Ok(true)` if it acted. Staleness (`created_at`) is what protects a legitimately in-flight
/// call: the ad-hoc row is NOT row-locked during the out-of-tx connector call, so `SKIP LOCKED`
/// alone can't skip it — but `worker_stale_secs` exceeds the max tool-call duration, so a call
/// still in flight is never this old. `started_at` distinguishes never-dispatched from maybe-ran
/// (both fail, only the recorded note differs).
async fn fail_stuck_tool_intent(
    db: &sqlx::PgPool,
    tenant: &str,
    id: uuid::Uuid,
    stale_secs: i64,
) -> Result<bool> {
    let mut tx = tenant_tx(db, tenant).await?;
    let leased: Option<(bool,)> = sqlx::query_as(
        "SELECT (started_at IS NOT NULL) AS attempted FROM tool_executions \
         WHERE id = $1 AND run_id IS NULL AND status = 'approved' \
           AND created_at < now() - make_interval(secs => $2) \
         FOR UPDATE SKIP LOCKED",
    )
    .bind(id)
    .bind(stale_secs)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((attempted,)) = leased else {
        return Ok(false);
    };
    let note = if attempted {
        "ad-hoc tool call was attempted; outcome unknown (finalize lost to a crash) — failed by crash-recovery"
    } else {
        "ad-hoc tool call was never dispatched (crashed before the connector call) — failed by crash-recovery"
    };
    sqlx::query(
        "UPDATE tool_executions SET status = 'failed', finished_at = now(), \
         result = COALESCE(result, $2) WHERE id = $1 AND status = 'approved'",
    )
    .bind(id)
    .bind(sqlx::types::Json(
        &serde_json::json!({ "error": note, "reconciled": true }),
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::warn!(%id, tenant, attempted, "reconciled a stale ad-hoc tool intent to failed");
    Ok(true)
}

/// Spawn the crash-recovery worker as a detached background task, polling every `poll_secs`.
/// A transient reconcile error is logged and the loop continues to the next tick — the worker
/// never panics its task. The returned handle can be dropped; the task runs until the process
/// exits (each pass is transactional, so an abrupt shutdown leaves no partial reconcile).
pub fn spawn_worker(
    db: sqlx::PgPool,
    connector: Arc<ConnectorImpl>,
    embedder: Arc<EmbedderImpl>,
    embedding_model: String,
    poll_secs: u64,
    stale_secs: i64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(poll_secs.max(1)));
        // If a pass runs long (or the runtime stalls), don't fire a burst of catch-up ticks.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(poll_secs, stale_secs, "crash-recovery worker started");
        loop {
            ticker.tick().await;
            match reconcile_runs(&db, &connector, stale_secs).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(reconciled = n, "background worker reconciled run(s)"),
                Err(e) => tracing::warn!(error = %e, "background worker pass failed"),
            }
            // Embedding is idempotent and generation-guarded, so due or crash-stranded
            // jobs can be retried safely while lexical chunks remain queryable.
            match ingestion::reconcile_embedding_jobs(&db, &embedder, &embedding_model, stale_secs)
                .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    processed = n,
                    "background worker processed embedding job(s)"
                ),
                Err(e) => tracing::warn!(error = %e, "background worker embedding pass failed"),
            }

            // Also fail ad-hoc /tool.execute intents stranded `approved` by a crash (the runs
            // reconciler doesn't see run_id-NULL rows), clearing any poison idempotency key.
            match reconcile_stuck_tool_intents(&db, stale_secs).await {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(
                        failed = n,
                        "background worker failed stuck ad-hoc tool intent(s)"
                    )
                }
                Err(e) => tracing::warn!(error = %e, "background worker tool-intent pass failed"),
            }
        }
    })
}
