//! The run step executor: advances a run through its materialized `run_steps`.

use crate::audit;
use crate::auth::policy::{authorize_in_tx, Action};
use crate::db::tenant_tx;
use crate::domain::{ApprovalMode, ToolExecuteRequest, ToolPolicy};
use crate::error::{Error, Result};
use crate::mcp::ConnectorImpl;
use crate::orchestration::workflow::ToolStep;
use crate::tools::gateway;

/// The leading non-terminal step row `advance` fetches:
/// `(id, step_index, name, kind, status, config, attempts, max_attempts, retry_not_due, armed)`.
/// `armed` = `next_attempt_at IS NOT NULL` (a `wait` timer already armed / a retry scheduled).
type LeadingStep = (
    uuid::Uuid,
    i32,
    String,
    String,
    String,
    sqlx::types::Json<serde_json::Value>,
    i32,
    i32,
    bool,
    bool,
);

/// An auto-approved tool call the executor has paused the run on (`run_steps.status =
/// 'running'`, a durable intent) so the caller can execute it OUTSIDE the run transaction
/// (network I/O can't be held atomic with the DB) and then record the outcome via
/// [`continue_run`]. Carries everything needed to invoke + record the call.
pub struct RunToolCall {
    pub step_id: uuid::Uuid,
    pub step_index: i32,
    pub name: String,
    pub tool_id: String,
    pub arguments: serde_json::Value,
    pub principal: String,
    /// The pre-call `approved` `tool_executions` intent row to finalize after the call.
    pub exec_id: uuid::Uuid,
}

/// Outcome of driving a run forward one or more steps.
pub enum AdvanceOutcome {
    /// All steps done — the run is `completed`.
    Completed,
    /// Hit a human-approval gate — the run is `waiting`; carries the resume token.
    Waiting(String),
    /// A step failed terminally — the run is `failed`.
    Failed,
    /// An auto tool step needs REAL execution against the connector, which cannot run inside
    /// the run's transaction — the caller executes it (out of tx) and calls [`continue_run`].
    ExecuteTool(RunToolCall),
    /// A step was DEFERRED to a future time via `run_steps.next_attempt_at` — a RETRIABLE failure
    /// rescheduled with a backoff (0017), or a `wait` timer step armed to a wake time (0020). The
    /// run stays `running` and is driven again by the background worker once `next_attempt_at`
    /// elapses (deferral is a worker capability — see `worker_enabled`).
    Scheduled,
}

impl AdvanceOutcome {
    /// The `runs.status` string this outcome corresponds to.
    pub fn status(&self) -> &'static str {
        match self {
            AdvanceOutcome::Completed => "completed",
            AdvanceOutcome::Waiting(_) => "waiting",
            AdvanceOutcome::Failed => "failed",
            // Transient — consumed by the drive-loop, never surfaced as a final run status.
            AdvanceOutcome::ExecuteTool(_) => "running",
            // The run is progressing in the background (a retry or timer is pending) — `running`.
            AdvanceOutcome::Scheduled => "running",
        }
    }

    /// The resume token, if the run suspended on an approval gate.
    pub fn resume_token(self) -> Option<String> {
        match self {
            AdvanceOutcome::Waiting(token) => Some(token),
            _ => None,
        }
    }
}

