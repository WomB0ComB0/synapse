//! Authentication + the Policy & Access Gateway.
//!
//! Four caller-identity modes, chosen ONCE at startup by [`JwtDecoder::from_config`] in PRECEDENCE
//! order (JWKS > static RS256 public key > HS256 secret > trusted headers):
//!
//! - **Verified JWT — RS256 via JWKS ([`crate::config::Config::auth_jwks_url`] set):** the caller
//!   presents `Authorization: Bearer <RS256 JWT>` whose `kid` selects a PUBLIC key fetched (and
//!   rotation-refreshed) from the configured JWKS endpoint. Asymmetric — the service holds only
//!   public keys and CANNOT mint tokens. See [`jwks`].
//! - **Verified JWT — RS256 static ([`crate::config::Config::auth_jwt_public_key`] set):** the same,
//!   but against a single statically-configured PUBLIC key (no endpoint).
//! - **Verified JWT — HS256 ([`crate::config::Config::auth_jwt_secret`] set):** verified against a
//!   symmetric shared secret.
//!
//!   Every verified mode requires a valid, unexpired token (signature, required `exp`, `nbf`, and —
//!   when [`crate::config::Config::auth_jwt_audience`] is set — `aud`), PINS the exact algorithm
//!   (closing `alg: none` and the RS256->HS256 confusion attack), and takes identity from the
//!   token's signed claims. The `X-*` identity headers are IGNORED, so they cannot be spoofed — this
//!   is real caller authentication.
//! - **Trusted headers (none of the above set, the default):** identity is read verbatim from `X-*`
//!   headers set by an upstream gateway/mesh (the reference architecture's Policy & Access Gateway).
//!   Safe ONLY behind such a trusted hop.
//!
//! Whichever mode, the resolved [`Principal`] is identical downstream, so tenant resolution, RLS,
//! audit, and the [`policy`] gateway are unchanged.
//!
//! Optional (opt-in via [`crate::config::Config::auth_revocation_enabled`]): stateful per-subject
//! token revocation — a `(tenant, principal)` `not_before` cutoff rejects verified tokens issued
//! (`iat`) before it (revoke-all / "logout everywhere"), the mitigation for a compromised token
//! before its short `exp`. Enforced fail-closed in the extractor for the verified-JWT modes only.

pub mod policy;

mod jwks;
pub use jwks::JwksVerifier;

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::error::Error;
use crate::state::AppState;

/// The authenticated caller for a request.
///
/// Populated either from a verified JWT's claims (`sub`/`tenant`/`teams`/`role`) or,
/// in trusted-headers mode, from the corresponding `X-*` headers
/// (`X-Principal-Id` required; `X-Tenant-Id`, `X-Team-Ids` comma-separated, `X-Role`
/// optional). See the module docs for how the mode is chosen.
#[derive(Debug, Clone)]
pub struct Principal {
    pub principal_id: String,
    pub tenant_id: Option<String>,
    pub team_ids: Vec<String>,
    pub role: Option<String>,
    /// Whether `role` came from a CRYPTOGRAPHICALLY VERIFIED source (a signed JWT claim) rather than
    /// an unverified `X-Role` header. Only a verified role may assert the elevated `admin` role via
    /// the request (an unverified header can't) — see [`policy::Role::from_hint`]. `true` in verified
    /// -JWT mode, `false` in trusted-headers mode.
    pub role_verified: bool,
    /// The token's `iat` (issued-at, Unix seconds) in the verified-JWT modes; `None` in
    /// trusted-headers mode (no token) or when the token omits `iat`. Used only by per-subject
    /// revocation to reject a token issued before a `(tenant, principal)` cutoff.
    pub iat: Option<i64>,
}

impl Principal {
    /// The authenticated tenant (`X-Tenant-Id`), resolved fail-closed.
    ///
    /// A missing/blank tenant is `Unauthorized`, so a caller can never omit the
    /// header and act tenant-less. Use this directly for tenant-scoped operations
    /// whose request body carries **no** tenant to confirm (e.g. the skill
    /// registry, whose manifest has no `tenant_id` field) — the tenant is always
    /// the authenticated principal's, never anything from the body.
    pub fn authenticated_tenant(&self) -> crate::error::Result<&str> {
        self.tenant_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(Error::Unauthorized)
    }

