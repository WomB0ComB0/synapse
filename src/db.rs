//! Database pool construction + the tenant-scoped transaction helper.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context as _;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};

use crate::config::Config;
use crate::error::{Error, Result};

/// Latched once the DB role is confirmed to enforce RLS, so the guard in
/// [`tenant_tx`] runs at most once per process (the role never changes at
/// runtime). A privileged role never latches, so it is refused on every call.
static RLS_VERIFIED: AtomicBool = AtomicBool::new(false);

/// Build a **lazily-connected** Postgres pool.
///
/// We use `connect_lazy` so the process can boot without a live database
/// (handy for CI, container start ordering, and health-gated readiness).
/// Connections are established on first use; failures surface at query time
/// and are turned into [`crate::error::Error::Db`].
///
/// Migrations are intentionally **not** run via the `sqlx::migrate!` macro:
/// run them out-of-band with `sqlx migrate run` (sqlx-cli). See the migrations
/// directory / docs (owned separately) for schema + RLS policies.
pub fn init(config: &Config) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .acquire_timeout(std::time::Duration::from_secs(
            config.db_acquire_timeout_secs,
        ))
        .connect_lazy(&config.database_url)
        .context("failed to build Postgres pool")?;
    Ok(pool)
}

/// Why [`assert_rls_enforcing`] could not confirm the DB role is safe.
#[derive(Debug, thiserror::Error)]
pub enum RlsCheckError {
    /// The check query failed (DB unreachable / not ready), so enforcement is
    /// UNKNOWN. Tolerated at boot (the pool is lazy), but `/ready` stays 503.
    #[error("could not verify the database role (database unreachable): {0}")]
    Unreachable(#[source] sqlx::Error),
    /// The effective DB role is SUPERUSER or BYPASSRLS — it silently ignores
    /// every RLS policy (even `FORCE`), so tenant isolation is void.
    #[error(
        "database role '{0}' has SUPERUSER or BYPASSRLS, which silently disables \
         Row-Level Security tenant isolation — run synapse as a non-privileged \
         application role"
    )]
    Privileged(String),
    /// The role OWNS one or more tables that have RLS enabled but not FORCEd.
    /// A table's owner is exempt from RLS unless `FORCE ROW LEVEL SECURITY` is
    /// set, so owning such a table is itself a silent bypass.
    #[error(
        "database role '{role}' owns table(s) with RLS enabled but not FORCEd \
         ({tables:?}); a table owner bypasses RLS unless it is forced — run \
         synapse as a role that does not own the tenant tables, or FORCE RLS on them"
    )]
    OwnerBypass {
        /// The owning (current) role.
        role: String,
        /// Offending table names.
        tables: Vec<String>,
    },
}

/// Confirm the pool's **effective** DB role cannot bypass Row-Level Security.
///
/// RLS (migration `0009`) is the ONLY thing isolating tenants, and a Postgres
/// role can skip it two ways, both checked here: (1) `SUPERUSER`/`BYPASSRLS`
/// bypass every policy unconditionally, even `FORCE`; (2) a table's owner is
/// exempt from that table's RLS unless it is `FORCE`d, so owning such a table is
/// itself a bypass. A deployment mistakenly running as such a role would serve
/// every tenant's rows to every caller **while all tests still pass** —
/// catastrophic and silent — so we refuse to operate that way. Asserted eagerly
/// at boot (fail fast), by the [`tenant_tx`] gate (fail closed before any tenant
/// query), and by `/ready`. Reading `pg_roles`/`pg_class` is available to any
/// role, so the check needs no grant.
pub async fn assert_rls_enforcing(pool: &PgPool) -> std::result::Result<(), RlsCheckError> {
    // Keep readiness to one remote round trip: the correlated catalog query
    // checks inherited table ownership alongside SUPERUSER/BYPASSRLS.
    let (role, privileged, unforced): (String, bool, Vec<String>) = sqlx::query_as(
        "SELECT current_user::text, r.rolsuper OR r.rolbypassrls, \
                ARRAY( \
                    SELECT c.relname::text \
                    FROM pg_class c \
                    JOIN pg_namespace n ON n.oid = c.relnamespace \
                    WHERE n.nspname = 'public' AND c.relkind = 'r' \
                      AND c.relrowsecurity AND NOT c.relforcerowsecurity \
                      AND pg_has_role(current_user, c.relowner, 'USAGE') \
                    ORDER BY c.relname \
                ) \
         FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .map_err(RlsCheckError::Unreachable)?;
    if privileged {
        return Err(RlsCheckError::Privileged(role));
    }

    // Owner privileges are inherited by roles with USAGE of the owner role.
    // Match pg_has_role rather than direct equality so inherited bypasses are
    // rejected while NOINHERIT memberships remain correctly excluded.
    if !unforced.is_empty() {
        return Err(RlsCheckError::OwnerBypass {
            role,
            tables: unforced,
        });
    }
    Ok(())
}

/// Begin a transaction with the multi-tenant RLS guard set for `tenant_id`.
///
/// Every tenant-scoped DB access MUST go through this helper: the Row-Level
/// Security policies from migration `0009` compare each row's `tenant_id`
/// against `app_current_tenant_id()` (the `app.tenant_id` GUC), so with the GUC
/// unset a query sees **zero** rows (fail-closed). Setting it transaction-locally
/// isolates the caller to a single tenant for the life of the returned tx.
///
/// The tenant value is **bound**, never string-formatted, to keep tenant input
/// out of the SQL text. Note `SET LOCAL app.tenant_id = $1` cannot take a bind
/// parameter (SET expects a literal), so we use `set_config(name, value, true)`
/// — its `is_local = true` argument makes it exactly transaction-local, the
/// functional equivalent of `SET LOCAL`.
///
/// Callers must pass the request's `tenant_id` *after* checking it is consistent
/// with the authenticated [`crate::auth::Principal`].
///
/// **Fail-closed RLS gate:** because this is the single chokepoint for every
/// tenant-scoped query, it also verifies (once, then latched) that the DB role
/// can't bypass RLS. A privileged/misconfigured role is refused *before* any
/// tenant data is touched — so the guarantee holds on every request, not only
/// when `/ready` happens to be polled. Auth (401/403) runs earlier in the
/// handler, so this never fires for unauthenticated requests (keeping the
/// DB-free smoke path DB-free).
pub async fn tenant_tx<'a>(pool: &PgPool, tenant_id: &str) -> Result<Transaction<'a, Postgres>> {
    if !RLS_VERIFIED.load(Ordering::Acquire) {
        match assert_rls_enforcing(pool).await {
            Ok(()) => RLS_VERIFIED.store(true, Ordering::Release),
            // A role that can bypass RLS must never run a tenant query.
            Err(e @ (RlsCheckError::Privileged(_) | RlsCheckError::OwnerBypass { .. })) => {
                tracing::error!(error = %e, "refusing tenant transaction: DB role can bypass RLS");
                return Err(Error::Internal(anyhow::anyhow!(
                    "database role cannot enforce RLS tenant isolation"
                )));
            }
            // DB unreachable: surface it as the same DB error the begin() below
            // would produce; nothing is verified, nothing is served.
            Err(RlsCheckError::Unreachable(e)) => return Err(Error::Db(e)),
        }
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