/// Advance `run_id` through as many steps as possible within `tx`.
///
/// Locks the run row `FOR UPDATE` (serializing against a concurrent resume) and
/// short-circuits an already-terminal run (idempotent). Then drives pending steps in
/// `step_index` order: a `transform` completes inline; a `human_approval` step suspends
/// `waiting` on a fresh single-use checkpoint; a `fail` step fails the run. A `tool` step:
/// with NO connector configured, the gateway placeholder runs it in-tx (auto → complete,
/// approval-required → suspend); with a real connector, an approval-required tool suspends,
/// while an AUTO tool is marked `running` and yielded as [`AdvanceOutcome::ExecuteTool`] so
/// the caller can execute it OUTSIDE the tx (see [`continue_run`]). Every transition appends
/// an ordered `run_events` row.
///
/// `worker_enabled` gates WORKER-DRIVEN DEFERRALS — retry/backoff (0017) and `wait` timers
/// (0020). When true, a RETRIABLE failure with attempts left is rescheduled, and a `wait` step
/// is armed to its wake time, both yielding [`AdvanceOutcome::Scheduled`]; the background worker
/// drives them once `next_attempt_at` elapses. When false (no worker), a retriable failure is
/// terminal and a `wait` completes immediately (the delay is skipped) — the pre-worker behavior,
/// so a deferral never strands a run. A leading step whose `next_attempt_at` has NOT elapsed
/// yields `Scheduled` without executing (self-guard: never run a step before its wake time).
pub async fn advance(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    connector: &ConnectorImpl,
    worker_enabled: bool,
) -> Result<AdvanceOutcome> {
    // principal_id is NULLABLE — the runs FK is `ON DELETE SET NULL (principal_id)`, so a run
    // can outlive its principal (offboarding). Only the tool branch requires a principal.
    let (run_status, run_principal): (String, Option<String>) =
        sqlx::query_as("SELECT status, principal_id FROM runs WHERE run_id = $1 FOR UPDATE")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await?;
    match run_status.as_str() {
        "completed" => return Ok(AdvanceOutcome::Completed),
        "failed" => return Ok(AdvanceOutcome::Failed),
        _ => {}
    }

    loop {
        // The first NON-terminal step by index. Stopping at a leading gate (instead of
        // skipping to the next `pending` step) means an approval gate can never be stepped past.
        let step: Option<LeadingStep> = sqlx::query_as(
            "SELECT id, step_index, name, kind, status, config, attempts, max_attempts, \
                    (next_attempt_at IS NOT NULL AND next_attempt_at > now()) AS retry_not_due, \
                    (next_attempt_at IS NOT NULL) AS armed \
             FROM run_steps \
             WHERE run_id = $1 AND status NOT IN ('completed', 'skipped') \
             ORDER BY step_index LIMIT 1",
        )
        .bind(run_id)
        .fetch_optional(&mut **tx)
        .await?;

        let Some((
            step_id,
            step_index,
            name,
            kind,
            status,
            config,
            attempts,
            max_attempts,
            retry_not_due,
            armed,
        )) = step
        else {
            // No non-terminal steps remain -> the run is complete.
            let (steps_completed,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM run_steps WHERE run_id = $1 AND status = 'completed'",
            )
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await?;
            let output =
                serde_json::json!({ "note": "run completed", "steps_completed": steps_completed });
            sqlx::query("UPDATE runs SET status = 'completed', output = $2 WHERE run_id = $1")
                .bind(run_id)
                .bind(sqlx::types::Json(&output))
                .execute(&mut **tx)
                .await?;
            append_event(tx, tenant, run_id, "completed", &output).await?;
            return Ok(AdvanceOutcome::Completed);
        };

        // A leading `pending` step whose retry backoff has NOT elapsed is not ready — leave it
        // for the worker to pick up once due (self-guard: never run a step before next_attempt_at).
        if status == "pending" && retry_not_due {
            return Ok(AdvanceOutcome::Scheduled);
        }

        // A leading non-`pending` step is a gate we must not step past.
        if status != "pending" {
            return match status.as_str() {
                "waiting" => {
                    let (token,): (String,) = sqlx::query_as(
                        "SELECT token FROM run_checkpoints WHERE run_id = $1 AND status = 'open' \
                         ORDER BY created_at DESC LIMIT 1",
                    )
                    .bind(run_id)
                    .fetch_one(&mut **tx)
                    .await?;
                    Ok(AdvanceOutcome::Waiting(token))
                }
                // A leading `running` step is an in-flight tool execution whose outcome was
                // never recorded — only reachable if a process crashed mid-drive (PR4's
                // drive-loop always records BEFORE re-advancing). Surface it loudly rather
                // than silently mis-failing the run; reconciliation is a later PR.
                "running" => Err(Error::Internal(anyhow::anyhow!(
                    "run step {step_index} is 'running' (an in-flight tool execution was not recorded)"
                ))),
                _ => Ok(AdvanceOutcome::Failed),
            };
        }

        match kind.as_str() {
            "transform" => {
                let this_attempt = attempts + 1;
                // Test-only flaky behavior: a transform configured with `fail_until_attempt`
                // fails RETRIABLY (idempotent — no external side effect) until that attempt.
                let fail_until = config
                    .0
                    .get("fail_until_attempt")
                    .and_then(serde_json::Value::as_i64)
                    .map(|v| v as i32);
                if let Some(k) = fail_until {
                    if this_attempt < k {
                        let err = format!("flaky step '{name}' failed on attempt {this_attempt}");
                        return match schedule_retry_or_fail(
                            tx,
                            tenant,
                            run_id,
                            step_id,
                            step_index,
                            &name,
                            attempts,
                            max_attempts,
                            &err,
                            worker_enabled,
                        )
                        .await?
                        {
                            RetryDecision::Rescheduled => Ok(AdvanceOutcome::Scheduled),
                            RetryDecision::Terminal => Ok(AdvanceOutcome::Failed),
                        };
                    }
                }
                let output =
                    serde_json::json!({ "note": "transform step completed", "step": name });
                sqlx::query(
                    "UPDATE run_steps SET status = 'completed', output = $2, attempts = $3, \
                     next_attempt_at = NULL WHERE id = $1",
                )
                .bind(step_id)
                .bind(sqlx::types::Json(&output))
                .bind(this_attempt)
                .execute(&mut **tx)
                .await?;
                append_event(
                    tx,
                    tenant,
                    run_id,
                    "step_completed",
                    &serde_json::json!({ "step_index": step_index, "name": name }),
                )
                .await?;
                // fall through to the next pending step
            }
            "human_approval" => {
                let token = suspend_on_gate(
                    tx,
                    tenant,
                    run_id,
                    step_id,
                    &serde_json::json!({ "step_index": step_index, "name": name }),
                    &serde_json::json!({ "reason": "human_approval", "step_index": step_index, "name": name }),
                )
                .await?;
                return Ok(AdvanceOutcome::Waiting(token));
            }
            "wait" => {
                // A TIMER step (0020): reuse `next_attempt_at` as the wake time. `armed` means a
                // wake time is already set; the self-guard above already returned `Scheduled` if
                // it is still in the FUTURE, so reaching here with `armed` means it has ELAPSED.
                if armed {
                    let output = serde_json::json!({ "note": "wait elapsed", "step": name });
                    sqlx::query(
                        "UPDATE run_steps SET status = 'completed', output = $2, attempts = 1, \
                         next_attempt_at = NULL WHERE id = $1",
                    )
                    .bind(step_id)
                    .bind(sqlx::types::Json(&output))
                    .execute(&mut **tx)
                    .await?;
                    append_event(
                        tx,
                        tenant,
                        run_id,
                        "step_completed",
                        &serde_json::json!({ "step_index": step_index, "name": name, "kind": "wait" }),
                    )
                    .await?;
                    // fall through to the next step
                } else if let Some(wait_until) = config.0.get("wait_until").and_then(|v| v.as_str())
                {
                    // ABSOLUTE-time timer: arm `next_attempt_at` to the FIXED wall-clock instant
                    // (`config.wait_until`, an RFC3339 UTC stamp canonicalized at materialization),
                    // not `now() + offset`. Only defer when the worker is on AND the instant is
                    // still in the FUTURE per the DB clock (the single time source); an already-
                    // elapsed deadline or a disabled worker completes inline so a deferral never
                    // strands the run (mirrors the relative branch + the retry gate).
                    let deadline = chrono::DateTime::parse_from_rfc3339(wait_until)
                        .map_err(|e| {
                            Error::Internal(anyhow::anyhow!(
                                "run_steps.config.wait_until is not valid RFC3339: {e}"
                            ))
                        })?
                        .with_timezone(&chrono::Utc);
                    if worker_enabled {
                        // Arm ONLY if the deadline is in the future (DB clock); a past deadline
                        // updates 0 rows and falls through to inline completion.
                        let armed = sqlx::query(
                            "UPDATE run_steps SET next_attempt_at = $2 WHERE id = $1 AND $2 > now()",
                        )
                        .bind(step_id)
                        .bind(deadline)
                        .execute(&mut **tx)
                        .await?
                        .rows_affected()
                            == 1;
                        if armed {
                            sqlx::query("UPDATE runs SET status = 'running' WHERE run_id = $1")
                                .bind(run_id)
                                .execute(&mut **tx)
                                .await?;
                            append_event(
                                tx,
                                tenant,
                                run_id,
                                "step_scheduled",
                                &serde_json::json!({ "step_index": step_index, "name": name, "wait_until": wait_until }),
                            )
                            .await?;
                            return Ok(AdvanceOutcome::Scheduled);
                        }
                    }
                    // Worker off OR deadline already elapsed — complete inline with an accurate note.
                    let note = if !worker_enabled {
                        "wait skipped (no worker to fire the timer)"
                    } else {
                        "wait_until already elapsed"
                    };
                    let output = serde_json::json!({ "note": note, "step": name });
                    sqlx::query(
                        "UPDATE run_steps SET status = 'completed', output = $2, attempts = 1 \
                         WHERE id = $1",
                    )
                    .bind(step_id)
                    .bind(sqlx::types::Json(&output))
                    .execute(&mut **tx)
                    .await?;
                    append_event(
                        tx,
                        tenant,
                        run_id,
                        "step_completed",
                        &serde_json::json!({ "step_index": step_index, "name": name, "kind": "wait", "wait_skipped": true }),
                    )
                    .await?;
                    // fall through to the next step
                } else {
                    // Fresh timer: `wait_secs` is the delay. ARM `next_attempt_at` and defer to the
                    // background worker. With no worker to fire it (or a non-positive delay), SKIP
                    // the delay and complete immediately so a deferral never strands the run
                    // (mirrors the retry gate).
                    // Clamp to [0, i32::MAX] (~68 years): the floor keeps `now() + interval` in the
                    // future, the ceiling keeps `make_interval` well within Postgres's interval /
                    // timestamptz range so an absurd delay can't overflow into a DB runtime error.
                    let wait_secs = config
                        .0
                        .get("wait_secs")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0)
                        .clamp(0, i32::MAX as i64);
                    if worker_enabled && wait_secs > 0 {
                        sqlx::query(
                            "UPDATE run_steps SET next_attempt_at = now() + make_interval(secs => $2) \
                             WHERE id = $1",
                        )
                        .bind(step_id)
                        .bind(wait_secs)
                        .execute(&mut **tx)
                        .await?;
                        // Reflect the deferral on the run: it may have been `waiting` before a
                        // resume-driven wait step, so mirror `running` in the DB (idempotent for a
                        // start — already `running`) — else worker discovery (status='running')
                        // would never surface the armed timer. Mirrors the auto-tool branch.
                        sqlx::query("UPDATE runs SET status = 'running' WHERE run_id = $1")
                            .bind(run_id)
                            .execute(&mut **tx)
                            .await?;
                        append_event(
                            tx,
                            tenant,
                            run_id,
                            "step_scheduled",
                            &serde_json::json!({ "step_index": step_index, "name": name, "wait_secs": wait_secs }),
                        )
                        .await?;
                        return Ok(AdvanceOutcome::Scheduled);
                    }
                    // Reached only when the worker is off OR the delay is non-positive — record
                    // which, so the audit note isn't misleading.
                    let note = if !worker_enabled {
                        "wait skipped (no worker to fire the timer)"
                    } else {
                        "wait skipped (non-positive delay)"
                    };
                    let output = serde_json::json!({ "note": note, "step": name });
                    sqlx::query(
                        "UPDATE run_steps SET status = 'completed', output = $2, attempts = 1 \
                         WHERE id = $1",
                    )
                    .bind(step_id)
                    .bind(sqlx::types::Json(&output))
                    .execute(&mut **tx)
                    .await?;
                    append_event(
                        tx,
                        tenant,
                        run_id,
                        "step_completed",
                        &serde_json::json!({ "step_index": step_index, "name": name, "kind": "wait", "wait_skipped": true }),
                    )
                    .await?;
                    // fall through to the next step
                }
            }
            "tool" => {
                let cfg: ToolStep = serde_json::from_value(config.0).map_err(|e| {
                    Error::Internal(anyhow::anyhow!("invalid tool step config: {e}"))
                })?;
                let tool_id = cfg.tool_id.clone();
                // A run whose principal was deleted (FK ON DELETE SET NULL) cannot attribute or
                // authorize a tool call — fail closed, don't run it anonymously.
                let principal = run_principal.as_deref().ok_or_else(|| {
                    Error::Internal(anyhow::anyhow!(
                        "run has no principal; cannot run a governed tool step"
                    ))
                })?;
                // Separation of duties: a role authorized to START a run must NOT use it to
                // invoke a tool that same role is DENIED at /tool.execute. Re-check on THIS tx
                // (DB-authoritative role; a deny rolls the whole run tx back).
                authorize_in_tx(
                    tx,
                    principal,
                    None,
                    false,
                    tenant,
                    Action::ToolExecute,
                    &tool_id,
                )
                .await?;

                // Every real outbound call is governed by the tenant registry before it can
                // become an intent. This validates arguments and connector scopes in the same short
                // transaction as the run transition; network I/O still happens only after commit.
                let runtime_policy = if connector.is_enabled() {
                    Some(
                        crate::tools::registry::govern_external_call(
                            tx,
                            tenant,
                            &tool_id,
                            &cfg.arguments,
                            connector,
                        )
                        .await?,
                    )
                } else {
                    None
                };
                let requires_approval =
                    gateway::requires_approval(tx, tenant, &tool_id, &cfg.approval_mode).await?
                        || runtime_policy
                            .as_ref()
                            .is_some_and(|policy| policy.requires_approval);

                // With a real connector, an AUTO tool must execute OUTSIDE this tx. If policy does
                // not require approval, mark `running` + yield to the caller.
                if connector.is_enabled() && !requires_approval {
                    // Pre-call ledger intent: write a reconcilable `approved` tool_executions row
                    // BEFORE the external call (mirroring the ad-hoc saga) so an executed write is
                    // never absent from the ledger even if finalization later fails. Also mark the
                    // step `running` (the step-level intent), then yield to the caller.
                    let exec_id = gateway::insert_run_tool_intent(
                        tx,
                        tenant,
                        principal,
                        &tool_id,
                        &cfg.arguments,
                        runtime_policy
                            .as_ref()
                            .expect("real connector has runtime policy"),
                        run_id,
                    )
                    .await?;
                    sqlx::query(
                        // Mark in-flight WITHOUT touching `attempts`: for a retriable tool step the
                        // count must ACCUMULATE across worker re-drives, so `attempts` is owned by
                        // `record_tool_outcome` (bumped on success, and via schedule_retry_or_fail on
                        // failure) rather than reset to 1 on every drive.
                        "UPDATE run_steps SET status = 'running' WHERE id = $1",
                    )
                    .bind(step_id)
                    .execute(&mut **tx)
                    .await?;
                    // Reflect the in-flight execution on the run too — it may have been
                    // `waiting` before a resume-driven auto tool, so mirror the reported
                    // `running` in the DB (idempotent for a start, already `running`).
                    sqlx::query("UPDATE runs SET status = 'running' WHERE run_id = $1")
                        .bind(run_id)
                        .execute(&mut **tx)
                        .await?;
                    append_event(
                        tx,
                        tenant,
                        run_id,
                        "step_started",
                        &serde_json::json!({ "step_index": step_index, "name": name, "tool_id": tool_id }),
                    )
                    .await?;
                    return Ok(AdvanceOutcome::ExecuteTool(RunToolCall {
                        step_id,
                        step_index,
                        name,
                        tool_id,
                        arguments: cfg.arguments,
                        principal: principal.to_string(),
                        exec_id,
                    }));
                }

                // Either no connector (placeholder for auto + approval), OR a real connector
                // with approval required. The gateway provisions + inserts the tool_executions
                // row (placeholder-executed OR pending) and returns the approval decision.
                let req = ToolExecuteRequest {
                    tenant_id: tenant.to_string(),
                    principal_id: principal.to_string(),
                    tool_id: cfg.tool_id,
                    arguments: cfg.arguments,
                    policy: ToolPolicy {
                        approval_mode: if requires_approval {
                            ApprovalMode::Required
                        } else {
                            cfg.approval_mode
                        },
                        reason: None,
                    },
                };
                let resp = gateway::execute_in_tx(
                    tx,
                    tenant,
                    principal,
                    &req,
                    runtime_policy.as_ref(),
                    Some(run_id),
                    None,
                )
                .await?;
                audit::record_in_tx(
                    tx,
                    tenant,
                    Some(principal),
                    "tool.execute",
                    &tool_id,
                    if resp.requires_approval {
                        "require_approval"
                    } else {
                        "success"
                    },
                    serde_json::json!({ "run_id": run_id, "status": resp.status, "step_index": step_index }),
                )
                .await?;
                if resp.requires_approval {
                    let token = suspend_on_gate(
                        tx,
                        tenant,
                        run_id,
                        step_id,
                        &serde_json::json!({ "step_index": step_index, "name": name, "kind": "tool", "tool_id": tool_id }),
                        &serde_json::json!({ "reason": "tool_approval", "step_index": step_index, "name": name, "tool_id": tool_id }),
                    )
                    .await?;
                    return Ok(AdvanceOutcome::Waiting(token));
                }
                // Auto + no connector: the placeholder result completes the step.
                let output = serde_json::json!({
                    "tool_id": resp.tool_id, "status": resp.status, "output": resp.output
                });
                sqlx::query(
                    "UPDATE run_steps SET status = 'completed', output = $2, attempts = 1 \
                     WHERE id = $1",
                )
                .bind(step_id)
                .bind(sqlx::types::Json(&output))
                .execute(&mut **tx)
                .await?;
                append_event(
                    tx,
                    tenant,
                    run_id,
                    "step_completed",
                    &serde_json::json!({ "step_index": step_index, "name": name, "tool_id": tool_id }),
                )
                .await?;
                // fall through to the next pending step
            }
            // "fail" (the only remaining kind) — fault injection. Routed through the shared
            // retry/terminal path: with the default `max_attempts = 1` it fails terminally on
            // the first attempt (unchanged), but a fail step given more attempts retries.
            _ => {
                let err = format!("step '{name}' failed");
                return match schedule_retry_or_fail(
                    tx,
                    tenant,
                    run_id,
                    step_id,
                    step_index,
                    &name,
                    attempts,
                    max_attempts,
                    &err,
                    worker_enabled,
                )
                .await?
                {
                    RetryDecision::Rescheduled => Ok(AdvanceOutcome::Scheduled),
                    RetryDecision::Terminal => Ok(AdvanceOutcome::Failed),
                };
            }
        }
    }
}

