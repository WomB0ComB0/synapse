//! `POST /documents.ingest` handler.

use axum::extract::State;
use axum::Json;

use crate::api::idempotency;
use crate::audit;
use crate::auth::policy::{enforce, resolve_role_in_tx, Action, Role};
use crate::auth::Principal;
use crate::db::tenant_tx;
use crate::domain::{
    DocumentAclResponse, DocumentGrantRequest, DocumentIngestRequest, DocumentIngestResponse,
    DocumentReembedRequest, DocumentReembedResponse, DocumentRevokeRequest,
};
use crate::error::{Error, Result};
use crate::ingestion::{process_embedding_job, JobOutcome};
use crate::retrieval::{chunk, EMBEDDING_DIM};
use crate::state::AppState;
use uuid::Uuid;

/// Idempotency scope for `documents.ingest` in the generic key registry (migration 0024): the key is
/// the `doc_id` and the fingerprint is the FULL canonical request (content + metadata + owners + ACL),
/// so a re-ingest that changes ANY written field re-ingests and applies it — only a byte-identical
/// re-ingest replays.
const INGEST_SCOPE: &str = "documents.ingest";

/// Ingest a document durably: commit canonical text, metadata, ACLs, and
/// lexical chunks first, then attempt the generation-guarded embedding job.
/// Gemini failures leave a retryable job instead of losing the document write.
pub async fn ingest(
    State(state): State<AppState>,
    principal: Principal,
    Json(mut req): Json<DocumentIngestRequest>,
) -> Result<Json<DocumentIngestResponse>> {
    let doc = &req.document;
    let doc_id = doc.doc_id.trim();
    tracing::info!(
        principal = %principal.principal_id,
        doc_id,
        tenant = %doc.tenant_id,
        has_content = req.content.is_some(),
        "documents.ingest"
    );

    if doc_id.is_empty() {
        return Err(Error::BadRequest("doc_id is required".to_string()));
    }
    let tenant = principal.require_tenant(&doc.tenant_id)?;
    enforce(&state, &principal, tenant, Action::DocumentsIngest, doc_id).await?;

    let fingerprint = state
        .config
        .ingest_idempotency_enabled
        .then(|| idempotency::request_bytes(&req));
    let content = req.content.take().unwrap_or_default();

    if let Some(fp) = &fingerprint {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        let (matched, existing, ingestion_status): (bool, i64, String) = sqlx::query_as(
            "SELECT \
                 coalesce((SELECT request_fingerprint = encode(sha256($4), 'hex') \
                           FROM idempotency_keys \
                           WHERE tenant_id = $1 AND scope = $2 AND idempotency_key = $3), false), \
                 (SELECT count(*) FROM chunks WHERE tenant_id = $1 AND doc_id = $3), \
                 coalesce((SELECT ingestion_status FROM documents \
                           WHERE tenant_id = $1 AND doc_id = $3), 'ready')",
        )
        .bind(tenant)
        .bind(INGEST_SCOPE)
        .bind(doc_id)
        .bind(fp)
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::Db)?;
        if matched {
            let status = if ingestion_status == "ready" {
                "replayed"
            } else {
                "queued"
            };
            audit::record_best_effort(
                &state.db,
                tenant,
                Some(&principal.principal_id),
                "documents.ingest",
                doc_id,
                "success",
                serde_json::json!({
                    "chunks": existing,
                    "replayed": true,
                    "ingestion_status": ingestion_status
                }),
            )
            .await;
            return Ok(Json(DocumentIngestResponse {
                doc_id: doc_id.to_string(),
                status: status.to_string(),
                chunks_ingested: existing.max(0) as usize,
            }));
        }
    }

    let pieces = chunk::split(&content);
    let chunks = pieces.len();
    let acl = serde_json::to_value(&doc.acl).map_err(|e| Error::Internal(e.into()))?;
    let job_id = Uuid::new_v4();
    let model = state.config.embedding_model.clone();

    let write: Result<()> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        if let Some(fp) = &fingerprint {
            idempotency::record_fingerprint(&mut tx, tenant, INGEST_SCOPE, doc_id, fp).await?;
        }

        sqlx::query(
            r#"
        INSERT INTO documents
            (doc_id, tenant_id, team_scope, source_system, source_uri, title,
             content_type, language, version, owners, acl, metadata, content,
             content_sha256, ingestion_status, embedding_job_id, embedding_model,
             embedding_attempts, next_embedding_attempt_at, embedding_started_at,
             embedding_completed_at, embedding_last_error)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                encode(sha256(convert_to($13, 'UTF8')), 'hex'), 'pending', $14, $15,
                0, now(), NULL, NULL, NULL)
        ON CONFLICT (tenant_id, doc_id) DO UPDATE SET
            team_scope    = EXCLUDED.team_scope,
            source_system = EXCLUDED.source_system,
            source_uri    = EXCLUDED.source_uri,
            title         = EXCLUDED.title,
            content_type  = EXCLUDED.content_type,
            language      = EXCLUDED.language,
            version       = EXCLUDED.version,
            owners        = CASE WHEN cardinality(EXCLUDED.owners) = 0
                                 THEN documents.owners ELSE EXCLUDED.owners END,
            acl           = CASE WHEN EXCLUDED.acl -> 'users' = '[]'::jsonb
                                  AND EXCLUDED.acl -> 'groups' = '[]'::jsonb
                                 THEN documents.acl ELSE EXCLUDED.acl END,
            metadata      = EXCLUDED.metadata,
            content       = EXCLUDED.content,
            content_sha256 = EXCLUDED.content_sha256,
            ingestion_status = 'pending',
            embedding_job_id = EXCLUDED.embedding_job_id,
            embedding_model = EXCLUDED.embedding_model,
            embedding_attempts = 0,
            next_embedding_attempt_at = now(),
            embedding_started_at = NULL,
            embedding_completed_at = NULL,
            embedding_last_error = NULL
        "#,
        )
        .bind(doc_id)
        .bind(&doc.tenant_id)
        .bind(&doc.team_scope)
        .bind(&doc.source_system)
        .bind(&doc.source_uri)
        .bind(&doc.title)
        .bind(&doc.content_type)
        .bind(&doc.language)
        .bind(&doc.version)
        .bind(&doc.owners)
        .bind(&acl)
        .bind(&doc.metadata)
        .bind(&content)
        .bind(job_id)
        .bind(&model)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::db_or_conflict(e, "document already exists"))?;

        if !doc.acl.users.is_empty() || !doc.acl.groups.is_empty() {
            sqlx::query("DELETE FROM document_acl WHERE tenant_id = $1 AND doc_id = $2")
                .bind(&doc.tenant_id)
                .bind(doc_id)
                .execute(&mut *tx)
                .await?;
            let mut query = sqlx::QueryBuilder::new(
                "INSERT INTO document_acl \
                 (tenant_id, doc_id, grantee_type, grantee_id, permission) ",
            );
            let grants = doc
                .acl
                .users
                .iter()
                .map(|user| ("user", user))
                .chain(doc.acl.groups.iter().map(|group| ("group", group)));
            query.push_values(grants, |mut row, (grantee_type, grantee_id)| {
                row.push_bind(&doc.tenant_id)
                    .push_bind(doc_id)
                    .push_bind(grantee_type)
                    .push_bind(grantee_id)
                    .push_bind("read");
            });
            query.push(" ON CONFLICT DO NOTHING");
            query.build().execute(&mut *tx).await?;
        }

        sqlx::query("DELETE FROM chunks WHERE tenant_id = $1 AND doc_id = $2")
            .bind(&doc.tenant_id)
            .bind(doc_id)
            .execute(&mut *tx)
            .await?;

        if !pieces.is_empty() {
            let mut query = sqlx::QueryBuilder::new(
                "INSERT INTO chunks \
                 (chunk_id, doc_id, tenant_id, ordinal, section_path, text, \
                  char_start, char_end, embedding_model, embedding_dimensions, \
                  embedding, metadata) ",
            );
            query.push_values(pieces.iter(), |mut row, piece| {
                let chunk_id =
                    format!("{}::{}::chunk::{:04}", doc.tenant_id, doc_id, piece.ordinal);
                row.push_bind(chunk_id)
                    .push_bind(doc_id.to_string())
                    .push_bind(doc.tenant_id.clone())
                    .push_bind(piece.ordinal)
                    .push_bind(piece.section_path.clone())
                    .push_bind(piece.text.clone())
                    .push_bind(piece.char_start)
                    .push_bind(piece.char_end)
                    .push_bind(model.clone())
                    .push_bind(EMBEDDING_DIM as i32)
                    .push_bind(Option::<pgvector::Vector>::None)
                    .push_bind(serde_json::json!({}));
            });
            query.build().execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(())
    }
    .await;

    if let Err(error) = write {
        let (outcome, metadata) = error.audit_report();
        audit::record_best_effort(
            &state.db,
            tenant,
            Some(&principal.principal_id),
            "documents.ingest",
            doc_id,
            outcome,
            metadata,
        )
        .await;
        return Err(error);
    }

    let job_outcome = process_embedding_job(
        &state.db,
        state.embedder.as_ref(),
        tenant,
        doc_id,
        job_id,
        &model,
        state.config.worker_stale_secs,
    )
    .await;
    let (status, job_metadata) = match job_outcome {
        Ok(JobOutcome::Completed { chunks }) => (
            "ingested",
            serde_json::json!({"ingestion_status": "ready", "chunks": chunks}),
        ),
        Ok(JobOutcome::Deferred {
            status,
            attempts,
            retry_after_secs,
        }) => (
            if status == "failed" {
                "embedding_failed"
            } else {
                "queued"
            },
            serde_json::json!({
                "ingestion_status": status,
                "chunks": chunks,
                "attempts": attempts,
                "retry_after_secs": retry_after_secs
            }),
        ),
        Ok(JobOutcome::Superseded) => (
            "queued",
            serde_json::json!({"ingestion_status": "superseded", "chunks": chunks}),
        ),
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant,
                doc_id,
                %job_id,
                "immediate embedding attempt failed; durable job remains pending"
            );
            (
                "queued",
                serde_json::json!({
                    "ingestion_status": "pending",
                    "chunks": chunks,
                    "error_code": error.code()
                }),
            )
        }
    };

    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "documents.ingest",
        doc_id,
        "success",
        job_metadata,
    )
    .await;

    Ok(Json(DocumentIngestResponse {
        doc_id: doc_id.to_string(),
        status: status.to_string(),
        chunks_ingested: chunks,
    }))
}