    /// Fail-closed tenant resolution for a tenant-scoped operation whose body
    /// ALSO names a tenant.
    ///
    /// The RLS tenant is the **authenticated** tenant (`X-Tenant-Id`), never the
    /// request body — the body's `tenant_id` is only allowed to *confirm* it. A
    /// missing/blank authenticated tenant is `Unauthorized`; a body that names a
    /// different tenant is `Forbidden`. Returns the authenticated tenant to use
    /// for `db::tenant_tx`.
    pub fn require_tenant<'a>(&'a self, body_tenant: &str) -> crate::error::Result<&'a str> {
        let tenant = self.authenticated_tenant()?;
        if body_tenant.trim() != tenant {
            return Err(Error::Forbidden);
        }
        Ok(tenant)
    }
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Verified JWT claims. `exp` is a REQUIRED registered claim — [`Validation`]
/// defaults to requiring and validating it — so an expired or `exp`-less token is
/// rejected (fail-closed). `sub` is the principal id; the rest mirror the `X-*`
/// headers' meaning, but signed.
#[derive(Debug, Deserialize)]
struct Claims {
    /// Subject — the principal id (required).
    sub: String,
    /// Tenant id (custom claim), same role as `X-Tenant-Id`.
    #[serde(default)]
    tenant: Option<String>,
    /// Coarse role (custom claim), same vocabulary as `X-Role`.
    #[serde(default)]
    role: Option<String>,
    /// Team ids (custom claim). `Option<Vec<_>>` (not `Vec`) so a `null` or absent
    /// `teams` claim is treated as "no teams" instead of failing deserialization
    /// (which would 401 an otherwise-valid token).
    #[serde(default)]
    teams: Option<Vec<String>>,
    /// Issued-at (standard registered claim, seconds since the Unix epoch). Optional and NOT
    /// validated by `jsonwebtoken` — captured only so per-subject revocation can reject a token
    /// issued before a `(tenant, principal)` cutoff. Absent (`None`) is treated as "unknown issue
    /// time", which fails closed when a cutoff exists.
    #[serde(default)]
    iat: Option<i64>,
}

