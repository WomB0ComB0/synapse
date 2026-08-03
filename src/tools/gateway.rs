//! Policy-guarded tool/connector (MCP) gateway.

use std::sync::Arc;

use crate::api::idempotency::{self, IdempotencyClaim};
use crate::db::tenant_tx;
use crate::domain::{
    ApprovalMode, ToolDecision, ToolExecuteRequest, ToolExecuteResponse, ToolLifecycleResponse,
};
use crate::error::{Error, Result};
use crate::mcp::ConnectorImpl;
use crate::tools::registry;

type PendingExecutionRow = (
    String,
    sqlx::types::Json<serde_json::Value>,
    String,
    Option<sqlx::types::Json<serde_json::Value>>,
    Option<uuid::Uuid>,
);
type OriginalExecutionRow = (
    String,
    sqlx::types::Json<serde_json::Value>,
    Option<sqlx::types::Json<serde_json::Value>>,
    String,
    Option<String>,
);

/// Generic idempotency fingerprint gate (0024) for `tool.execute`: record/verify the request
/// fingerprint for a keyed call and reject a reuse-with-a-different-body with `409` — alongside the
/// per-execution idempotency (#33, `tool_executions.idempotency_key`), which this only adds the
/// body-mismatch check to. No-op when keyless. Runs in the caller's tx (atomic with the write).
async fn gate_tool_fingerprint(
    conn: &mut sqlx::PgConnection,
    tenant: &str,
    key: Option<&str>,
    req: &ToolExecuteRequest,
) -> Result<()> {
    if let Some(key) = key {
        let fp = idempotency::request_bytes(req);
        if idempotency::claim_idempotency_key(conn, tenant, "tool.execute", key, &fp).await?
            == IdempotencyClaim::Mismatch
        {
            return Err(Error::Conflict(
                "Idempotency-Key was already used for a different tool.execute request".into(),
            ));
        }
    }
    Ok(())
}

/// Mediates every tool call, tenant-isolated by RLS (migration 0009).
///
/// Enforces **human-in-the-loop approval before autonomous execution**: approval is
/// required if the caller declares `approval_mode = required` OR — server-side policy the
/// client cannot downgrade — a skill in this tenant that requires this tool is tagged
/// `human_approval_required`. When approval is required the call is persisted `pending` and
/// NOT executed. Every call is recorded in `tool_executions` (migration 0007).
///
/// When a real [`ConnectorImpl`] is configured (`MCP_ENDPOINT`), an auto-approved call is
/// actually EXECUTED against it (the external call runs OUTSIDE the DB transaction — the
/// same short-tx discipline as the embedder — and the real result recorded `executed`, a
/// connector fault recorded `failed`). Without a connector, execution is a placeholder.
pub struct ToolGateway {
    db: sqlx::PgPool,
    connector: Arc<ConnectorImpl>,
}

impl ToolGateway {
    /// Construct a gateway over the given pool + tool connector.
    pub fn new(db: sqlx::PgPool, connector: Arc<ConnectorImpl>) -> Self {
        Self { db, connector }
    }