/// Queue and attempt a fresh embedding generation for an existing document.
pub async fn reembed(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<DocumentReembedRequest>,
) -> Result<Json<DocumentReembedResponse>> {
    let doc_id = req.doc_id.trim();
    if doc_id.is_empty() {
        return Err(Error::BadRequest("doc_id is required".to_string()));
    }
    let tenant = principal.authenticated_tenant()?;
    enforce(&state, &principal, tenant, Action::DocumentsIngest, doc_id).await?;

    let job_id = Uuid::new_v4();
    let model = state.config.embedding_model.clone();
    let mut tx = tenant_tx(&state.db, tenant).await?;
    let updated = sqlx::query(
        "UPDATE documents SET \
             ingestion_status = 'pending', embedding_job_id = $3, \
             embedding_model = $4, embedding_attempts = 0, \
             next_embedding_attempt_at = now(), embedding_started_at = NULL, \
             embedding_completed_at = NULL, embedding_last_error = NULL \
         WHERE tenant_id = $1 AND doc_id = $2",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(job_id)
    .bind(&model)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(Error::NotFound("document not found".to_string()));
    }

    let chunks = sqlx::query(
        "UPDATE chunks SET embedding = NULL, embedding_model = $3, \
             embedding_dimensions = $4 \
         WHERE tenant_id = $1 AND doc_id = $2",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(&model)
    .bind(EMBEDDING_DIM as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected() as usize;
    tx.commit().await?;

    let outcome = process_embedding_job(
        &state.db,
        state.embedder.as_ref(),
        tenant,
        doc_id,
        job_id,
        &model,
        state.config.worker_stale_secs,
    )
    .await;
    let (status, metadata) = match outcome {
        Ok(JobOutcome::Completed { chunks }) => (
            "reembedded",
            serde_json::json!({"ingestion_status": "ready", "chunks": chunks}),
        ),
        Ok(JobOutcome::Deferred {
            status,
            attempts,
            retry_after_secs,
        }) => (
            if status == "failed" {
                "embedding_failed"
            } else {
                "queued"
            },
            serde_json::json!({
                "ingestion_status": status,
                "chunks": chunks,
                "attempts": attempts,
                "retry_after_secs": retry_after_secs
            }),
        ),
        Ok(JobOutcome::Superseded) => (
            "queued",
            serde_json::json!({"ingestion_status": "superseded", "chunks": chunks}),
        ),
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant,
                doc_id,
                %job_id,
                "immediate re-embedding attempt failed; durable job remains pending"
            );
            (
                "queued",
                serde_json::json!({
                    "ingestion_status": "pending",
                    "chunks": chunks,
                    "error_code": error.code()
                }),
            )
        }
    };

    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "documents.reembed",
        doc_id,
        "success",
        metadata,
    )
    .await;

    Ok(Json(DocumentReembedResponse {
        doc_id: doc_id.to_string(),
        status: status.to_string(),
        chunks_queued: chunks,
    }))
}

