//! Audit log — the append-only record of governed actions.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::db::tenant_tx;
use crate::domain::AuditEvent;
use crate::error::Result;

/// Append-only audit log over the `audit_events` table (migration 0008),
/// tenant-isolated by RLS (migration 0009).
///
/// Every governed action records `{principal, action, resource, outcome,
/// metadata}` here; the trail is queried tenant-scoped with keyset pagination.
pub struct AuditLog {
    db: sqlx::PgPool,
}

/// Filters + keyset cursor for [`AuditLog::query`]. `limit` is pre-clamped.
#[derive(Debug, Default)]
pub struct AuditFilter {
    pub action: Option<String>,
    pub principal_id: Option<String>,
    pub resource: Option<String>,
    /// Return only events strictly older than this event (the prior page's last id).
    pub before: Option<Uuid>,
    pub limit: i64,
}

/// One `audit_events` row as stored, mapped from Postgres.
#[derive(sqlx::FromRow)]
struct AuditRow {
    event_id: Uuid,
    tenant_id: String,
    principal_id: Option<String>,
    action: String,
    resource: Option<String>,
    outcome: String,
    ts: DateTime<Utc>,
    metadata: serde_json::Value,
}

impl From<AuditRow> for AuditEvent {
    fn from(r: AuditRow) -> Self {
        AuditEvent {
            event_id: r.event_id.to_string(),
            tenant_id: r.tenant_id,
            principal_id: r.principal_id,
            action: r.action,
            resource: r.resource.unwrap_or_default(),
            outcome: r.outcome,
            ts: r.ts,
            metadata: r.metadata,
        }
    }
}

impl AuditLog {
    /// Construct an audit log over the given pool.
    pub fn new(db: sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Append one event for `tenant`. `event_id` and `ts` are assigned by the DB.
    pub async fn record(
        &self,
        tenant: &str,
        principal_id: Option<&str>,
        action: &str,
        resource: &str,
        outcome: &str,
        metadata: serde_json::Value,
    ) -> Result<()> {
        let mut tx = tenant_tx(&self.db, tenant).await?;
        sqlx::query(
            "INSERT INTO audit_events (tenant_id, principal_id, action, resource, outcome, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant)
        .bind(principal_id)
        .bind(action)
        .bind(resource)
        .bind(outcome)
        .bind(sqlx::types::Json(metadata))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Query events for `tenant`, newest-first, applying the filter + keyset
    /// cursor. Returns the page plus a `next_cursor` (the last event_id) when
    /// more rows remain.
    pub async fn query(
        &self,
        tenant: &str,
        filter: &AuditFilter,
    ) -> Result<(Vec<AuditEvent>, Option<String>)> {
        // Defensive: the handler already clamps to 1..=200, but a direct caller
        // could pass a negative limit; max(0) avoids a negative LIMIT / usize underflow.
        let limit = filter.limit.max(0);
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT event_id, tenant_id, principal_id, action, resource, outcome, ts, metadata \
             FROM audit_events WHERE tenant_id = ",
        );
        qb.push_bind(tenant.to_string());
        if let Some(a) = &filter.action {
            qb.push(" AND action = ").push_bind(a.clone());
        }
        if let Some(p) = &filter.principal_id {
            qb.push(" AND principal_id = ").push_bind(p.clone());
        }
        if let Some(r) = &filter.resource {
            qb.push(" AND resource = ").push_bind(r.clone());
        }
        if let Some(before) = filter.before {
            // Keyset: strictly older than the cursor row (tenant-scoped subquery,
            // so a cursor from another tenant selects nothing and pages empty).
            qb.push(
                " AND (ts, event_id) < (SELECT ts, event_id FROM audit_events WHERE tenant_id = ",
            )
            .push_bind(tenant.to_string())
            .push(" AND event_id = ")
            .push_bind(before)
            .push(")");
        }
        qb.push(" ORDER BY ts DESC, event_id DESC LIMIT ")
            .push_bind(limit + 1);

        let mut tx = tenant_tx(&self.db, tenant).await?;
        let rows: Vec<AuditRow> = qb.build_query_as().fetch_all(&mut *tx).await?;
        tx.commit().await?;

        let has_more = rows.len() as i64 > limit;
        let events: Vec<AuditEvent> = rows
            .into_iter()
            .take(limit as usize)
            .map(Into::into)
            .collect();
        let next_cursor = if has_more {
            events.last().map(|e| e.event_id.clone())
        } else {
            None
        };
        Ok((events, next_cursor))
    }
}

/// Append one governed-action event on a CALLER-PROVIDED transaction.
///
/// For a caller already inside a `tenant_tx` — the run executor recording an in-run
/// `tool.execute` — this writes the `audit_events` row ATOMICALLY with the run's own
/// state (it rolls back with the run if the step later fails), and keeps in-run tool
/// calls visible in the governed-action trail alongside ad-hoc `/tool.execute` calls.
pub async fn record_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    principal_id: Option<&str>,
    action: &str,
    resource: &str,
    outcome: &str,
    metadata: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_events (tenant_id, principal_id, action, resource, outcome, metadata) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tenant)
    .bind(principal_id)
    .bind(action)
    .bind(resource)
    .bind(outcome)
    .bind(sqlx::types::Json(metadata))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Best-effort audit: record the event, logging (not failing) on error so an
/// audit hiccup never breaks the governed action.
///
/// KNOWN LIMITATIONS (follow-ups): recording is (1) *after* the action commits,
/// so a crash in the window loses the event, and (2) best-effort — a failed audit
/// write is dropped (logged at error level, not retried). For strict governance,
/// write the audit row in the SAME transaction as the action (or a transactional
/// outbox). Failed attempts ARE audited: handlers pass `error` via
/// [`crate::error::Error::audit_report`], and the [`crate::auth::policy`] gateway
/// records a `deny` (with `metadata.code`) whenever it refuses an action — so a
/// genuine RBAC refusal now lands in the trail, not just downstream errors.
pub async fn record_best_effort(
    db: &sqlx::PgPool,
    tenant: &str,
    principal_id: Option<&str>,
    action: &str,
    resource: &str,
    outcome: &str,
    metadata: serde_json::Value,
) {
    if let Err(e) = AuditLog::new(db.clone())
        .record(tenant, principal_id, action, resource, outcome, metadata)
        .await
    {
        tracing::error!(error = %e, action, "audit event DROPPED (best-effort write failed); trail may be incomplete");
    }
}