/// Extract the raw token from an `Authorization: Bearer <token>` header. The
/// scheme is matched case-insensitively per RFC 7235; a blank token is `None`.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Split a comma-separated header/claim value into trimmed, non-empty parts.
fn split_ids(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Trusted-headers mode: identity read verbatim from `X-*` headers.
fn principal_from_headers(headers: &HeaderMap) -> Result<Principal, Error> {
    // X-Principal-Id is mandatory; anything else is optional context.
    let principal_id = header_str(headers, "x-principal-id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(Error::Unauthorized)?;

    // Trim like `x-principal-id` (and the verified-JWT `tenant` claim) so the tenant is CANONICAL
    // from the start — every consumer (RLS, `authenticated_tenant`, revocation, rate limiting) then
    // sees the same normalized value instead of a raw header.
    let tenant_id = header_str(headers, "x-tenant-id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let team_ids = header_str(headers, "x-team-ids")
        .map(|s| split_ids(&s))
        .unwrap_or_default();
    let role = header_str(headers, "x-role");

    Ok(Principal {
        principal_id,
        tenant_id,
        team_ids,
        role,
        // Trusted-headers mode: `X-Role` is unverified, so it can never assert `admin`.
        role_verified: false,
        // No bearer token in headers mode, so there is no `iat` — revocation does not apply here.
        iat: None,
    })
}

/// A verifier for the caller's bearer token, chosen ONCE from config (built at startup so it is not
/// rebuilt per request). JWKS and static RS256 are ASYMMETRIC — the service holds only PUBLIC keys
/// and CANNOT mint tokens; HS256 is the symmetric shared-secret mode; [`JwtDecoder::None`] selects
/// trusted-headers.
pub enum JwtDecoder {
    /// Rotating asymmetric RS256 — keys fetched from a JWKS endpoint by `kid` (see [`JwksVerifier`]).
    Jwks(Arc<JwksVerifier>),
    /// Asymmetric RS256, verified against a single statically-configured public key.
    Rs256(DecodingKey),
    /// Symmetric HS256, verified against the shared secret.
    Hs256(DecodingKey),
    /// No JWT verification configured — trusted-upstream-headers mode.
    None,
}

impl JwtDecoder {
    /// Build the verifier from config in PRECEDENCE order: JWKS endpoint > static RS256 public key >
    /// HS256 secret > trusted-headers. The RS256 PEM and the JWKS URL were validated at
    /// [`crate::config::Config::from_env`], so building here is infallible (JWKS keys are fetched
    /// LAZILY on first use, not here).
    pub fn from_config(config: &crate::config::Config) -> Self {
        if let Some(url) = config.auth_jwks_url.as_deref() {
            JwtDecoder::Jwks(Arc::new(JwksVerifier::new(
                url.to_string(),
                config.auth_jwks_timeout_secs,
                config.auth_jwks_min_refetch_secs,
            )))
        } else if let Some(pem) = config.auth_jwt_public_key.as_deref() {
            JwtDecoder::Rs256(
                DecodingKey::from_rsa_pem(pem.as_bytes())
                    .expect("AUTH_JWT_PUBLIC_KEY validated at config load"),
            )
        } else if let Some(secret) = config.auth_jwt_secret.as_deref() {
            JwtDecoder::Hs256(DecodingKey::from_secret(secret.as_bytes()))
        } else {
            JwtDecoder::None
        }
    }
}

/// Verified-JWT mode: identity from a signed, unexpired bearer token verified with `key` under the
/// PINNED `algorithm` (an HS256 secret or an RS256 public key). The `X-*` identity headers are
/// intentionally NOT consulted, so a caller cannot spoof identity by setting them. `audience`, when
/// `Some`, is the expected `aud`.
fn principal_from_bearer(
    headers: &HeaderMap,
    key: &DecodingKey,
    algorithm: Algorithm,
    audience: Option<&str>,
    issuer: Option<&str>,
) -> Result<Principal, Error> {
    let token = bearer_token(headers).ok_or(Error::Unauthorized)?;
    verify_token(token, key, algorithm, audience, issuer)
}

/// Verify an already-extracted bearer `token` with `key` under the PINNED `algorithm`, returning the
/// [`Principal`] from its signed claims. Shared by the static HS256/RS256 modes and the JWKS mode
/// (which resolves `key` by `kid` first). `bearer_token`, `verify_token`, and [`Principal`] are the
/// JWKS module's only entry points into this one.
pub(super) fn verify_token(
    token: &str,
    key: &DecodingKey,
    algorithm: Algorithm,
    audience: Option<&str>,
    issuer: Option<&str>,
) -> Result<Principal, Error> {
    // Pin the algorithm to EXACTLY the configured one: this rejects `alg: none` and — critically for
    // RS256 — the RS256->HS256 confusion attack (an attacker signing with the PUBLIC key bytes as an
    // HMAC secret), since jsonwebtoken then only accepts that single algorithm. Validation also
    // requires + checks `exp` by default; enable `nbf` too so a not-yet-valid token is rejected.
    let mut validation = Validation::new(algorithm);
    validation.validate_nbf = true;
    let mut required_claims = vec!["exp"];
    match audience {
        Some(aud) => {
            validation.set_audience(&[aud]);
            required_claims.push("aud");
        }
        None => validation.validate_aud = false,
    }
    if let Some(iss) = issuer {
        validation.set_issuer(&[iss]);
        required_claims.push("iss");
    }
    validation.set_required_spec_claims(&required_claims);
    let claims = decode::<Claims>(token, key, &validation)
        .map_err(|_| Error::Unauthorized)?
        .claims;

    let principal_id = claims.sub.trim();
    if principal_id.is_empty() {
        return Err(Error::Unauthorized);
    }
    let clean = |s: String| {
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    Ok(Principal {
        principal_id: principal_id.to_string(),
        tenant_id: claims.tenant.and_then(clean),
        // The `teams` claim is already a structured array — take each id verbatim
        // (trim + drop blanks); do NOT comma-split it (that is only for the single
        // comma-joined `X-Team-Ids` header).
        team_ids: claims
            .teams
            .unwrap_or_default()
            .into_iter()
            .filter_map(clean)
            .collect(),
        role: claims.role.and_then(clean),
        // Verified-JWT mode: the `role` claim is signed, so it MAY assert `admin`.
        role_verified: true,
        // Carry the signed `iat` so per-subject revocation can compare it to the cutoff.
        iat: claims.iat,
    })
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        let audience = state.config.auth_jwt_audience.as_deref();
        let issuer = state.config.auth_jwt_issuer.as_deref();
        // The verifier was chosen once at startup (JWKS / static RS256 / HS256 / trusted-headers).
        // `verified` marks the cryptographically-verified modes (a bearer token was checked), vs
        // trusted-headers — it gates the token-specific revocation check below.
        let (principal, verified) = match &*state.jwt_decoder {
            // Rotating RS256: resolve the key by `kid` from the (lazily-fetched) JWKS, then verify.
            // This is the only async branch — it may await a throttled JWKS refetch.
            JwtDecoder::Jwks(verifier) => (
                verifier
                    .principal_from_bearer(headers, audience, issuer)
                    .await?,
                true,
            ),
            // Static verified modes: a valid signed token under the pinned algorithm is required.
            JwtDecoder::Rs256(key) => (
                principal_from_bearer(headers, key, Algorithm::RS256, audience, issuer)?,
                true,
            ),
            JwtDecoder::Hs256(key) => (
                principal_from_bearer(headers, key, Algorithm::HS256, audience, issuer)?,
                true,
            ),
            // Trusted-upstream mode: headers verbatim (no bearer token).
            JwtDecoder::None => (principal_from_headers(headers)?, false),
        };
        // Per-subject revocation applies only to VERIFIED tokens (a trusted-headers request carries no
        // token / `iat`). Opt-in; off by default — no lookup.
        if verified && state.config.auth_revocation_enabled {
            enforce_not_revoked(&state.db, &principal).await?;
        }
        // Per-tenant rate limiting applies to ALL authenticated requests. Opt-in; off by default — no
        // bucket lookup. Enforced here (after tenant resolution) so a throttled tenant is rejected 429
        // before any handler work.
        if state.config.rate_limit_enabled {
            crate::ratelimit::enforce(&state.db, &principal, &state.config).await?;
        }
        Ok(principal)
    }
}

/// Reject a verified token that has been revoked: a token issued no later than the principal's
/// `(tenant, principal)` `not_before` cutoff SECOND — or that carries no `iat` while a cutoff
/// exists — is [`Error::Unauthorized`]. Fail-closed at whole-second `iat` resolution (the cutoff
/// second is rejected), on a missing `iat`, and on a DB error; only the absence of a cutoff row
/// admits. A verified token with no tenant claim is out of scope (revocation is tenant-scoped) and
/// admitted here (tenant-scoped handlers reject it downstream anyway).
async fn enforce_not_revoked(db: &sqlx::PgPool, principal: &Principal) -> Result<(), Error> {
    // `verify_token` already trims the tenant claim + normalizes empty to `None` (the canonical trim
    // point), so no re-trim here; the non-empty guard is a cheap fail-closed backstop for this
    // security path should a blank tenant ever reach it.
    let Some(tenant) = principal.tenant_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    // Go through `tenant_tx` so RLS's `app_current_tenant_id()` is set: a direct pool query would
    // match ZERO rows under FORCE RLS and silently fail OPEN. `tenant_id` is ALSO bound explicitly
    // (composite-PK prefix / defense-in-depth).
    let mut tx = crate::db::tenant_tx(db, tenant).await?;
    let row: Option<(chrono::DateTime<chrono::Utc>,)> = sqlx::query_as(
        "SELECT not_before FROM revocations WHERE tenant_id = $1 AND principal_id = $2",
    )
    .bind(tenant)
    .bind(&principal.principal_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::Db)?;
    match row {
        // No cutoff for this principal — not revoked.
        None => Ok(()),
        // A cutoff exists: admit ONLY a token issued STRICTLY AFTER the cutoff second. `iat` has
        // whole-second resolution while `not_before` has sub-second precision, so `>=` would admit a
        // token issued in the SAME second as (but before) the cutoff — a fail-OPEN. Use `>` to reject
        // the entire cutoff second (fail-closed); a genuine re-auth lands in a later second and
        // passes. A token with no `iat` can't prove it post-dates the cutoff, so it too is rejected.
        Some((not_before,)) => match principal.iat {
            Some(iat) if iat > not_before.timestamp() => Ok(()),
            _ => Err(Error::Unauthorized),
        },
    }
}