/// Validate the `grantee_type` / optional `permission` of an ACL request. The DB
/// CHECKs would also reject bad values, but as a raw 23514 (→ 500); validating here
/// gives a clean 400.
fn validate_grantee(grantee_type: &str) -> Result<()> {
    if !matches!(grantee_type, "user" | "group") {
        return Err(Error::BadRequest(
            "grantee_type must be 'user' or 'group'".into(),
        ));
    }
    Ok(())
}

fn validate_permission(permission: &str) -> Result<()> {
    if !matches!(permission, "read" | "write" | "admin") {
        return Err(Error::BadRequest(
            "permission must be 'read', 'write', or 'admin'".into(),
        ));
    }
    Ok(())
}

/// Ownership gate for ACL management. A document is manageable when the caller is an **owner**, OR
/// the document is *truly public* (no owners AND no grants — any RBAC-admitted caller may add the
/// first grant), OR the caller is an **admin AND the document is UNOWNED** (`owners = {}`). That last
/// clause lets an admin govern an owner-less document — including an owner-less-but-group-restricted
/// one that previously had NO manager via this path (fail-closed, only resolvable by re-ingesting
/// with owners). An admin deliberately may NOT seize an **owned** document's ACL: ownership is
/// respected, so admin power here is scoped to UNOWNED docs (matching the pillar).
///
/// This mirrors the retrieval readability test, so a non-admin still can't self-grant onto an
/// owner-less, group-restricted doc. Critically, a document the caller may NOT manage returns the
/// SAME [`Error::NotFound`] as a missing one, so a non-owner learns nothing about the document's
/// existence (no same-tenant oracle).
///
/// This is the fence that keeps `documents.grant`/`revoke` from being a self-grant bypass of the
/// read ACL: without it, any Member (the default role) could grant themselves read on any
/// owner-restricted doc and immediately retrieve it. Runs on the caller's transaction so the check
/// and the ACL mutation are atomic.
async fn assert_can_manage_acl(
    conn: &mut sqlx::PgConnection,
    tenant: &str,
    doc_id: &str,
    caller: &str,
    is_admin: bool,
) -> Result<()> {
    // Also fetch whether the doc carries ANY document_acl grant. A doc is "tenant-public"
    // — freely manageable by any RBAC-admitted caller — ONLY when it has no owners AND no
    // grants, mirroring the retrieval readability test (hybrid.rs). A doc RESTRICTED by
    // grants but with no owners is NOT freely self-granted (that was a read-ACL bypass —
    // any Member could grant themselves onto an owner-less, group-restricted doc and read
    // it), BUT an admin may manage it: an UNOWNED doc (owners = {}) is admin-governable
    // regardless of grants, so an orphaned/owner-less doc always has a manager.
    let row: Option<(Vec<String>, bool)> = sqlx::query_as(
        "SELECT owners, \
                EXISTS (SELECT 1 FROM document_acl WHERE tenant_id = $1 AND doc_id = $2) \
         FROM documents WHERE tenant_id = $1 AND doc_id = $2",
    )
    .bind(tenant)
    .bind(doc_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        // The caller owns the doc; OR the doc is UNOWNED and either truly public (no grants) or the
        // caller is an admin (admins govern any owner-less doc, but never seize an OWNED one).
        Some((owners, has_grants))
            if owners.iter().any(|o| o == caller)
                || (owners.is_empty() && (!has_grants || is_admin)) =>
        {
            Ok(())
        }
        // Missing doc OR a restricted doc the caller can't manage: identical NotFound (no oracle).
        _ => Err(Error::NotFound(format!("document '{doc_id}'"))),
    }
}

