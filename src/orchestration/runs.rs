//! Durable run orchestration (`POST /runs.start`, `POST /runs.resume`).

use std::sync::Arc;

use crate::api::idempotency::{self, IdempotencyClaim};
use crate::db::tenant_tx;
use crate::domain::{RunResponse, RunsResumeRequest, RunsStartRequest};
use crate::error::{Error, Result};
use crate::mcp::ConnectorImpl;
use crate::orchestration::executor::{self, append_event, AdvanceOutcome};
use crate::orchestration::workflow;

/// Starts and resumes durable, multi-step workflow runs, tenant-isolated by RLS
/// (migrations 0009/0010).
///
/// A run is materialized into an ordered list of `run_steps` at start
/// (migrations 0010/0012) from the in-code [`workflow`] catalog, then driven by
/// [`executor::advance`]: `transform` steps complete inline, a `tool` step invokes
/// the policy-guarded gateway (auto-approved tools complete inline; an
/// approval-required tool suspends), a `human_approval` step suspends the run
/// `waiting` on a single-use checkpoint (migration 0007), and a resume continues
/// the run into the NEXT step (finalizing an approved tool's pending execution).
/// This is the substrate for "require audit + approval before autonomous writes".
/// State survives process restarts; every transition appends an ordered
/// `run_events` row.
pub struct Orchestrator {
    db: sqlx::PgPool,
    connector: Arc<ConnectorImpl>,
    /// Whether a retriable step failure is rescheduled with a backoff (driven later by the
    /// background worker) vs. failing the run terminally. Set from `WORKER_ENABLED` — retries
    /// need the worker to make progress, so they are only honored when it runs.
    worker_enabled: bool,
}

impl Orchestrator {
    /// Construct an orchestrator over the given pool + tool connector. `worker_enabled`
    /// (typically `config.worker_enabled`) gates retry/backoff of failed steps.
    pub fn new(db: sqlx::PgPool, connector: Arc<ConnectorImpl>, worker_enabled: bool) -> Self {
        Self {
            db,
            connector,
            worker_enabled,
        }
    }

