//! Unified error type + JSON error responses.
//!
//! Every handler returns [`Result`]; [`Error`] implements
//! [`axum::response::IntoResponse`] so failures render as a stable JSON shape:
//! `{ "error": { "code": "...", "message": "..." } }`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The crate-wide error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A referenced resource could not be found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The request was malformed or failed validation.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The caller is not authenticated (missing/invalid principal).
    #[error("unauthorized")]
    Unauthorized,

    /// The caller is authenticated but not permitted (policy denial).
    #[error("forbidden")]
    Forbidden,

    /// The request conflicts with existing state (e.g. re-registering an
    /// immutable resource version).
    #[error("conflict: {0}")]
    Conflict(String),

    /// The tenant exceeded its per-tenant request rate limit (token bucket). Carries the seconds the
    /// caller should wait before retrying, surfaced as a `Retry-After` header on the 429.
    #[error("too many requests")]
    TooManyRequests { retry_after_secs: u64 },

    /// A database/query failure.
    #[error("database error")]
    Db(#[from] sqlx::Error),

    /// An upstream dependency (e.g. the embedding provider) failed or misbehaved.
    /// Distinguished from [`Error::Internal`] so a provider outage reads as a
    /// gateway problem (502), not a bug in synapse.
    #[error("upstream error: {0}")]
    Upstream(String),

    /// Any other internal failure.
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl Error {
    /// Stable, machine-readable error code for the JSON body.
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "not_found",
            Error::BadRequest(_) => "bad_request",
            Error::Unauthorized => "unauthorized",
            Error::Forbidden => "forbidden",
            Error::Conflict(_) => "conflict",
            Error::TooManyRequests { .. } => "too_many_requests",
            Error::Db(_) => "db_error",
            Error::Upstream(_) => "upstream_error",
            Error::Internal(_) => "internal",
        }
    }

    /// HTTP status this error maps to.
    fn status(&self) -> StatusCode {
        match self {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::Forbidden => StatusCode::FORBIDDEN,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Upstream(_) => StatusCode::BAD_GATEWAY,
            Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn client_message(&self) -> String {
        match self {
            Error::Upstream(_) => "an upstream dependency failed".to_string(),
            Error::Db(_) | Error::Internal(_) => "an internal service error occurred".to_string(),
            _ => self.to_string(),
        }
    }

    /// The audit `outcome` for a governed action that failed with this error:
    /// an authorization failure is a policy `deny`; anything else is an `error`.
    pub fn audit_outcome(&self) -> &'static str {
        match self {
            Error::Forbidden | Error::Unauthorized => "deny",
            _ => "error",
        }
    }

    /// The `(outcome, metadata)` to audit for a governed action that failed with
    /// this error — outcome per [`Self::audit_outcome`], metadata carrying the
    /// stable error `code`. Centralizes the failure-audit shape so each handler
    /// makes a single `record_best_effort` call.
    pub fn audit_report(&self) -> (&'static str, serde_json::Value) {
        (self.audit_outcome(), json!({ "code": self.code() }))
    }

    /// Map expected Postgres constraint violations on a write to a 4xx instead of
    /// a 500:
    /// - `23505` unique-violation → [`Error::Conflict`] (409) with `conflict_msg`
    ///   (e.g. losing a race on an immutable/unique constraint);
    /// - `23503` foreign-key violation → [`Error::BadRequest`] (400): in every
    ///   write path the only client-influenced FK is `tenant_id → tenants`, so
    ///   this means the (authenticated) tenant isn't provisioned. The message is
    ///   generic and never echoes the tenant, so it's not a cross-tenant oracle.
    ///
    /// Any other database error stays [`Error::Db`] (500).
    pub fn db_or_conflict(e: sqlx::Error, conflict_msg: impl Into<String>) -> Error {
        if let sqlx::Error::Database(db) = &e {
            match db.code().as_deref() {
                Some("23505") => return Error::Conflict(conflict_msg.into()),
                Some("23503") => {
                    return Error::BadRequest("request references an unknown tenant".into())
                }
                _ => {}
            }
        }
        Error::Db(e)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.code();
        // Pull the retry hint before constructing the response.
        let retry_after = match &self {
            Error::TooManyRequests { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        };
        let message = self.client_message();

        // Server faults get full detail in the logs; clients get a stable shape.
        if status.is_server_error() {
            tracing::error!(error = %self, status = %status, "request failed");
        } else {
            tracing::debug!(error = %self, status = %status, "request rejected");
        }

        let body = Json(json!({ "error": { "code": code, "message": message } }));
        let mut resp = (status, body).into_response();
        // Advise a rate-limited caller when to retry (a numeric second-count is always a valid header).
        if let Some(secs) = retry_after {
            if let Ok(hv) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, hv);
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn server_error_details_are_not_client_visible() {
        let upstream = Error::Upstream(
            "request to https://secret-provider.example failed with credential detail".to_string(),
        );
        assert_eq!(upstream.client_message(), "an upstream dependency failed");

        let internal = Error::Internal(anyhow::anyhow!("private invariant detail"));
        assert_eq!(
            internal.client_message(),
            "an internal service error occurred"
        );
    }
}