    /// Execute a governed tool call for `tenant` on behalf of `principal_id` (the ad-hoc
    /// `/tool.execute` path, no owning run). Returns the outcome (`executed` / `pending`);
    /// a connector fault is recorded `failed` and surfaced as an upstream error (502).
    pub async fn execute(
        &self,
        tenant: &str,
        principal_id: &str,
        req: &ToolExecuteRequest,
        idempotency_key: Option<&str>,
    ) -> Result<ToolExecuteResponse> {
        let tool_id = req.tool_id.trim();
        if tool_id.is_empty() {
            return Err(Error::BadRequest("tool_id is required".into()));
        }

        // No real connector: keep the pre-connector behavior exactly — policy + placeholder
        // execution in one short tenant tx (the same path the in-run executor uses). Dedup rides
        // inside execute_in_tx (the placeholder is terminal, so a same-key repeat replays).
        if !self.connector.is_enabled() {
            let mut tx = tenant_tx(&self.db, tenant).await?;
            gate_tool_fingerprint(&mut tx, tenant, idempotency_key, req).await?;
            let resp = execute_in_tx(
                &mut tx,
                tenant,
                principal_id,
                req,
                None,
                None,
                idempotency_key,
            )
            .await?;
            tx.commit().await?;
            return Ok(resp);
        }

        let arguments = coerce_arguments(&req.arguments);

        // Phase 1 (short tx): provision the caller + decide approval. An approval-gated call is
        // persisted `pending` and NOT executed. An auto call persists a durable `approved`
        // INTENT row BEFORE the external call, so a tool that runs is NEVER lost from the ledger
        // even if the outcome recording later fails (the row stays `approved`, reconcilable) —
        // closing the lost-record / retry-double-execution hole. With a client `idempotency_key`,
        // the intent insert is `ON CONFLICT DO NOTHING`: a same-key repeat gets `None` and REPLAYS
        // the original outcome (never a second connector call). The committed unique-index claim
        // is the cross-request mutex over the out-of-tx window.
        let mut tx = tenant_tx(&self.db, tenant).await?;
        gate_tool_fingerprint(&mut tx, tenant, idempotency_key, req).await?;
        provision_principal(&mut tx, tenant, principal_id).await?;
        let runtime_policy =
            registry::govern_external_call(&mut tx, tenant, tool_id, &arguments, &self.connector)
                .await?;
        let requires = runtime_policy.requires_approval
            || requires_approval(&mut tx, tenant, tool_id, &req.policy.approval_mode).await?;
        if requires {
            match insert_execution(
                &mut tx,
                tenant,
                principal_id,
                tool_id,
                &arguments,
                &req.policy.reason,
                "required",
                "pending",
                None,
                Some(runtime_policy.revision),
                runtime_policy.rollback_tool_id.as_deref(),
                None,
                idempotency_key,
            )
            .await?
            {
                Some(exec_id) => {
                    tx.commit().await?;
                    return Ok(ToolExecuteResponse {
                        tool_id: tool_id.to_string(),
                        execution_id: Some(exec_id.to_string()),
                        status: "pending".to_string(),
                        output: serde_json::json!({}),
                        requires_approval: true,
                    });
                }
                None => {
                    let resp = replay_execution(&mut tx, tenant, idempotency_key).await?;
                    tx.commit().await?;
                    return Ok(resp);
                }
            }
        }
        let exec_id = match insert_execution(
            &mut tx,
            tenant,
            principal_id,
            tool_id,
            &arguments,
            &req.policy.reason,
            "none",
            "approved",
            None,
            Some(runtime_policy.revision),
            runtime_policy.rollback_tool_id.as_deref(),
            None,
            idempotency_key,
        )
        .await?
        {
            Some(id) => id,
            None => {
                let resp = replay_execution(&mut tx, tenant, idempotency_key).await?;
                tx.commit().await?;
                return Ok(resp);
            }
        };
        // Release the DB connection BEFORE the external call — never hold a tx across I/O.
        tx.commit().await?;

        // Stamp `started_at` (best-effort) right before the call so the reconciler can tell a
        // crash BEFORE dispatch (never attempted) from one during/after (maybe ran).
        mark_started(&self.db, tenant, exec_id).await;

        // Phase 2 (NO tx): execute the tool via the connector. The `exec_id` is this execution's
        // stable idempotency key for the two-sided MCP contract (a client-level retry with the same
        // `Idempotency-Key` replays via `replay_execution` above and never reaches here, so `exec_id`
        // is stable per logical execution; the connector's connect-phase re-send reuses it too).
        match self
            .connector
            .call(tool_id, &arguments, &exec_id.to_string())
            .await
        {
            Ok(result) => {
                // Phase 3 (best-effort): finalize the intent row to `executed`. On a recording
                // failure the row stays `approved` (reconcilable, NOT lost) and we STILL return
                // success — the tool ran, so a 500 here would wrongly induce a client retry (a
                // duplicate side effect).
                if let Err(e) = record_final(&self.db, tenant, exec_id, "executed", &result).await {
                    tracing::warn!(error = %e, tool = %tool_id, %exec_id, "tool executed but recording the result failed");
                }
                Ok(ToolExecuteResponse {
                    tool_id: tool_id.to_string(),
                    execution_id: Some(exec_id.to_string()),
                    status: "executed".to_string(),
                    output: result,
                    requires_approval: false,
                })
            }
            Err(e) => {
                // Finalize the intent row to `failed` (best-effort), then surface the error.
                if let Err(rec) = record_final(
                    &self.db,
                    tenant,
                    exec_id,
                    "failed",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await
                {
                    tracing::warn!(error = %rec, tool = %tool_id, %exec_id, "failed to record tool failure");
                }
                Err(e)
            }
        }
    }

    /// Approve or deny one standalone pending execution. Run-owned gates continue to use
    /// `runs.resume`, preserving the run checkpoint token and event state machine.
    pub async fn decide(
        &self,
        tenant: &str,
        approver: &str,
        execution_id: uuid::Uuid,
        decision: ToolDecision,
        reason: Option<&str>,
    ) -> Result<ToolLifecycleResponse> {
        let mut tx = tenant_tx(&self.db, tenant).await?;
        let row: Option<PendingExecutionRow> = sqlx::query_as(
            "SELECT tool_id, arguments, status, result, run_id FROM tool_executions \
             WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(tenant)
        .bind(execution_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (tool_id, arguments, status, result, run_id) =
            row.ok_or_else(|| Error::NotFound(format!("tool execution '{execution_id}'")))?;
        if run_id.is_some() {
            return Err(Error::BadRequest(
                "run-owned tool approvals must use runs.resume".into(),
            ));
        }

        match decision {
            ToolDecision::Deny => {
                if status == "denied" {
                    tx.commit().await?;
                    return Ok(ToolLifecycleResponse {
                        execution_id: execution_id.to_string(),
                        tool_id,
                        status,
                        output: result.map(|value| value.0).unwrap_or_default(),
                    });
                }
                if status != "pending" {
                    return Err(Error::Conflict(format!(
                        "cannot deny a tool execution in status {status:?}"
                    )));
                }
                sqlx::query(
                    "UPDATE tool_executions SET status = 'denied', decided_by = $1, \
                     decision_reason = $2, decided_at = now(), finished_at = now(), \
                     result = '{}'::jsonb WHERE id = $3 AND status = 'pending'",
                )
                .bind(approver)
                .bind(reason.map(str::trim).filter(|value| !value.is_empty()))
                .bind(execution_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(ToolLifecycleResponse {
                    execution_id: execution_id.to_string(),
                    tool_id,
                    status: "denied".into(),
                    output: serde_json::json!({}),
                });
            }
            ToolDecision::Approve => {
                if status == "executed" {
                    tx.commit().await?;
                    return Ok(ToolLifecycleResponse {
                        execution_id: execution_id.to_string(),
                        tool_id,
                        status,
                        output: result.map(|value| value.0).unwrap_or_default(),
                    });
                }
                if status != "pending" {
                    return Err(Error::Conflict(format!(
                        "cannot approve a tool execution in status {status:?}"
                    )));
                }
            }
        }

        let policy = registry::govern_external_call(
            &mut tx,
            tenant,
            &tool_id,
            &arguments.0,
            &self.connector,
        )
        .await?;
        sqlx::query(
            "UPDATE tool_executions SET status = 'approved', decided_by = $1, \
             decision_reason = $2, decided_at = now(), definition_revision = $3, \
             rollback_tool_id = $4 WHERE id = $5 AND status = 'pending'",
        )
        .bind(approver)
        .bind(reason.map(str::trim).filter(|value| !value.is_empty()))
        .bind(policy.revision)
        .bind(policy.rollback_tool_id.as_deref())
        .bind(execution_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        mark_started(&self.db, tenant, execution_id).await;
        match self
            .connector
            .call(&tool_id, &arguments.0, &execution_id.to_string())
            .await
        {
            Ok(output) => {
                record_final(&self.db, tenant, execution_id, "executed", &output).await?;
                Ok(ToolLifecycleResponse {
                    execution_id: execution_id.to_string(),
                    tool_id,
                    status: "executed".into(),
                    output,
                })
            }
            Err(error) => {
                let stored = serde_json::json!({ "error": error.to_string() });
                if let Err(record_error) =
                    record_final(&self.db, tenant, execution_id, "failed", &stored).await
                {
                    tracing::warn!(error = %record_error, %execution_id, "failed to record approved tool failure");
                }
                Err(error)
            }
        }
    }

    /// Execute the registered compensation tool exactly once for a completed execution.
    pub async fn rollback(
        &self,
        tenant: &str,
        approver: &str,
        original_id: uuid::Uuid,
        reason: Option<&str>,
    ) -> Result<ToolLifecycleResponse> {
        let mut tx = tenant_tx(&self.db, tenant).await?;
        provision_principal(&mut tx, tenant, approver).await?;
        let original: Option<OriginalExecutionRow> = sqlx::query_as(
            "SELECT tool_id, arguments, result, status, rollback_tool_id FROM tool_executions \
             WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(tenant)
        .bind(original_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (original_tool, original_arguments, original_result, original_status, rollback_tool) =
            original.ok_or_else(|| Error::NotFound(format!("tool execution '{original_id}'")))?;
        if original_status != "executed" {
            return Err(Error::Conflict(
                "only an executed tool call can be rolled back".into(),
            ));
        }

        let existing: Option<(
            uuid::Uuid,
            String,
            String,
            Option<sqlx::types::Json<serde_json::Value>>,
        )> = sqlx::query_as(
            "SELECT id, tool_id, status, result FROM tool_executions \
             WHERE tenant_id = $1 AND rollback_of = $2 FOR UPDATE",
        )
        .bind(tenant)
        .bind(original_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((id, tool_id, status, result)) = existing {
            tx.commit().await?;
            if status == "approved" {
                return Err(Error::Conflict(
                    "rollback execution is already in progress".into(),
                ));
            }
            return Ok(ToolLifecycleResponse {
                execution_id: id.to_string(),
                tool_id,
                status,
                output: result.map(|value| value.0).unwrap_or_default(),
            });
        }

        let rollback_tool = rollback_tool
            .ok_or_else(|| Error::Conflict("tool has no snapshotted rollback handler".into()))?;
        let rollback_arguments = serde_json::json!({
            "original_execution_id": original_id,
            "original_tool_id": original_tool,
            "original_arguments": original_arguments.0,
            "original_result": original_result.map(|value| value.0).unwrap_or_default(),
            "reason": reason.map(str::trim).filter(|value| !value.is_empty()),
        });
        let policy = registry::govern_external_call(
            &mut tx,
            tenant,
            &rollback_tool,
            &rollback_arguments,
            &self.connector,
        )
        .await?;
        let rollback_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO tool_executions \
                 (id, tenant_id, principal_id, tool_id, arguments, approval_mode, status, \
                  definition_revision, rollback_of, decided_by, decision_reason, decided_at) \
             VALUES ($1, $2, $3, $4, $5, 'none', 'approved', $6, $7, $3, $8, now())",
        )
        .bind(rollback_id)
        .bind(tenant)
        .bind(approver)
        .bind(&rollback_tool)
        .bind(sqlx::types::Json(&rollback_arguments))
        .bind(policy.revision)
        .bind(original_id)
        .bind(reason.map(str::trim).filter(|value| !value.is_empty()))
        .execute(&mut *tx)
        .await
        .map_err(|error| Error::db_or_conflict(error, "tool execution was already rolled back"))?;
        tx.commit().await?;

        mark_started(&self.db, tenant, rollback_id).await;
        match self
            .connector
            .call(
                &rollback_tool,
                &rollback_arguments,
                &rollback_id.to_string(),
            )
            .await
        {
            Ok(output) => {
                record_final(&self.db, tenant, rollback_id, "executed", &output).await?;
                Ok(ToolLifecycleResponse {
                    execution_id: rollback_id.to_string(),
                    tool_id: rollback_tool,
                    status: "executed".into(),
                    output,
                })
            }
            Err(error) => {
                let stored = serde_json::json!({ "error": error.to_string() });
                if let Err(record_error) =
                    record_final(&self.db, tenant, rollback_id, "failed", &stored).await
                {
                    tracing::warn!(error = %record_error, %rollback_id, "failed to record rollback failure");
                }
                Err(error)
            }
        }
    }
}

/// Finalize a tool-execution intent row (`approved`) to its terminal `status` + `result`, in
/// its own short tenant transaction. Used for the connector's out-of-tx outcome.
async fn record_final(
    db: &sqlx::PgPool,
    tenant: &str,
    id: uuid::Uuid,
    status: &str,
    result: &serde_json::Value,
) -> Result<()> {
    let mut tx = tenant_tx(db, tenant).await?;
    update_execution(&mut tx, id, status, result).await?;
    tx.commit().await?;
    Ok(())
}

/// REPLAY the outcome of a tool call already claimed by this idempotency key (a same-key repeat),
/// WITHOUT re-invoking the non-idempotent connector. Reads the row `FOR UPDATE` (serialize against
/// an in-flight finalize so `status` is a consistent terminal snapshot) and reconstructs the
/// response by status: `executed` → the stored result (200); `pending`/`denied` → that terminal
/// outcome (200); `failed` → re-raise a 502 (never re-run — the failure may be post-attempt);
/// `approved` → still in-flight, so **409** (the outcome is unknown and the tool is non-idempotent,
/// so we neither re-call nor fabricate success — a stuck one is cleared by the background reconciler).
async fn replay_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    idempotency_key: Option<&str>,
) -> Result<ToolExecuteResponse> {
    let row: Option<(
        uuid::Uuid,
        String,
        String,
        Option<sqlx::types::Json<serde_json::Value>>,
    )> = sqlx::query_as(
        "SELECT id, tool_id, status, result FROM tool_executions \
             WHERE tenant_id = $1 AND idempotency_key = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    // The row must exist — we just conflicted on it inside this same tx. A `None` is an invariant
    // violation, not a client error → Internal, not a raw `RowNotFound`.
    let (execution_id, tool_id, status, result) = row.ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "idempotency-claimed tool execution not found on replay (tenant-scoped)"
        ))
    })?;
    let output = result.map(|j| j.0).unwrap_or_else(|| serde_json::json!({}));
    match status.as_str() {
        "executed" => Ok(ToolExecuteResponse {
            tool_id,
            execution_id: Some(execution_id.to_string()),
            status: "executed".to_string(),
            output,
            requires_approval: false,
        }),
        "pending" => Ok(ToolExecuteResponse {
            tool_id,
            execution_id: Some(execution_id.to_string()),
            status: "pending".to_string(),
            output: serde_json::json!({}),
            requires_approval: true,
        }),
        "denied" => Ok(ToolExecuteResponse {
            tool_id,
            execution_id: Some(execution_id.to_string()),
            status: "denied".to_string(),
            output: serde_json::json!({}),
            requires_approval: false,
        }),
        "failed" => {
            // The typed error variant was not persisted (only `{"error": msg}`) — re-raise a
            // generic upstream fault (502), the same class the original returned. Never re-execute.
            let msg = output
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("tool execution failed")
                .to_string();
            Err(Error::Upstream(msg))
        }
        "approved" => Err(Error::Conflict(
            "a tool execution for this idempotency key is already in progress".to_string(),
        )),
        other => Err(Error::Internal(anyhow::anyhow!(
            "unexpected tool_executions status on idempotent replay: {other}"
        ))),
    }
}

/// Best-effort stamp `started_at` on the `approved` intent right before the out-of-tx connector
/// call. A failure is non-fatal — the call still proceeds; the reconciler then treats the row as
/// maybe-ran (the fail-safe default) rather than provably-never-attempted.
async fn mark_started(db: &sqlx::PgPool, tenant: &str, id: uuid::Uuid) {
    let stamp = async {
        let mut tx = tenant_tx(db, tenant).await?;
        sqlx::query(
            "UPDATE tool_executions SET started_at = now() WHERE id = $1 AND started_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<(), Error>(())
    };
    if let Err(e) = stamp.await {
        tracing::warn!(error = %e, %id, "failed to stamp tool execution started_at");
    }
}

/// Update a `tool_executions` row's terminal status + result. Keyed by the globally-unique
/// `id` (UUID PK) under RLS in the caller's tenant transaction.
pub(crate) async fn update_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: uuid::Uuid,
    status: &str,
    result: &serde_json::Value,
) -> Result<()> {
    // Stamp `finished_at` on finalize: a non-NULL `finished_at` is the positive marker of a
    // terminal row, and (paired with `started_at`) lets the reconciler tell a finalized row from
    // a crash-stranded `approved` one.
    sqlx::query(
        "UPDATE tool_executions SET status = $1, result = $2, finished_at = now() WHERE id = $3",
    )
    .bind(status)
    .bind(sqlx::types::Json(result))
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The core governed-tool-call logic on a CALLER-PROVIDED transaction — the run executor's
/// Tool step (atomic with the run's step/checkpoint/event writes), and the ad-hoc path
/// when no connector is configured. `run_id` links the `tool_executions` row to that run.
///
/// Execution is a PLACEHOLDER here: a real in-run connector call must run OUTSIDE the run's
/// transaction (network I/O can't be held atomic with the DB writes), so real in-run tool
/// execution is deferred to the async/leased-worker executor (retry/backoff/timers, PR4).
/// Approval-gated calls are persisted `pending` and NOT executed, exactly as before.
pub async fn execute_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    principal_id: &str,
    req: &ToolExecuteRequest,
    runtime_policy: Option<&registry::RuntimeToolPolicy>,
    run_id: Option<uuid::Uuid>,
    idempotency_key: Option<&str>,
) -> Result<ToolExecuteResponse> {
    let tool_id = req.tool_id.trim();
    if tool_id.is_empty() {
        return Err(Error::BadRequest("tool_id is required".into()));
    }
    provision_principal(tx, tenant, principal_id).await?;
    let requires = requires_approval(tx, tenant, tool_id, &req.policy.approval_mode).await?;
    let arguments = coerce_arguments(&req.arguments);
    // Approval-gated calls are persisted pending and NOT executed. Auto calls record a
    // placeholder result (no in-run connector wired — see the fn doc / PR4).
    let (status, approval_mode, result) = if requires {
        ("pending", "required", None)
    } else {
        (
            "executed",
            "none",
            Some(serde_json::json!({
                "note": "no MCP connector configured; call recorded as executed",
                "tool_id": tool_id,
            })),
        )
    };
    // The placeholder is terminal in this one tx (no out-of-tx window), so a same-key repeat
    // simply REPLAYS the stored outcome instead of inserting a duplicate.
    match insert_execution(
        tx,
        tenant,
        principal_id,
        tool_id,
        &arguments,
        &req.policy.reason,
        approval_mode,
        status,
        result.as_ref(),
        runtime_policy.map(|policy| policy.revision),
        runtime_policy.and_then(|policy| policy.rollback_tool_id.as_deref()),
        run_id,
        idempotency_key,
    )
    .await?
    {
        Some(exec_id) => Ok(ToolExecuteResponse {
            tool_id: tool_id.to_string(),
            execution_id: Some(exec_id.to_string()),
            status: status.to_string(),
            output: result.unwrap_or_else(|| serde_json::json!({})),
            requires_approval: requires,
        }),
        None => replay_execution(tx, tenant, idempotency_key).await,
    }
}

/// Coerce an omitted (JSON null) arguments to `{}` so stored rows match the column default.
fn coerce_arguments(arguments: &serde_json::Value) -> serde_json::Value {
    if arguments.is_null() {
        serde_json::json!({})
    } else {
        arguments.clone()
    }
}

/// Ensure the acting principal exists (tenant-scoped) so the `tool_executions` FK to
/// `principals` holds. An unknown tenant (FK 23503) is a 400, not a 500.
async fn provision_principal(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    principal_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO principals (principal_id, tenant_id) VALUES ($1, $2) \
         ON CONFLICT (tenant_id, principal_id) DO NOTHING",
    )
    .bind(principal_id)
    .bind(tenant)
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::db_or_conflict(e, "principal already exists"))?;
    Ok(())
}

/// Whether the tool call requires human approval: the client declared it, OR — server-side
/// policy the client cannot downgrade — the tenant's CURRENT (`is_latest`) skill requiring
/// this tool tags it `human_approval_required`. Scoping to `is_latest` keeps the latest
/// version authoritative so policy can EVOLVE (a newer version may add or drop the
/// requirement); the client can raise the bar (`approval_mode`) but never lower it.
pub(crate) async fn requires_approval(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    tool_id: &str,
    approval_mode: &ApprovalMode,
) -> Result<bool> {
    let server_requires: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM skills \
         WHERE tenant_id = $1 AND is_latest AND $2 = ANY(required_tools) \
           AND 'human_approval_required' = ANY(policy_tags) \
         LIMIT 1",
    )
    .bind(tenant)
    .bind(tool_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(server_requires.is_some() || matches!(approval_mode, ApprovalMode::Required))
}

/// Insert the pre-call `approved` INTENT row for an in-RUN tool call — a reconcilable ledger
/// record written BEFORE the external side effect (mirroring the ad-hoc saga's pre-call intent
/// row), returning its id so the executor can finalize it to `executed`/`failed` via
/// [`update_execution`] after the connector call. So even if that finalize fails, an executed
/// external write is never absent from the `tool_executions` ledger.
pub(crate) async fn insert_run_tool_intent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    principal_id: &str,
    tool_id: &str,
    arguments: &serde_json::Value,
    runtime_policy: &registry::RuntimeToolPolicy,
    run_id: uuid::Uuid,
) -> Result<uuid::Uuid> {
    // In-run intents carry NO client idempotency key (a keyless insert never conflicts on the
    // partial index, so it always returns a fresh id) — tool-step dedup is a separate future item.
    insert_execution(
        tx,
        tenant,
        principal_id,
        tool_id,
        arguments,
        &None,
        "none",
        "approved",
        None,
        Some(runtime_policy.revision),
        runtime_policy.rollback_tool_id.as_deref(),
        Some(run_id),
        None,
    )
    .await?
    .ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "keyless run-tool intent insert returned no id"
        ))
    })
}