    /// Start a new durable run for `tenant`, initiated by `principal_id`.
    ///
    /// Resolves the workflow plan, persists the run + its steps, records a
    /// `started` event, then drives the executor: the run ends `completed`,
    /// suspends `waiting` on the first approval gate (returning a resume token),
    /// or ends `failed` — all in one transaction.
    pub async fn start(
        &self,
        tenant: &str,
        principal_id: &str,
        req: &RunsStartRequest,
        idempotency_key: Option<&str>,
    ) -> Result<RunResponse> {
        let run_type = req.run_type.trim();
        if run_type.is_empty() {
            return Err(Error::BadRequest("run_type is required".into()));
        }
        // workflow_id is NOT NULL; default it to the run_type when omitted.
        let workflow_id = req
            .workflow_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(run_type);
        let input = default_if_null(&req.input);
        let plan = workflow::resolve(workflow_id, req.callbacks.human_approval);

        // Absolute-time timers: if the plan has a wait step that sources its wake instant from the
        // run input, parse + validate `input.wait_until` (RFC3339 UTC) UP FRONT so a bad value
        // fails the request before any run is persisted. Stored canonicalized to UTC RFC3339.
        let wait_until: Option<String> = if plan.steps.iter().any(|s| s.wait_until_from_input) {
            // Trim + drop empty, matching the `run_type` / `workflow_id` normalization above:
            // an omitted, empty, or whitespace-only value is "required" (not a confusing "invalid
            // RFC3339"), and a copy-paste-padded timestamp parses (chrono's RFC3339 is strict and
            // does not skip surrounding whitespace).
            let raw = input
                .get("wait_until")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::BadRequest(
                        "input.wait_until (an RFC3339 UTC timestamp) is required for this workflow"
                            .into(),
                    )
                })?;
            let ts = chrono::DateTime::parse_from_rfc3339(raw).map_err(|e| {
                Error::BadRequest(format!(
                    "input.wait_until is not a valid RFC3339 timestamp: {e}"
                ))
            })?;
            Some(ts.with_timezone(&chrono::Utc).to_rfc3339())
        } else {
            None
        };

        let mut tx = tenant_tx(&self.db, tenant).await?;

        // Generic idempotency gate (0024): when a key is present, record/verify the request
        // fingerprint. A repeat with a DIFFERENT body is a 409 (a key is bound to its first request);
        // a repeat with the SAME body falls through to the run-level ON CONFLICT replay below.
        // Atomic with the run insert (same tx).
        if let Some(key) = idempotency_key {
            let fp = idempotency::request_bytes(req);
            if idempotency::claim_idempotency_key(&mut tx, tenant, "runs.start", key, &fp).await?
                == IdempotencyClaim::Mismatch
            {
                return Err(Error::Conflict(
                    "Idempotency-Key was already used for a different runs.start request".into(),
                ));
            }
        }

        // Ensure the initiating principal exists (tenant-scoped) so the composite
        // runs -> principals FK holds. An unknown tenant (FK 23503) is a 400.
        sqlx::query(
            "INSERT INTO principals (principal_id, tenant_id) VALUES ($1, $2) \
             ON CONFLICT (tenant_id, principal_id) DO NOTHING",
        )
        .bind(principal_id)
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::db_or_conflict(e, "principal already exists"))?;

        // Persist the run in `running`; the executor sets the terminal status. With a client
        // `idempotency_key`, `ON CONFLICT DO NOTHING` makes a repeat a no-op — the claim commits
        // atomically with the run (this whole body is one tenant_tx), so a concurrent duplicate
        // sees the conflict and replays the original run rather than starting a second one. A
        // keyless run (NULL key) never conflicts on the partial index, so it always inserts
        // (today's at-least-once behavior, unchanged).
        let inserted: Option<(uuid::Uuid,)> = sqlx::query_as(
            "INSERT INTO runs \
                (tenant_id, principal_id, run_type, workflow_id, status, input, callbacks, idempotency_key) \
             VALUES ($1, $2, $3, $4, 'running', $5, $6, $7) \
             ON CONFLICT (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL \
             DO NOTHING \
             RETURNING run_id",
        )
        .bind(tenant)
        .bind(principal_id)
        .bind(run_type)
        .bind(workflow_id)
        .bind(sqlx::types::Json(&input))
        .bind(sqlx::types::Json(&req.callbacks))
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?;

        let run_id = match inserted {
            Some((run_id,)) => run_id,
            // The key already maps to a run — idempotent REPLAY. Return the ORIGINAL run's
            // current state (do NOT start a second run or re-materialize steps). `idempotency_key`
            // is necessarily `Some` here (a NULL key never conflicts on the partial index); guard
            // that invariant rather than binding a surprise NULL.
            None => {
                let key = idempotency_key.ok_or_else(|| {
                    Error::Internal(anyhow::anyhow!(
                        "runs INSERT hit ON CONFLICT with a NULL idempotency_key (unreachable)"
                    ))
                })?;
                return self.replay_run(tx, tenant, key).await;
            }
        };

        append_event(&mut tx, tenant, run_id, "started", &input).await?;

        // Materialize the ordered step plan (status defaults to 'pending'). A Tool
        // step's invocation config is serialized onto the row so the executor is
        // self-contained; non-tool steps store the '{}' default.
        for (i, step) in plan.steps.iter().enumerate() {
            let step_index = i as i32;
            // A Tool step serializes its invocation config; a flaky Transform carries its
            // `fail_until_attempt`; a Wait step carries its `wait_secs`; every other step keeps
            // the '{}' default.
            let config = if let Some(t) = step.tool.as_ref() {
                serde_json::to_value(t).expect("serialize tool step config")
            } else if let Some(k) = step.flaky_fail_until {
                serde_json::json!({ "fail_until_attempt": k })
            } else if step.wait_until_from_input {
                // Absolute-time timer: the wake instant is `input.wait_until` (validated above, so
                // `wait_until` is necessarily `Some` when any step carries this flag).
                serde_json::json!({ "wait_until": wait_until.as_deref().expect("wait_until validated above") })
            } else if let Some(secs) = step.wait_secs {
                serde_json::json!({ "wait_secs": secs })
            } else {
                serde_json::json!({})
            };
            sqlx::query(
                "INSERT INTO run_steps \
                    (run_id, tenant_id, step_index, name, kind, max_attempts, idempotency_key, config) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(run_id)
            .bind(tenant)
            .bind(step_index)
            .bind(&step.name)
            .bind(step.kind.as_str())
            .bind(step.max_attempts)
            .bind(workflow::idempotency_key(run_id, step_index))
            .bind(sqlx::types::Json(&config))
            .execute(&mut *tx)
            .await?;
        }

        let outcome = self.drive(tenant, run_id, tx).await?;

        Ok(RunResponse {
            run_id: run_id.to_string(),
            status: outcome.status().to_string(),
            resume_token: outcome.resume_token(),
        })
    }

    /// Replay a run claimed by an already-used idempotency key: return the ORIGINAL run's
    /// current state (RLS-scoped to `tenant`) instead of starting a second one. A `waiting` run
    /// re-derives its single-use resume token from the open checkpoint (the token lives there,
    /// not on the run row); every other status carries no token. Commits the (no-op) transaction.
    async fn replay_run(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        tenant: &str,
        idempotency_key: &str,
    ) -> Result<RunResponse> {
        // Lock the run row `FOR UPDATE` (consistent lock order: runs before run_checkpoints, as
        // in `resume`). Without it, `status` and the open-checkpoint token are two separate
        // READ COMMITTED reads: a concurrent `resume` committing between them could return
        // `status = 'waiting'` with a `null` token (the checkpoint just got consumed). The lock
        // blocks until any in-flight resume commits, so the pair is mutually consistent.
        let row: Option<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT run_id, status FROM runs WHERE tenant_id = $1 AND idempotency_key = $2 \
             FOR UPDATE",
        )
        .bind(tenant)
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?;
        // The row must exist — we just conflicted on it inside this same tx. A `None` is an
        // invariant violation (RLS/concurrent delete), not a client error → Internal, not a raw
        // `RowNotFound`.
        let (run_id, status) = row.ok_or_else(|| {
            Error::Internal(anyhow::anyhow!(
                "idempotency-claimed run not found on replay (tenant-scoped)"
            ))
        })?;
        let resume_token = if status == "waiting" {
            let tok: Option<(String,)> = sqlx::query_as(
                "SELECT token FROM run_checkpoints WHERE run_id = $1 AND status = 'open' \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
            tok.map(|(t,)| t)
        } else {
            None
        };
        tx.commit().await?;
        Ok(RunResponse {
            run_id: run_id.to_string(),
            status,
            resume_token,
        })
    }

    /// Advance the run within `tx`, then commit and — if it paused for a real tool execution —
    /// drive it to completion OUTSIDE the transaction (execute the connector, record, advance).
    async fn drive(
        &self,
        tenant: &str,
        run_id: uuid::Uuid,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<AdvanceOutcome> {
        let outcome = executor::advance(
            &mut tx,
            tenant,
            run_id,
            &self.connector,
            self.worker_enabled,
        )
        .await?;
        match outcome {
            // An auto tool step needs execution OUTSIDE the tx: commit the setup + the
            // `running` intent, then drive the run (execute the tool, record, advance).
            AdvanceOutcome::ExecuteTool(call) => {
                tx.commit().await?;
                executor::continue_run(
                    &self.db,
                    tenant,
                    run_id,
                    &self.connector,
                    call,
                    self.worker_enabled,
                )
                .await
            }
            // Completed / Waiting(token) / Failed / Scheduled (run stays `running`, the
            // worker drives the due retry): commit and surface the status.
            other => {
                tx.commit().await?;
                Ok(other)
            }
        }
    }

    /// Resume a suspended run using its opaque single-use `token`.
    ///
    /// Fails closed: the run is scoped to `tenant` by RLS, the checkpoint is
    /// consumed atomically (`FOR UPDATE` serializes concurrent resumes, so a
    /// token resumes at most once), the awaiting step is completed with
    /// `resume_input`, and the executor drives the run into the NEXT step — which
    /// may complete the run, suspend on a further gate (a new token), or fail.
    pub async fn resume(
        &self,
        tenant: &str,
        _principal_id: &str,
        req: &RunsResumeRequest,
    ) -> Result<RunResponse> {
        let run_id = uuid::Uuid::parse_str(req.run_id.trim())
            .map_err(|_| Error::BadRequest("run_id must be a valid UUID".into()))?;
        let token = req.token.trim();
        if token.is_empty() {
            return Err(Error::BadRequest("token is required".into()));
        }
        let resume_input = default_if_null(&req.resume_input);

        let mut tx = tenant_tx(&self.db, tenant).await?;

        // Lock the run row (RLS-scoped). Absent -> 404 (also hides other tenants'
        // runs, so no cross-tenant existence oracle).
        let run: Option<(String,)> =
            sqlx::query_as("SELECT status FROM runs WHERE run_id = $1 FOR UPDATE")
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((run_status,)) = run else {
            return Err(Error::NotFound("run not found".into()));
        };

        // Lock the checkpoint. A wrong/unknown token is indistinguishable from an
        // unknown run (both 404), so the token can't be probed.
        let checkpoint: Option<(uuid::Uuid, String, sqlx::types::Json<serde_json::Value>)> =
            sqlx::query_as(
                "SELECT id, status, state FROM run_checkpoints \
                 WHERE run_id = $1 AND token = $2 FOR UPDATE",
            )
            .bind(run_id)
            .bind(token)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((checkpoint_id, checkpoint_status, checkpoint_state)) = checkpoint else {
            return Err(Error::NotFound("run not found".into()));
        };

        // Single-use: a consumed/expired checkpoint is a conflict, not a 404.
        if checkpoint_status != "open" {
            return Err(Error::Conflict(format!(
                "checkpoint already {checkpoint_status}"
            )));
        }
        // Only a suspended run is resumable (a completed/failed run must not be
        // reanimated) — this is the 409 a follow-up resume of a terminal run gets.
        if run_status != "waiting" && run_status != "suspended" {
            return Err(Error::Conflict(format!(
                "run is {run_status}, not resumable"
            )));
        }

        // Consume the checkpoint. The FOR UPDATE lock serializes concurrent
        // resumers; the WHERE status='open' guard is belt-and-suspenders.
        let consumed = sqlx::query(
            "UPDATE run_checkpoints SET status = 'consumed', consumed_at = now() \
             WHERE id = $1 AND status = 'open'",
        )
        .bind(checkpoint_id)
        .execute(&mut *tx)
        .await?;
        if consumed.rows_affected() != 1 {
            return Err(Error::Conflict("checkpoint already consumed".into()));
        }

        append_event(&mut tx, tenant, run_id, "resumed", &resume_input).await?;

        let step_index = checkpoint_state
            .0
            .get("step_index")
            .and_then(serde_json::Value::as_i64)
            .map(|i| i as i32);

        // If the just-approved gate is a Tool step and a real connector is configured, EXECUTE
        // it for real via the drive-loop (the pending tool_execution from suspend time is the
        // ledger intent) instead of the placeholder finalize below.
        if let Some(idx) = step_index {
            if let Some(call) =
                executor::begin_gated_tool(&mut tx, tenant, run_id, idx, &self.connector).await?
            {
                tx.commit().await?;
                let outcome = executor::continue_run(
                    &self.db,
                    tenant,
                    run_id,
                    &self.connector,
                    call,
                    self.worker_enabled,
                )
                .await?;
                return Ok(RunResponse {
                    run_id: run_id.to_string(),
                    status: outcome.status().to_string(),
                    resume_token: outcome.resume_token(),
                });
            }
        }

        // Human-approval gate, OR a gated tool with no connector: complete the gated step (by
        // the step_index it recorded) with the resume input as its output, then let the executor
        // drive the next step. (Fallback to any waiting step for a checkpoint without a
        // step_index — defensive; PR1 always records it.)
        match step_index {
            Some(idx) => {
                sqlx::query(
                    "UPDATE run_steps SET status = 'completed', output = $2, attempts = 1 \
                     WHERE run_id = $1 AND step_index = $3 AND status = 'waiting'",
                )
                .bind(run_id)
                .bind(sqlx::types::Json(&resume_input))
                .bind(idx)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "UPDATE run_steps SET status = 'completed', output = $2, attempts = 1 \
                     WHERE run_id = $1 AND status = 'waiting'",
                )
                .bind(run_id)
                .bind(sqlx::types::Json(&resume_input))
                .execute(&mut *tx)
                .await?;
            }
        }

        // If the gate just cleared was a Tool step with NO connector configured, its pending
        // tool_execution is now approved -> finalize it (pending -> executed placeholder). At
        // most one tool_execution is 'pending' per run (advance suspends at the FIRST waiting
        // step), so this targets the right row and is a harmless no-op for a human_approval gate
        // (which has no pending tool). With a REAL connector the gated tool already executed for
        // real above via the drive-loop, so this path isn't reached for it.
        sqlx::query(
            "UPDATE tool_executions SET status = 'executed', result = COALESCE(result, $2) \
             WHERE run_id = $1 AND status = 'pending'",
        )
        .bind(run_id)
        .bind(sqlx::types::Json(&serde_json::json!({
            "note": "approved via run resume; gated-tool in-run execution deferred",
        })))
        .execute(&mut *tx)
        .await?;

        let outcome = self.drive(tenant, run_id, tx).await?;

        Ok(RunResponse {
            run_id: run_id.to_string(),
            status: outcome.status().to_string(),
            resume_token: outcome.resume_token(),
        })
    }
}

/// Substitute the column's `{}` default when the caller omits a jsonb field (it
/// deserializes to `null`). A non-null value is stored verbatim (the columns are
/// `jsonb`, so any well-formed JSON is accepted).
fn default_if_null(v: &serde_json::Value) -> serde_json::Value {
    if v.is_null() {
        serde_json::json!({})
    } else {
        v.clone()
    }
}
