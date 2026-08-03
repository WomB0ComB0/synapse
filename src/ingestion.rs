//! Durable document embedding jobs.
//!
//! Document metadata, canonical text, ACLs, and lexical chunks are committed
//! before any provider call. Embedding is idempotent and generation-guarded by
//! `documents.embedding_job_id`, so a crash or Gemini outage can be retried
//! without losing the write or letting stale work overwrite a newer re-ingest.

use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::db::tenant_tx;
use crate::error::{Error, Result};
use crate::retrieval::embed::{Embedder, EmbedderImpl};
use crate::retrieval::EMBEDDING_DIM;

/// Result of one embedding-job attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// The current generation was embedded and finalized.
    Completed { chunks: usize },
    /// The provider failed; the job is queued for retry or exhausted.
    Deferred {
        status: String,
        attempts: i32,
        retry_after_secs: Option<u64>,
    },
    /// A newer re-ingest replaced this generation before it could be leased/finalized.
    Superseded,
}

/// Try one embedding job. Provider failures are persisted as retry state and
/// returned as [`JobOutcome::Deferred`]; database/invariant failures remain hard errors.
pub async fn process_embedding_job(
    db: &sqlx::PgPool,
    embedder: &EmbedderImpl,
    tenant: &str,
    doc_id: &str,
    job_id: Uuid,
    target_model: &str,
    stale_secs: i64,
) -> Result<JobOutcome> {
    let mut tx = tenant_tx(db, tenant).await?;
    let leased: Option<(i32, i32)> = sqlx::query_as(
        "UPDATE documents SET \
             ingestion_status = 'processing', \
             embedding_attempts = embedding_attempts + 1, \
             embedding_started_at = now(), \
             next_embedding_attempt_at = NULL, \
             embedding_last_error = NULL \
         WHERE tenant_id = $1 AND doc_id = $2 AND embedding_job_id = $3 \
           AND embedding_model = $4 \
           AND (ingestion_status = 'pending' \
             OR (ingestion_status = 'retry' \
                 AND coalesce(next_embedding_attempt_at, now()) <= now()) \
             OR (ingestion_status = 'processing' \
                 AND embedding_started_at < now() - make_interval(secs => $5))) \
         RETURNING embedding_attempts, embedding_max_attempts",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(job_id)
    .bind(target_model)
    .bind(stale_secs)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((attempts, max_attempts)) = leased else {
        return Ok(JobOutcome::Superseded);
    };

    let chunks: Vec<(String, String)> = sqlx::query_as(
        "SELECT chunk_id, text FROM chunks \
         WHERE tenant_id = $1 AND doc_id = $2 ORDER BY ordinal",
    )
    .bind(tenant)
    .bind(doc_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    if chunks.is_empty() {
        return finalize_embedding_job(
            db,
            tenant,
            doc_id,
            job_id,
            target_model,
            &chunks,
            Vec::new(),
        )
        .await;
    }

    let texts: Vec<String> = chunks.iter().map(|(_, text)| text.clone()).collect();
    let embeddings = match embedder.embed(&texts).await {
        Ok(embeddings) => embeddings,
        Err(error) => {
            tracing::warn!(
                tenant,
                doc_id,
                %job_id,
                attempts,
                error_code = error.code(),
                "embedding job deferred"
            );
            return defer_embedding_job(
                db,
                tenant,
                doc_id,
                job_id,
                attempts,
                max_attempts,
                embedding_error_message(&error),
            )
            .await;
        }
    };

    finalize_embedding_job(
        db,
        tenant,
        doc_id,
        job_id,
        target_model,
        &chunks,
        embeddings,
    )
    .await
}

async fn finalize_embedding_job(
    db: &sqlx::PgPool,
    tenant: &str,
    doc_id: &str,
    job_id: Uuid,
    target_model: &str,
    chunks: &[(String, String)],
    embeddings: Vec<Vec<f32>>,
) -> Result<JobOutcome> {
    if chunks.len() != embeddings.len() {
        return Err(Error::Internal(anyhow::anyhow!(
            "embedding job returned {} vectors for {} chunks",
            embeddings.len(),
            chunks.len()
        )));
    }

    let mut tx = tenant_tx(db, tenant).await?;
    let current: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM documents \
         WHERE tenant_id = $1 AND doc_id = $2 AND embedding_job_id = $3 \
           AND embedding_model = $4 AND ingestion_status = 'processing' \
         FOR UPDATE",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(job_id)
    .bind(target_model)
    .fetch_optional(&mut *tx)
    .await?;
    if current.is_none() {
        return Ok(JobOutcome::Superseded);
    }

    if !chunks.is_empty() {
        let mut query = QueryBuilder::<Postgres>::new(
            "UPDATE chunks AS c SET \
             embedding = values.embedding, \
             embedding_model = ",
        );
        query
            .push_bind(target_model)
            .push(", embedding_dimensions = ")
            .push_bind(EMBEDDING_DIM as i32)
            .push(" FROM (");
        query.push_values(
            chunks.iter().zip(embeddings.into_iter()),
            |mut row, ((chunk_id, _), embedding)| {
                row.push_bind(chunk_id)
                    .push_bind(pgvector::Vector::from(embedding));
            },
        );
        query.push(
            ") AS values(chunk_id, embedding) \
             WHERE c.tenant_id = ",
        );
        query
            .push_bind(tenant)
            .push(" AND c.doc_id = ")
            .push_bind(doc_id)
            .push(" AND c.chunk_id = values.chunk_id");
        query.build().execute(&mut *tx).await?;
    }

    let finalized = sqlx::query(
        "UPDATE documents SET \
             ingestion_status = 'ready', \
             next_embedding_attempt_at = NULL, \
             embedding_started_at = NULL, \
             embedding_completed_at = now(), \
             embedding_last_error = NULL \
         WHERE tenant_id = $1 AND doc_id = $2 AND embedding_job_id = $3",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    if finalized.rows_affected() != 1 {
        return Ok(JobOutcome::Superseded);
    }
    tx.commit().await?;

    Ok(JobOutcome::Completed {
        chunks: chunks.len(),
    })
}

async fn defer_embedding_job(
    db: &sqlx::PgPool,
    tenant: &str,
    doc_id: &str,
    job_id: Uuid,
    attempts: i32,
    max_attempts: i32,
    error_message: String,
) -> Result<JobOutcome> {
    let exhausted = attempts >= max_attempts;
    let retry_after_secs = (!exhausted).then(|| retry_delay_secs(attempts));
    let status = if exhausted { "failed" } else { "retry" };

    let mut tx = tenant_tx(db, tenant).await?;
    let updated = sqlx::query(
        "UPDATE documents SET \
             ingestion_status = $4, \
             next_embedding_attempt_at = CASE WHEN $4 = 'retry' \
                 THEN now() + make_interval(secs => $5) ELSE NULL END, \
             embedding_started_at = NULL, \
             embedding_last_error = $6 \
         WHERE tenant_id = $1 AND doc_id = $2 AND embedding_job_id = $3 \
           AND ingestion_status = 'processing'",
    )
    .bind(tenant)
    .bind(doc_id)
    .bind(job_id)
    .bind(status)
    .bind(retry_after_secs.unwrap_or_default() as i64)
    .bind(error_message)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Ok(JobOutcome::Superseded);
    }
    tx.commit().await?;

    Ok(JobOutcome::Deferred {
        status: status.to_string(),
        attempts,
        retry_after_secs,
    })
}

fn retry_delay_secs(attempts: i32) -> u64 {
    let exponent = attempts.clamp(0, 12) as u32;
    2_u64.saturating_pow(exponent).min(3600)
}

fn embedding_error_message(error: &Error) -> String {
    match error {
        Error::Upstream(_) => "embedding provider unavailable or rejected the request".to_string(),
        _ => format!("embedding attempt failed ({})", error.code()),
    }
}

/// Discover and process due embedding jobs across tenants. Discovery is a
/// read-only SECURITY DEFINER function; every lease/finalize runs under RLS.
pub async fn reconcile_embedding_jobs(
    db: &sqlx::PgPool,
    embedder: &EmbedderImpl,
    target_model: &str,
    stale_secs: i64,
) -> Result<usize> {
    let due: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT tenant_id, doc_id, embedding_job_id \
         FROM synapse_list_due_embedding_jobs($1, $2)",
    )
    .bind(stale_secs)
    .bind(target_model)
    .fetch_all(db)
    .await?;

    let mut processed = 0usize;
    for (tenant, doc_id, job_id) in due {
        match process_embedding_job(
            db,
            embedder,
            &tenant,
            &doc_id,
            job_id,
            target_model,
            stale_secs,
        )
        .await
        {
            Ok(JobOutcome::Superseded) => {}
            Ok(outcome) => {
                processed += 1;
                tracing::info!(tenant, doc_id, %job_id, ?outcome, "embedding job processed");
            }
            Err(error) => {
                return Err(Error::Internal(anyhow::anyhow!(
                    "embedding job {job_id} for tenant {tenant} document {doc_id} failed: {error:?}"
                )));
            }
        }
    }
    Ok(processed)
}

#[cfg(test)]
mod tests {
    use super::retry_delay_secs;

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay_secs(1), 2);
        assert_eq!(retry_delay_secs(2), 4);
        assert_eq!(retry_delay_secs(10), 1024);
        assert_eq!(retry_delay_secs(20), 3600);
    }
}
