//! Shared client `Idempotency-Key` header parsing + the generic idempotency-key registry (migration
//! 0024) — used by `/runs.start` and `/tool.execute`.

use axum::http::HeaderMap;

use crate::error::{Error, Result};

/// Max accepted `Idempotency-Key` length in bytes (bounds the tenant-scoped unique index key).
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// Parse the optional `Idempotency-Key` request header. Absent/blank → `None` (keyless, the
/// at-least-once default); a non-UTF-8 or over-long value is a 400 (never silently truncated).
///
/// This is the SINGLE canonical trim point — nothing downstream re-trims the key before it is
/// bound into the tenant-scoped unique index, so ` k ` and `k` must be normalized to one key
/// here. Header values are opaque octets, so decode with `std::str::from_utf8` (not
/// `HeaderValue::to_str`, which rejects any non-ASCII byte) to honor a full-UTF-8 key.
pub fn parse_idempotency_key(headers: &HeaderMap) -> Result<Option<String>> {
    let Some(raw) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = std::str::from_utf8(raw.as_bytes())
        .map_err(|_| Error::BadRequest("Idempotency-Key must be valid UTF-8".into()))?
        .trim();
    if key.is_empty() {
        return Ok(None);
    }
    if key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(Error::BadRequest(format!(
            "Idempotency-Key too long (max {MAX_IDEMPOTENCY_KEY_LEN} bytes)"
        )));
    }
    Ok(Some(key.to_string()))
}

/// The canonical request bytes to fingerprint: the serde JSON serialization (stable field order per
/// the struct definition), so two logically-equal requests fingerprint identically (and a reordered
/// or extra-field JSON body normalizes to the same request). Serialization of a plain request DTO is
/// infallible; an impossible failure hashes empty (fail-closed to a single fingerprint).
pub fn request_bytes(req: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(req).unwrap_or_default()
}

/// The outcome of claiming an idempotency key with its request fingerprint (migration 0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyClaim {
    /// First use of this `(tenant, scope, key)` — proceed to execute the request.
    Fresh,
    /// Re-use with the SAME request fingerprint — proceed; the per-table replay (#32/#33) returns the
    /// original result.
    Replay,
    /// Re-use with a DIFFERENT request fingerprint — the caller must reject with `409 Conflict` (a
    /// key is bound to its first request; a different body is a client error, not a silent replay).
    Mismatch,
}

/// Record/verify the request fingerprint for `(tenant, scope, key)` in the CALLER's transaction, and
/// classify the call as [`IdempotencyClaim`]. This is the generic fingerprint gate (migration 0024):
/// it runs ALONGSIDE the per-table idempotency (`runs.idempotency_key` / `tool_executions.
/// idempotency_key`), adding ONLY reuse-with-different-body detection — the existing replay paths are
/// unchanged. The fingerprint is a server-side `sha256` (no crypto dependency). Same tx as the write,
/// so the fingerprint claim and the run/execution commit atomically.
pub async fn claim_idempotency_key(
    conn: &mut sqlx::PgConnection,
    tenant: &str,
    scope: &str,
    key: &str,
    request: &[u8],
) -> Result<IdempotencyClaim> {
    // First use records the fingerprint; a repeat conflicts (DO NOTHING) and we compare below.
    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO idempotency_keys (tenant_id, scope, idempotency_key, request_fingerprint) \
             VALUES ($1, $2, $3, encode(sha256($4), 'hex')) \
         ON CONFLICT (tenant_id, scope, idempotency_key) DO NOTHING \
         RETURNING request_fingerprint",
    )
    .bind(tenant)
    .bind(scope)
    .bind(key)
    .bind(request)
    .fetch_optional(&mut *conn)
    // Map the only client-influenced FK (tenant_id -> tenants) to a 400, matching every other
    // tenant-scoped write: this gate now runs BEFORE the provisioning inserts, so an authenticated-
    // but-unprovisioned tenant must still be a 400 here (not a 500), and keyed/keyless stay
    // consistent. ON CONFLICT DO NOTHING already absorbs the 23505 unique case.
    .await
    .map_err(|e| Error::db_or_conflict(e, "idempotency key already used"))?;
    if inserted.is_some() {
        return Ok(IdempotencyClaim::Fresh);
    }
    // Conflict: does the incoming fingerprint match the one stored on first use? The row necessarily
    // exists (the INSERT above conflicted on it), but use fetch_optional + a clean Internal error
    // rather than a raw RowNotFound if it somehow vanished (e.g. a concurrent tenant CASCADE delete).
    let matches: Option<(bool,)> = sqlx::query_as(
        "SELECT request_fingerprint = encode(sha256($4), 'hex') \
         FROM idempotency_keys WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3",
    )
    .bind(tenant)
    .bind(scope)
    .bind(key)
    .bind(request)
    .fetch_optional(&mut *conn)
    .await?;
    let (matches,) = matches.ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "idempotency key row for ({scope}) disappeared concurrently"
        ))
    })?;
    Ok(if matches {
        IdempotencyClaim::Replay
    } else {
        IdempotencyClaim::Mismatch
    })
}

/// Record (or REFRESH) the fingerprint for `(scope, key)`: insert it, or REPLACE a stored fingerprint
/// when the content changed. Use for a MUTABLE, re-ingestable resource whose latest content is
/// authoritative (a changed body updates the marker) — as opposed to [`claim_idempotency_key`], where
/// a key is bound immutably to its first request and a different body is a `409`. Runs in the caller's
/// transaction, so the marker commits atomically with the resource write.
pub async fn record_fingerprint(
    conn: &mut sqlx::PgConnection,
    tenant: &str,
    scope: &str,
    key: &str,
    request: &[u8],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO idempotency_keys (tenant_id, scope, idempotency_key, request_fingerprint) \
             VALUES ($1, $2, $3, encode(sha256($4), 'hex')) \
         ON CONFLICT (tenant_id, scope, idempotency_key) \
         DO UPDATE SET request_fingerprint = EXCLUDED.request_fingerprint",
    )
    .bind(tenant)
    .bind(scope)
    .bind(key)
    .bind(request)
    .execute(&mut *conn)
    .await
    // The only client-influenced FK (tenant_id -> tenants) maps to a 400 like every tenant-scoped write.
    .map_err(|e| Error::db_or_conflict(e, "idempotency conflict"))?;
    Ok(())
}
