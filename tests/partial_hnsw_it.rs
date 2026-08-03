//! Live integration test: the per-model PARTIAL HNSW index (migration 0022) is usable by, and
//! preferred for, the exact predicate the vector arm now emits.
//!
//! The model-consistency filter (#35) restricts the vector arm to the current model. Over a mixed
//! corpus, a filtered ANN on the single FULL HNSW graph wastes its search budget on other models'
//! neighbours (the "filtered-HNSW recall cliff"). Migration 0022 adds a partial HNSW index whose
//! graph holds only the default model's vectors, and the vector arm now INLINES the model as a
//! validated literal so the planner can pick it (a bound `= $n` under a generic plan cannot match a
//! partial index's literal predicate).
//!
//! We assert the planner contract against the REAL migrations:
//!   * inlined-literal model predicate -> the PARTIAL index `idx_chunks_embedding_hnsw_default`
//!   * no model predicate               -> the FULL index `idx_chunks_embedding_hnsw`
//!
//! To keep the plan deterministic we disable `enable_seqscan` + `enable_sort`, which forces
//! index-provided vector ordering (only an HNSW index supplies it) and isolates the partial-vs-full
//! choice from the orthogonal "seq-scan is cheaper at tiny scale" cost noise. In production the
//! partial index also wins *naturally* in the recall-cliff regime — the current model being a
//! minority of a large mixed corpus — which is exactly when it matters. The unfiltered case (b) is
//! structural, not just cost-based: the partial index omits other models' rows, so it CANNOT serve
//! an unfiltered query and the full index must be used.
//!
//! Filter correctness (returns only same-model chunks) is covered by `model_consistency_it.rs`.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5472:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5472/postgres
//! cargo test --test partial_hnsw_it -- --nocapture
//! ```

mod common;

use common::{apply_schema, TestDb};
use sqlx::postgres::PgPoolOptions;

/// A constant 1536-dim query vector, built in SQL so the test needs no `pgvector` dependency.
const QVEC: &str =
    "(('[' || array_to_string(array_fill(0.1::real, ARRAY[1536]), ',') || ']')::vector)";

#[tokio::test]
async fn model_filtered_knn_uses_the_partial_hnsw_index() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };

    let test_db = TestDb::create(&base_url, "partialhnsw").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;

    // A parent document for the FK, then a MIXED-model corpus: 400 current-model chunks (the model
    // with the partial index) + 6000 legacy-model chunks, with random embeddings. The current model
    // is a DECISIVE minority so the partial index (400 rows) is unambiguously cheaper than the full
    // index (6400 rows) for the filtered query — a smaller gap lets the randomized HNSW build
    // occasionally tip the planner to the full index + a post-filter (a flaky tie). (admin bypasses
    // RLS, so these raw inserts are fine.)
    sqlx::query("INSERT INTO documents (tenant_id, doc_id) VALUES ('tenant_a','d1')")
        .execute(&admin)
        .await
        .expect("seed document");
    sqlx::query(
        "INSERT INTO chunks (tenant_id, doc_id, chunk_id, ordinal, text, embedding_model, embedding)
         SELECT 'tenant_a','d1','s'||g, g, 'x', 'text-embedding-3-small',
                (SELECT array_agg(random())::vector(1536) FROM generate_series(1,1536))
         FROM generate_series(1,400) g",
    )
    .execute(&admin)
    .await
    .expect("seed current-model chunks");
    sqlx::query(
        "INSERT INTO chunks (tenant_id, doc_id, chunk_id, ordinal, text, embedding_model, embedding)
         SELECT 'tenant_a','d1','l'||g, 400+g, 'x', 'text-embedding-3-large',
                (SELECT array_agg(random())::vector(1536) FROM generate_series(1,1536))
         FROM generate_series(1,6000) g",
    )
    .execute(&admin)
    .await
    .expect("seed legacy-model chunks");
    sqlx::query("ANALYZE chunks")
        .execute(&admin)
        .await
        .expect("analyze");

    // Pin ONE connection so the planner GUCs apply to the EXPLAINs that follow.
    let mut conn = admin.acquire().await.expect("acquire");
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .expect("disable seqscan");
    sqlx::query("SET enable_sort = off")
        .execute(&mut *conn)
        .await
        .expect("disable sort");

    async fn plan(conn: &mut sqlx::PgConnection, query: &str) -> String {
        sqlx::query_scalar::<_, String>(&format!("EXPLAIN (COSTS OFF) {query}"))
            .fetch_all(conn)
            .await
            .expect("EXPLAIN")
            .join("\n")
    }

    // (a) Inlined-literal model predicate — exactly what the vector arm emits when the consistency
    //     filter is on — must use the PARTIAL index.
    let filtered = plan(
        &mut conn,
        &format!(
            "SELECT c.chunk_id FROM chunks c
             WHERE c.tenant_id = 'tenant_a'
               AND c.embedding IS NOT NULL
               AND c.embedding_model = 'text-embedding-3-small'
             ORDER BY c.embedding <=> {QVEC}
             LIMIT 50"
        ),
    )
    .await;
    assert!(
        filtered.contains("idx_chunks_embedding_hnsw_default"),
        "filtered KNN should use the PARTIAL index; plan was:\n{filtered}"
    );
    println!("(a) inlined-literal model predicate uses the partial index idx_chunks_embedding_hnsw_default");

    // (b) No model predicate (consistency off) — must use the FULL index, NOT the partial one (the
    //     partial index omits legacy-model rows, so it cannot serve an unfiltered query).
    let unfiltered = plan(
        &mut conn,
        &format!(
            "SELECT c.chunk_id FROM chunks c
             WHERE c.tenant_id = 'tenant_a'
               AND c.embedding IS NOT NULL
             ORDER BY c.embedding <=> {QVEC}
             LIMIT 50"
        ),
    )
    .await;
    assert!(
        unfiltered.contains("idx_chunks_embedding_hnsw") && !unfiltered.contains("_default"),
        "unfiltered KNN should use the FULL index (not the partial one); plan was:\n{unfiltered}"
    );
    println!("(b) no model predicate uses the full index idx_chunks_embedding_hnsw");

    drop(conn);
    admin.close().await;
    println!("partial HNSW: the model-filtered vector arm uses the per-model partial index (recall-preserving); the unfiltered path uses the full index.");
}
