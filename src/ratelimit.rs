//! Per-tenant request rate limiting — a Postgres-backed token bucket, so the limit is EXACT and
//! SHARED across replicas (unlike a per-process counter).
//!
//! Each tenant has one `rate_limits` row (`tokens` refilled continuously at
//! [`crate::config::Config::rate_limit_tenant_rps`] tokens/sec, capped at
//! [`crate::config::Config::rate_limit_burst`]). On every authenticated request (when enabled) the
//! bucket is refilled-then-consumed under a `FOR UPDATE` row lock inside a tenant transaction: this
//! serializes only the SAME tenant's concurrent requests (different tenants never contend), so the
//! count is exact even across replicas. A request that can't consume a token is
//! [`Error::TooManyRequests`] (`429` + `Retry-After`). The bucket math is a pure, unit-tested
//! function; the DB only persists the state.

use sqlx::PgPool;

use crate::auth::Principal;
use crate::config::Config;
use crate::db::tenant_tx;
use crate::error::{Error, Result};

/// The outcome of one token-bucket step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Whether a token was available and consumed (the request is allowed).
    pub allowed: bool,
    /// The token balance to persist after this step.
    pub new_tokens: f64,
    /// When denied, whole seconds until one token accrues (for `Retry-After`); `0` when allowed.
    pub retry_after_secs: u64,
}

/// Refill a bucket by `elapsed_secs * rate` (capped at `capacity`), then try to consume one token.
/// Pure + deterministic so the policy is testable independent of the database. `rate > 0` and
/// `capacity >= 1` are guaranteed by config validation at load.
pub fn refill_and_consume(tokens: f64, elapsed_secs: f64, capacity: f64, rate: f64) -> Decision {
    let refilled = (tokens + elapsed_secs.max(0.0) * rate).min(capacity);
    if refilled >= 1.0 {
        Decision {
            allowed: true,
            new_tokens: refilled - 1.0,
            retry_after_secs: 0,
        }
    } else {
        // Seconds until the balance reaches one whole token, at least 1.
        let wait = ((1.0 - refilled) / rate).ceil().max(1.0);
        Decision {
            allowed: false,
            new_tokens: refilled,
            retry_after_secs: wait as u64,
        }
    }
}

/// Enforce the per-tenant rate limit for `principal`. A request with no tenant is out of scope
/// (rate limiting is per-tenant; tenant-scoped handlers reject a tenantless request anyway).
///
/// The bucket row is created full for a new tenant, then locked (`FOR UPDATE`) and updated inside a
/// single [`tenant_tx`] so RLS is satisfied (`tenant_id` is also bound explicitly) and concurrent
/// same-tenant requests serialize exactly. Postgres `now()` is the transaction timestamp, so a
/// just-inserted row reads zero elapsed time (starts at full capacity).
pub async fn enforce(db: &PgPool, principal: &Principal, config: &Config) -> Result<()> {
    let Some(tenant) = principal.tenant_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let capacity = config.rate_limit_burst;
    let rate = config.rate_limit_tenant_rps;

    let mut tx = tenant_tx(db, tenant).await?;
    // A new tenant starts with a FULL bucket; an existing bucket is left untouched.
    sqlx::query("INSERT INTO rate_limits (tenant_id, tokens) VALUES ($1, $2) ON CONFLICT (tenant_id) DO NOTHING")
        .bind(tenant)
        .bind(capacity)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::db_or_conflict(e, "rate limit"))?;
    let row: Option<(f64, f64)> = sqlx::query_as(
        "SELECT tokens, EXTRACT(EPOCH FROM (now() - updated_at))::float8 \
         FROM rate_limits WHERE tenant_id = $1 FOR UPDATE",
    )
    .bind(tenant)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::Db)?;
    // The INSERT ... ON CONFLICT DO NOTHING above guarantees the row exists in THIS transaction; a
    // missing row is impossible, so treat it as an internal error rather than a raw RowNotFound.
    let (tokens, elapsed_secs) = row
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("rate_limits row missing after upsert")))?;

    let decision = refill_and_consume(tokens, elapsed_secs, capacity, rate);
    // Persist ONLY when a token was consumed. A DENIED request leaves the bucket unchanged, and the
    // lazy refill is always measured from the last CONSUME's `updated_at` — so skipping the write is
    // EQUIVALENT (a denied `refilled` is < 1 <= capacity, hence never capped, so the next request
    // recomputes the same balance) while avoiding a write on every rejected request under a flood.
    if decision.allowed {
        sqlx::query("UPDATE rate_limits SET tokens = $2, updated_at = now() WHERE tenant_id = $1")
            .bind(tenant)
            .bind(decision.new_tokens)
            .execute(&mut *tx)
            .await
            .map_err(Error::Db)?;
    }
    tx.commit().await.map_err(Error::Db)?;

    if decision.allowed {
        Ok(())
    } else {
        Err(Error::TooManyRequests {
            retry_after_secs: decision.retry_after_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_bucket_allows_and_debits_one_token() {
        let d = refill_and_consume(20.0, 0.0, 20.0, 10.0);
        assert!(d.allowed);
        assert_eq!(d.new_tokens, 19.0);
        assert_eq!(d.retry_after_secs, 0);
    }

    #[test]
    fn an_empty_bucket_denies_with_a_retry_hint() {
        // 0 tokens, no elapsed time -> nothing to consume; wait ceil(1/rate) = ceil(0.1) = 1s.
        let d = refill_and_consume(0.0, 0.0, 20.0, 10.0);
        assert!(!d.allowed);
        assert_eq!(d.new_tokens, 0.0);
        assert_eq!(d.retry_after_secs, 1);
    }

    #[test]
    fn slow_rate_gives_a_longer_retry_hint() {
        // rate 0.5/s, empty bucket -> 1 token needs 2s.
        let d = refill_and_consume(0.0, 0.0, 5.0, 0.5);
        assert!(!d.allowed);
        assert_eq!(d.retry_after_secs, 2);
    }

    #[test]
    fn refill_accrues_over_elapsed_time_then_allows() {
        // 0 tokens but 1s elapsed at 10/s -> 10 tokens refilled, consume 1 -> 9 left.
        let d = refill_and_consume(0.0, 1.0, 20.0, 10.0);
        assert!(d.allowed);
        assert_eq!(d.new_tokens, 9.0);
    }

    #[test]
    fn refill_is_capped_at_capacity() {
        // A long idle can't exceed the burst capacity.
        let d = refill_and_consume(5.0, 10_000.0, 20.0, 10.0);
        assert!(d.allowed);
        assert_eq!(d.new_tokens, 19.0); // capped at 20, then -1
    }

    #[test]
    fn a_fractional_balance_below_one_is_denied() {
        let d = refill_and_consume(0.4, 0.0, 20.0, 10.0);
        assert!(!d.allowed);
        assert_eq!(d.new_tokens, 0.4); // unchanged; nothing consumed
    }
}