/// Grant a document ACL to a user or group. Idempotent (an existing grant is a
/// no-op). Ownership-scoped: an **owner** may grant on the document, and an **admin** may grant on
/// any **unowned** document (so an orphaned/owner-less doc is always governable); a non-owner — like
/// a missing document — gets a 404, so grant can't be used to self-grant read nor to probe document
/// existence. Tenant-isolated + audited; a Viewer is denied.
pub async fn grant(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<DocumentGrantRequest>,
) -> Result<Json<DocumentAclResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let doc_id = req.doc_id.trim();
    let grantee_type = req.grantee_type.trim();
    let grantee_id = req.grantee_id.trim();
    let permission = req
        .permission
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("read");
    if doc_id.is_empty() || grantee_id.is_empty() {
        return Err(Error::BadRequest(
            "doc_id and grantee_id are required".into(),
        ));
    }
    validate_grantee(grantee_type)?;
    validate_permission(permission)?;
    enforce(&state, &principal, tenant, Action::DocumentsGrant, doc_id).await?;

    let write: Result<()> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        // Ownership-scoped: an owner (or an admin, for an UNOWNED doc) may manage the ACL; a
        // non-owner gets the same 404 as a missing doc. Also subsumes the doc-existence check the
        // composite FK would otherwise surface as an opaque 400. The role is DB-authoritative (or a
        // verified-JWT admin), never a plaintext X-Role header.
        let is_admin = resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            == Role::Admin;
        assert_can_manage_acl(&mut tx, tenant, doc_id, &principal.principal_id, is_admin).await?;
        sqlx::query(
            "INSERT INTO document_acl (tenant_id, doc_id, grantee_type, grantee_id, permission) \
                 VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT DO NOTHING",
        )
        .bind(tenant)
        .bind(doc_id)
        .bind(grantee_type)
        .bind(grantee_id)
        .bind(permission)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(()) => (
            "success",
            serde_json::json!({ "grantee_type": grantee_type, "grantee_id": grantee_id, "permission": permission }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "documents.grant",
        doc_id,
        outcome,
        metadata,
    )
    .await;
    write?;

    Ok(Json(DocumentAclResponse {
        doc_id: doc_id.to_string(),
        grantee_type: grantee_type.to_string(),
        grantee_id: grantee_id.to_string(),
        permission: permission.to_string(),
        status: "granted".to_string(),
    }))
}

