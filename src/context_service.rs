//! Context service — user/team/org context + personal memory.

use crate::db::tenant_tx;
use crate::domain::{Context, DataClassification};
use crate::error::{Error, Result};

/// Stores principal [`Context`] profiles in Postgres, tenant-isolated by RLS
/// (migration `0009`).
///
/// Personal memory is kept **separate** from org knowledge, and PII is
/// minimized/flagged via `data_classification`. The context service is what lets
/// the brain answer "who is asking, on whose behalf, with what limits?" for every
/// governed action.
pub struct ContextService {
    db: sqlx::PgPool,
}

/// Columns selected for a full context read (order matches [`ContextRow`]).
/// `approval_limit_usd` is cast to `float8` so it maps to `f64` (the column is
/// `numeric`, which sqlx would otherwise decode as a decimal type).
const CONTEXT_COLS: &str = "principal_id, tenant_id, team_ids, role, location, \
     approval_limit_usd::float8 AS approval_limit_usd, preferred_tools, \
     active_projects, policy_overrides, data_classification, updated_at";

/// One `context` row as stored, mapped from Postgres.
#[derive(sqlx::FromRow)]
struct ContextRow {
    principal_id: String,
    tenant_id: String,
    team_ids: Vec<String>,
    role: Option<String>,
    location: Option<String>,
    approval_limit_usd: Option<f64>,
    preferred_tools: Vec<String>,
    active_projects: Vec<String>,
    policy_overrides: Vec<String>,
    data_classification: serde_json::Value,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<ContextRow> for Context {
    fn from(r: ContextRow) -> Self {
        Context {
            principal_id: r.principal_id,
            tenant_id: Some(r.tenant_id),
            team_ids: r.team_ids,
            role: r.role,
            location: r.location,
            approval_limit_usd: r.approval_limit_usd,
            preferred_tools: r.preferred_tools,
            active_projects: r.active_projects,
            policy_overrides: r.policy_overrides,
            data_classification: serde_json::from_value::<DataClassification>(
                r.data_classification,
            )
            .unwrap_or_default(),
            updated_at: Some(r.updated_at),
        }
    }
}

impl ContextService {
    /// Construct a context service over the given pool.
    pub fn new(db: sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Upsert the full context document for `ctx.principal_id` under `tenant`.
    ///
    /// Identity is **tenant-scoped**, so the subject principal is provisioned into
    /// `principals` if new — this can never touch or collide with another tenant's
    /// same-id principal. The identity anchor's `role` is managed separately (a
    /// dedicated admin path), so it is deliberately not set from the context here.
    ///
    /// **PUT semantics:** this replaces the whole context document — fields the
    /// caller omits reset to their defaults, so send the complete document to
    /// update.
    pub async fn upsert(&self, tenant: &str, ctx: &Context) -> Result<()> {
        let principal_id = ctx.principal_id.trim();
        if principal_id.is_empty() {
            return Err(Error::BadRequest("principal_id is required".into()));
        }
        // Validate the spend cap up front so out-of-range input is a clean 400
        // (the numeric(14,2) column would otherwise overflow -> SQLSTATE 22003 -> 500).
        if let Some(limit) = ctx.approval_limit_usd {
            if !limit.is_finite() || !(0.0..=999_999_999_999.99).contains(&limit) {
                return Err(Error::BadRequest(format!(
                    "approval_limit_usd {limit} is out of range [0, 999999999999.99]"
                )));
            }
        }

        let mut tx = tenant_tx(&self.db, tenant).await?;

        // Provision the subject principal if new. Tenant-scoped identity means this
        // can never collide with another tenant's principal; `role` is managed
        // elsewhere, so it is intentionally left unset here.
        sqlx::query(
            "INSERT INTO principals (principal_id, tenant_id) VALUES ($1, $2) \
             ON CONFLICT (tenant_id, principal_id) DO NOTHING",
        )
        .bind(principal_id)
        .bind(tenant)
        .execute(&mut *tx)
        .await
        // An unknown tenant (FK 23503) is a 400, not a 500.
        .map_err(|e| Error::db_or_conflict(e, "principal already exists"))?;

        // Replace the context document. The BEFORE UPDATE trigger maintains
        // updated_at; `approval_limit_usd` is bound as f64 and cast to numeric.
        sqlx::query(
            r#"
            INSERT INTO context
                (principal_id, tenant_id, team_ids, role, location, approval_limit_usd,
                 preferred_tools, active_projects, policy_overrides, data_classification)
            VALUES ($1, $2, $3, $4, $5, $6::numeric, $7, $8, $9, $10)
            ON CONFLICT (tenant_id, principal_id) DO UPDATE SET
                team_ids            = EXCLUDED.team_ids,
                role                = EXCLUDED.role,
                location            = EXCLUDED.location,
                approval_limit_usd  = EXCLUDED.approval_limit_usd,
                preferred_tools     = EXCLUDED.preferred_tools,
                active_projects     = EXCLUDED.active_projects,
                policy_overrides    = EXCLUDED.policy_overrides,
                data_classification = EXCLUDED.data_classification
            "#,
        )
        .bind(principal_id)
        .bind(tenant)
        .bind(&ctx.team_ids)
        .bind(&ctx.role)
        .bind(&ctx.location)
        .bind(ctx.approval_limit_usd)
        .bind(&ctx.preferred_tools)
        .bind(&ctx.active_projects)
        .bind(&ctx.policy_overrides)
        .bind(sqlx::types::Json(&ctx.data_classification))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Fetch a principal's context profile for `tenant`, or `None` if absent.
    pub async fn get(&self, tenant: &str, principal_id: &str) -> Result<Option<Context>> {
        let principal_id = principal_id.trim();
        let sql = format!(
            "SELECT {CONTEXT_COLS} FROM context WHERE tenant_id = $1 AND principal_id = $2"
        );
        let mut tx = tenant_tx(&self.db, tenant).await?;
        let row: Option<ContextRow> = sqlx::query_as(&sql)
            .bind(tenant)
            .bind(principal_id)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row.map(Into::into))
    }
}
