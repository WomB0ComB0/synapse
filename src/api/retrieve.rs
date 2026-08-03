//! `POST /retrieve` handler.

use axum::extract::State;
use axum::Json;
use uuid::Uuid;

use crate::audit;
use crate::auth::policy::{enforce, Action};
use crate::auth::Principal;
use crate::db::tenant_tx;
use crate::domain::{RetrievalMode, RetrieveRequest, RetrieveResponse};
use crate::error::{Error, Result};
use crate::retrieval::embed::Embedder;
use crate::retrieval::hybrid;
use crate::state::AppState;

/// Permission-aware hybrid retrieval.
///
/// Embeds the query when a vector arm is needed, then runs the requested arms (`hybrid` | `vector` |
/// `lexical`) inside a tenant-scoped transaction so Row-Level Security isolates
/// the tenant. Lexical-only retrieval never calls the embedder, and hybrid retrieval degrades to
/// lexical when query embedding is temporarily unavailable. Team/project scope and `top_k` are honored in fully parameterized
/// SQL; candidates are fused with RRF and (optionally) heuristically reranked. By
/// default results are returned sorted by fused score desc; when `retrieval.mmr`
/// is set they are returned in MMR diversity-selection order instead — so a more
/// diverse chunk can precede a higher-scored near-duplicate and the reported
/// `score` (the fused score) is non-monotonic across positions.
pub async fn retrieve(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<RetrieveRequest>,
) -> Result<Json<RetrieveResponse>> {
    let trace_id = Uuid::new_v4().to_string();
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %req.tenant_id,
        mode = ?req.retrieval.mode,
        top_k = req.retrieval.top_k,
        trace_id = %trace_id,
        "retrieve"
    );

    // Fail closed: the RLS tenant is the AUTHENTICATED tenant (the body's
    // tenant_id may only confirm it). Missing X-Tenant-Id -> 401; mismatch -> 403.
    let tenant = principal.require_tenant(&req.tenant_id)?;

    // Reject a blank query before the policy/DB round-trip — validate cheap request
    // SYNTAX before authorizing, the same order the other governed reads already use
    // (`context.get` / `skills.get` reject an empty id before `enforce`). This also
    // keeps the DB-free smoke test DB-free now that `enforce` hits the DB. NOTE: a
    // tenant CAN revoke `retrieve` from a role via role_permissions, so a malformed
    // request from a would-be-denied caller returns 400 (malformed) rather than a
    // 403 + deny audit; that is the accepted cost of syntax-before-policy, uniform
    // across the read handlers (a 400 leaks nothing and grants no access).
    if req.query.trim().is_empty() {
        return Err(Error::BadRequest("query is required".into()));
    }

    enforce(&state, &principal, tenant, Action::Retrieve, &trace_id).await?;

    let mut effective_req = req.clone();
    let mut degraded_to_lexical = false;

    // Embed the query with the same process-wide embedder used at ingest, but only when a vector arm
    // is actually requested. A transient Gemini/OpenAI outage should not break lexical retrieval, and
    // a hybrid query can still return ACL-filtered lexical hits. Ingest still fails closed on embedder
    // failure, because writing mock/fallback vectors into a real index would corrupt future ranking.
    let needs_vector = matches!(
        effective_req.retrieval.mode,
        RetrievalMode::Hybrid | RetrievalMode::Vector
    );
    let query_embedding = if needs_vector {
        let embedder = state.embedder.as_ref();
        match embedder.embed(std::slice::from_ref(&req.query)).await {
            Ok(vectors) => vectors
                .into_iter()
                .next()
                .ok_or_else(|| Error::Internal(anyhow::anyhow!("embedder returned no vector")))?,
            Err(e) if effective_req.retrieval.mode == RetrievalMode::Hybrid => {
                tracing::warn!(
                    error = %e,
                    trace_id = %trace_id,
                    "query embedding failed; degrading hybrid retrieval to lexical"
                );
                effective_req.retrieval.mode = RetrievalMode::Lexical;
                degraded_to_lexical = true;
                Vec::new()
            }
            Err(e) => return Err(e),
        }
    } else {
        Vec::new()
    };

    // Opt-in embedding-model consistency: when enabled, the vector arm only compares chunks
    // embedded by the SAME model as this query (the query is embedded by the process-wide model),
    // so a model change can't silently cosine-compare across incompatible spaces. It is inert on
    // lexical-only/degraded retrieval.
    let model_filter = state
        .config
        .embedding_model_consistency
        .then_some(())
        .filter(|_| effective_req.retrieval.mode != RetrievalMode::Lexical)
        .map(|_| state.config.embedding_model.as_str());

    // Capture the search as a Result so both success and failure are audited.
    let search: Result<Vec<_>> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        // The document ACL is keyed on the AUTHENTICATED caller; group membership is
        // resolved from `team_members` inside the query (NOT the self-asserted
        // X-Team-Ids / request scope, which is only a narrowing fence).
        let results = hybrid::search(
            &mut tx,
            &effective_req,
            &query_embedding,
            &principal.principal_id,
            model_filter,
            state.config.retrieval_mmr_lambda,
        )
        .await?;
        tx.commit().await?;
        Ok(results)
    }
    .await;

    let (outcome, metadata) = match &search {
        Ok(results) => (
            "success",
            serde_json::json!({
                "requested_mode": format!("{:?}", req.retrieval.mode),
                "mode": format!("{:?}", effective_req.retrieval.mode),
                "degraded_to_lexical": degraded_to_lexical,
                "results": results.len()
            }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "retrieve",
        &trace_id,
        outcome,
        metadata,
    )
    .await;
    let results = search?;

    Ok(Json(RetrieveResponse { results, trace_id }))
}