/// Insert one `tool_executions` row, returning its generated `id`. With a client
/// `idempotency_key`, `ON CONFLICT DO NOTHING` makes a repeat a no-op — `None` is returned so
/// the caller can REPLAY the original outcome instead of re-executing (a NULL key never
/// conflicts on the partial index, so keyless callers always get `Some`).
#[allow(clippy::too_many_arguments)]
async fn insert_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    principal_id: &str,
    tool_id: &str,
    arguments: &serde_json::Value,
    approval_reason: &Option<String>,
    approval_mode: &str,
    status: &str,
    result: Option<&serde_json::Value>,
    definition_revision: Option<i64>,
    rollback_tool_id: Option<&str>,
    run_id: Option<uuid::Uuid>,
    idempotency_key: Option<&str>,
) -> Result<Option<uuid::Uuid>> {
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "INSERT INTO tool_executions \
            (tenant_id, principal_id, tool_id, arguments, approval_mode, approval_reason, status, result, definition_revision, rollback_tool_id, run_id, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         ON CONFLICT (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL \
         DO NOTHING \
         RETURNING id",
    )
    .bind(tenant)
    .bind(principal_id)
    .bind(tool_id)
    .bind(sqlx::types::Json(arguments))
    .bind(approval_mode)
    .bind(approval_reason)
    .bind(status)
    .bind(result.map(sqlx::types::Json))
    .bind(definition_revision)
    .bind(rollback_tool_id)
    .bind(run_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(id,)| id))
}