/// Drive a run to its next pause after an [`AdvanceOutcome::ExecuteTool`]: execute the tool
/// OUTSIDE any transaction, then in a fresh tenant transaction record the outcome + advance,
/// repeating until the run completes, suspends, or fails. A connector failure fails the step
/// (and the run).
///
/// Mirrors the ad-hoc gateway saga: BEFORE the external call, [`advance`] has already committed
/// a reconcilable `approved` `tool_executions` intent row + a `running` step, so a fault during
/// the outcome recording leaves reconcilable state — never a silent lost ledger record. The
/// intent row is finalized to `executed`/`failed` here; a `running` step orphaned by a mid-drive
/// crash is recovered by the async worker (a later PR). NOTE: run I/O is at-least-once — a client
/// that retries a failed `runs.start` starts a NEW run and re-executes; idempotency is a future item.
pub async fn continue_run(
    db: &sqlx::PgPool,
    tenant: &str,
    run_id: uuid::Uuid,
    connector: &ConnectorImpl,
    mut call: RunToolCall,
    worker_enabled: bool,
) -> Result<AdvanceOutcome> {
    loop {
        // Deterministic idempotency key for this tool STEP — the same value across every retry
        // (this call's connect-phase re-send AND a worker step-level re-drive), so a compliant MCP
        // server dedups a re-sent (possibly-executed) call. This is the two-sided contract that makes
        // tool-step retry safe.
        let idempotency_key =
            crate::orchestration::workflow::idempotency_key(run_id, call.step_index);
        // Execute the tool OUTSIDE any DB transaction (never hold a connection across I/O).
        let outcome = connector
            .call(&call.tool_id, &call.arguments, &idempotency_key)
            .await;
        // Record the outcome + advance, atomically, in a fresh tenant transaction.
        let mut tx = tenant_tx(db, tenant).await?;
        // Lock the run row FIRST to keep a consistent lock order (runs before run_steps
        // everywhere), so this recording tx can't deadlock against a concurrent advance/resume
        // that takes the runs `FOR UPDATE` lock before touching run_steps.
        sqlx::query("SELECT 1 FROM runs WHERE run_id = $1 FOR UPDATE")
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        record_tool_outcome(&mut tx, tenant, run_id, &call, &outcome, worker_enabled).await?;
        let advanced = advance(&mut tx, tenant, run_id, connector, worker_enabled).await?;
        tx.commit().await?;
        match advanced {
            AdvanceOutcome::ExecuteTool(next) => call = next,
            other => return Ok(other),
        }
    }
}