/// Revoke a document ACL grant. Omitting `permission` revokes EVERY permission for
/// the grantee on the document. Idempotent (revoking a non-existent grant is a 200
/// no-op). Ownership-scoped like [`grant`]: an **owner** may revoke on an owned doc, and an
/// **admin** on any **unowned** doc; a non-owner or missing doc → 404. Tenant-isolated + audited; a
/// Viewer is denied.
pub async fn revoke(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<DocumentRevokeRequest>,
) -> Result<Json<DocumentAclResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let doc_id = req.doc_id.trim();
    let grantee_type = req.grantee_type.trim();
    let grantee_id = req.grantee_id.trim();
    let permission = req
        .permission
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if doc_id.is_empty() || grantee_id.is_empty() {
        return Err(Error::BadRequest(
            "doc_id and grantee_id are required".into(),
        ));
    }
    validate_grantee(grantee_type)?;
    if let Some(p) = permission {
        validate_permission(p)?;
    }
    enforce(&state, &principal, tenant, Action::DocumentsRevoke, doc_id).await?;

    let write: Result<u64> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        // Ownership-scoped, same as grant: an owner (or an admin, for an UNOWNED doc) may revoke;
        // a non-owner/missing doc → the same 404 (no existence oracle).
        let is_admin = resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            == Role::Admin;
        assert_can_manage_acl(&mut tx, tenant, doc_id, &principal.principal_id, is_admin).await?;
        // Delete the specific permission, or (permission omitted) every grant for the
        // grantee. `doc_id` is tenant-scoped, so tenant_id is part of every predicate.
        let r = match permission {
            Some(p) => {
                sqlx::query(
                    "DELETE FROM document_acl WHERE tenant_id = $1 AND doc_id = $2 \
                       AND grantee_type = $3 AND grantee_id = $4 AND permission = $5",
                )
                .bind(tenant)
                .bind(doc_id)
                .bind(grantee_type)
                .bind(grantee_id)
                .bind(p)
                .execute(&mut *tx)
                .await?
            }
            None => {
                sqlx::query(
                    "DELETE FROM document_acl WHERE tenant_id = $1 AND doc_id = $2 \
                       AND grantee_type = $3 AND grantee_id = $4",
                )
                .bind(tenant)
                .bind(doc_id)
                .bind(grantee_type)
                .bind(grantee_id)
                .execute(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(r.rows_affected())
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(removed) => (
            "success",
            serde_json::json!({ "grantee_type": grantee_type, "grantee_id": grantee_id, "permission": permission, "removed": removed }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "documents.revoke",
        doc_id,
        outcome,
        metadata,
    )
    .await;
    write?;

    Ok(Json(DocumentAclResponse {
        doc_id: doc_id.to_string(),
        grantee_type: grantee_type.to_string(),
        grantee_id: grantee_id.to_string(),
        permission: permission.unwrap_or("*").to_string(),
        status: "revoked".to_string(),
    }))
}