/// If the just-approved gate at `step_index` is a TOOL step and a real connector is configured,
/// prepare it for REAL execution and return the [`RunToolCall`] for the drive-loop: re-check
/// RBAC, mark the step + run `running` (the `pending` `tool_executions` row from suspend time
/// is the ledger intent), and append a `step_started` event. Returns `None` for a
/// human_approval gate, a non-tool step, or when no connector is configured — the caller then
/// records the placeholder finalize instead. The run is already locked `FOR UPDATE` by resume.
pub async fn begin_gated_tool(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    step_index: i32,
    connector: &ConnectorImpl,
) -> Result<Option<RunToolCall>> {
    if !connector.is_enabled() {
        return Ok(None);
    }
    let step: Option<(
        uuid::Uuid,
        String,
        String,
        sqlx::types::Json<serde_json::Value>,
    )> = sqlx::query_as(
        "SELECT id, name, kind, config FROM run_steps \
             WHERE run_id = $1 AND step_index = $2 AND status = 'waiting'",
    )
    .bind(run_id)
    .bind(step_index)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((step_id, name, kind, config)) = step else {
        return Ok(None);
    };
    if kind != "tool" {
        return Ok(None);
    }
    let cfg: ToolStep = serde_json::from_value(config.0)
        .map_err(|e| Error::Internal(anyhow::anyhow!("invalid tool step config: {e}")))?;
    let runtime_policy = crate::tools::registry::govern_external_call(
        tx,
        tenant,
        &cfg.tool_id,
        &cfg.arguments,
        connector,
    )
    .await?;
    // The run's principal (fail closed if it was deleted — can't attribute/authorize the call).
    let (principal,): (Option<String>,) =
        sqlx::query_as("SELECT principal_id FROM runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await?;
    let principal = principal.ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "run has no principal; cannot run a governed tool step"
        ))
    })?;
    // Separation of duties: re-check tool.execute at execution time (DB-authoritative role).
    authorize_in_tx(
        tx,
        &principal,
        None,
        false,
        tenant,
        Action::ToolExecute,
        &cfg.tool_id,
    )
    .await?;
    // The `pending` tool_executions row written at suspend time is the ledger intent to
    // finalize (at most one pending per run — advance suspends at the FIRST waiting step).
    // Filter by tool_id too, and surface a missing row as a clean internal error (not a raw
    // RowNotFound) so an unexpected inconsistency is legible.
    let exec_row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT id FROM tool_executions \
         WHERE run_id = $1 AND tool_id = $2 AND status = 'pending' LIMIT 1",
    )
    .bind(run_id)
    .bind(cfg.tool_id.trim())
    .fetch_optional(&mut **tx)
    .await?;
    let (exec_id,) = exec_row.ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "no pending tool_execution for run {run_id} tool {}",
            cfg.tool_id
        ))
    })?;
    // Flip the intent row `pending` -> `approved` so it reads as IN-FLIGHT (committed to
    // execution) during the out-of-tx connector call, not as still-awaiting-approval — mirroring
    // the auto path's `approved` intent. It is finalized to executed/failed after the call.
    sqlx::query(
        "UPDATE tool_executions SET status = 'approved', definition_revision = $1, \
         rollback_tool_id = $2 WHERE id = $3",
    )
    .bind(runtime_policy.revision)
    .bind(runtime_policy.rollback_tool_id.as_deref())
    .bind(exec_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE run_steps SET status = 'running' WHERE id = $1")
        .bind(step_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE runs SET status = 'running' WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    append_event(
        tx,
        tenant,
        run_id,
        "step_started",
        &serde_json::json!({ "step_index": step_index, "name": name, "tool_id": cfg.tool_id, "approved": true }),
    )
    .await?;
    Ok(Some(RunToolCall {
        step_id,
        step_index,
        name,
        tool_id: cfg.tool_id,
        arguments: cfg.arguments,
        principal,
        exec_id,
    }))
}

/// Finalize a `running` tool step from the connector `outcome`: record the `tool_executions`
/// row, then complete the step (success) or route the failure through the shared retry/terminal
/// path — a retriable tool step (`max_attempts > 1`) is rescheduled with a backoff and re-driven by
/// the worker (re-sending the SAME deterministic idempotency key, so a compliant MCP server dedups),
/// while a `max_attempts = 1` tool fails the step AND the run terminally (unchanged). Audits + events.
async fn record_tool_outcome(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    call: &RunToolCall,
    outcome: &Result<serde_json::Value>,
    worker_enabled: bool,
) -> Result<()> {
    match outcome {
        Ok(output) => {
            let step_output = serde_json::json!({
                "tool_id": call.tool_id, "status": "executed", "output": output
            });
            // Guard `status = 'running'`: complete the step ONLY if it is still in flight. If the
            // crash-recovery worker already reconciled this run (failing the orphaned `running`
            // step) during a post-call stall, this updates 0 rows — and then EVERY success write is
            // skipped below, so we never flip the ledger back to `executed`, audit `success`, or
            // append a `step_completed` event under a `failed` run (the failure branch guards the
            // same way). Bump `attempts` on completion (no longer reset to 1 per drive — see the
            // auto-tool branch): a first-try success ends at 1, a success after N retries at N+1.
            let completed = sqlx::query(
                "UPDATE run_steps SET status = 'completed', output = $2, attempts = attempts + 1 \
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(call.step_id)
            .bind(sqlx::types::Json(&step_output))
            .execute(&mut **tx)
            .await?
            .rows_affected()
                == 1;
            if completed {
                // Finalize THIS attempt's `approved` intent row to `executed` with the real result.
                gateway::update_execution(tx, call.exec_id, "executed", output).await?;
                audit::record_in_tx(
                    tx,
                    tenant,
                    Some(&call.principal),
                    "tool.execute",
                    &call.tool_id,
                    "success",
                    serde_json::json!({ "run_id": run_id, "status": "executed", "step_index": call.step_index }),
                )
                .await?;
                append_event(
                    tx,
                    tenant,
                    run_id,
                    "step_completed",
                    &serde_json::json!({ "step_index": call.step_index, "name": call.name, "tool_id": call.tool_id }),
                )
                .await?;
            }
        }
        Err(e) => {
            let err_val = serde_json::json!({ "error": e.to_string() });
            // Finalize THIS attempt's `approved` intent row to `failed` (the ledger is per-attempt;
            // a retry re-drive opens a NEW intent row). Always recorded, even if the step was
            // meanwhile reconciled by the worker.
            gateway::update_execution(tx, call.exec_id, "failed", &err_val).await?;
            let err = format!("tool step '{}' failed", call.name);
            audit::record_in_tx(
                tx,
                tenant,
                Some(&call.principal),
                "tool.execute",
                &call.tool_id,
                "error",
                serde_json::json!({ "run_id": run_id, "status": "failed", "step_index": call.step_index }),
            )
            .await?;
            // Guard `status = 'running'`: only act on a step still in flight. If the crash-recovery
            // worker already reconciled this run (failing the orphaned step) while the call was in
            // flight, do nothing — never resurrect (reschedule) a step the worker terminated.
            let inflight: Option<(i32, i32)> = sqlx::query_as(
                "SELECT attempts, max_attempts FROM run_steps WHERE id = $1 AND status = 'running'",
            )
            .bind(call.step_id)
            .fetch_optional(&mut **tx)
            .await?;
            if let Some((attempts, max_attempts)) = inflight {
                // Retriable (max_attempts > 1) -> reschedule with a backoff (worker re-drives, re-
                // sending the same deterministic Idempotency-Key so a compliant server dedups); else
                // fail the step AND the run terminally. `schedule_retry_or_fail` bumps `attempts`.
                schedule_retry_or_fail(
                    tx,
                    tenant,
                    run_id,
                    call.step_id,
                    call.step_index,
                    &call.name,
                    attempts,
                    max_attempts,
                    &err,
                    worker_enabled,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Base retry backoff in seconds — the first retry waits this long.
const RETRY_BASE_SECS: i64 = 5;
/// Backoff ceiling — a retry never waits longer than this (bounds exponential growth).
const RETRY_CAP_SECS: i64 = 300;

/// Backoff (seconds) before retry `attempt` (1-based): exponential
/// `RETRY_BASE_SECS * 2^(attempt-1)`, capped at `RETRY_CAP_SECS`.
fn retry_backoff_secs(attempt: i32) -> i64 {
    let shift = attempt.saturating_sub(1).clamp(0, 16) as u32;
    RETRY_BASE_SECS
        .saturating_mul(1i64 << shift)
        .min(RETRY_CAP_SECS)
}

/// Whether a retriable failure was rescheduled or exhausted its attempts.
enum RetryDecision {
    Rescheduled,
    Terminal,
}

/// Handle a RETRIABLE step failure. If `worker_enabled` and attempts remain, reschedule the
/// step `pending` with a backoff (`next_attempt_at`) + append `step_retry_scheduled`
/// (`Rescheduled`); otherwise fail the step AND the run terminally (`Terminal`), appending
/// `step_failed` + `failed`. Bumps `attempts` either way.
#[allow(clippy::too_many_arguments)]
async fn schedule_retry_or_fail(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    step_id: uuid::Uuid,
    step_index: i32,
    name: &str,
    attempts: i32,
    max_attempts: i32,
    err: &str,
    worker_enabled: bool,
) -> Result<RetryDecision> {
    let new_attempts = attempts + 1;
    if worker_enabled && new_attempts < max_attempts {
        let backoff = retry_backoff_secs(new_attempts);
        sqlx::query(
            "UPDATE run_steps SET status = 'pending', attempts = $2, error = $3, \
             next_attempt_at = now() + make_interval(secs => $4) WHERE id = $1",
        )
        .bind(step_id)
        .bind(new_attempts)
        .bind(err)
        .bind(backoff)
        .execute(&mut **tx)
        .await?;
        // Reflect the deferral on the run (idempotent for a start — already `running`) so worker
        // discovery (status='running') surfaces the rescheduled step even if the failure occurred
        // on a resume-driven path that left the run `waiting`. Mirrors the auto-tool branch.
        sqlx::query("UPDATE runs SET status = 'running' WHERE run_id = $1")
            .bind(run_id)
            .execute(&mut **tx)
            .await?;
        append_event(
            tx,
            tenant,
            run_id,
            "step_retry_scheduled",
            &serde_json::json!({
                "step_index": step_index, "name": name,
                "attempt": new_attempts, "max_attempts": max_attempts, "backoff_secs": backoff,
            }),
        )
        .await?;
        Ok(RetryDecision::Rescheduled)
    } else {
        sqlx::query(
            "UPDATE run_steps SET status = 'failed', attempts = $2, error = $3, \
             next_attempt_at = NULL WHERE id = $1",
        )
        .bind(step_id)
        .bind(new_attempts)
        .bind(err)
        .execute(&mut **tx)
        .await?;
        sqlx::query("UPDATE runs SET status = 'failed', error = $2 WHERE run_id = $1")
            .bind(run_id)
            .bind(err)
            .execute(&mut **tx)
            .await?;
        append_event(
            tx,
            tenant,
            run_id,
            "step_failed",
            &serde_json::json!({ "step_index": step_index, "name": name, "error": err }),
        )
        .await?;
        append_event(
            tx,
            tenant,
            run_id,
            "failed",
            &serde_json::json!({ "error": err }),
        )
        .await?;
        Ok(RetryDecision::Terminal)
    }
}

/// Suspend the run on an approval gate: mark the step + run `waiting`, open a fresh single-use
/// checkpoint carrying `checkpoint_state`, append a `suspended` event, and return the token.
async fn suspend_on_gate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    step_id: uuid::Uuid,
    checkpoint_state: &serde_json::Value,
    event_payload: &serde_json::Value,
) -> Result<String> {
    sqlx::query("UPDATE run_steps SET status = 'waiting' WHERE id = $1")
        .bind(step_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE runs SET status = 'waiting' WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    let token = new_token();
    sqlx::query(
        "INSERT INTO run_checkpoints (run_id, tenant_id, token, state, status) \
         VALUES ($1, $2, $3, $4, 'open')",
    )
    .bind(run_id)
    .bind(tenant)
    .bind(&token)
    .bind(sqlx::types::Json(checkpoint_state))
    .execute(&mut **tx)
    .await?;
    append_event(tx, tenant, run_id, "suspended", event_payload).await?;
    Ok(token)
}

/// Append an ordered event to a run's append-only history. `seq` is the next value per run
/// (`MAX(seq)+1`), backed by `UNIQUE (run_id, seq)`. Safe within a single transaction (the
/// caller holds the run `FOR UPDATE`, so there is no concurrent writer to the same run).
pub(super) async fn append_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    run_id: uuid::Uuid,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO run_events (run_id, tenant_id, seq, event_type, payload) \
         VALUES ($1, $2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = $1), $3, $4)",
    )
    .bind(run_id)
    .bind(tenant)
    .bind(event_type)
    .bind(sqlx::types::Json(payload))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// An opaque, unguessable resume token (two v4 UUIDs → 64 hex chars, 244 bits).
pub(super) fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::{retry_backoff_secs, RETRY_BASE_SECS, RETRY_CAP_SECS};

    #[test]
    fn backoff_is_exponential_and_capped() {
        // base * 2^(attempt-1), capped at RETRY_CAP_SECS.
        assert_eq!(retry_backoff_secs(1), RETRY_BASE_SECS); // 5
        assert_eq!(retry_backoff_secs(2), RETRY_BASE_SECS * 2); // 10
        assert_eq!(retry_backoff_secs(3), RETRY_BASE_SECS * 4); // 20
        assert_eq!(retry_backoff_secs(4), RETRY_BASE_SECS * 8); // 40
                                                                // A large attempt saturates to the cap rather than overflowing.
        assert_eq!(retry_backoff_secs(30), RETRY_CAP_SECS);
        assert_eq!(retry_backoff_secs(1000), RETRY_CAP_SECS);
    }
}
