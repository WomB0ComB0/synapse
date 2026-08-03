//! Hybrid retriever: dense (pgvector) + lexical (Postgres FTS) fused with RRF.
//!
//! All queries here run **inside** a tenant-scoped transaction (see
//! [`crate::db::tenant_tx`]): Row-Level Security constrains every arm to the
//! caller's tenant, and the SQL is fully parameterized (query vector, scope
//! arrays, language, and text are all bound — never interpolated). The
//! chunk→document joins additionally key on `tenant_id` (`d.tenant_id =
//! c.tenant_id AND d.doc_id = c.doc_id`): defense-in-depth beyond RLS and the
//! correct relational mapping now that `doc_id` is unique only *within* a tenant.
//!
//! Pipeline:
//! 1. **vector** arm — `ORDER BY embedding <=> $qvec` (cosine distance);
//! 2. **lexical** arm — `tsv @@ plainto_tsquery($lang, $q) ORDER BY ts_rank`;
//! 3. **fuse** with Reciprocal Rank Fusion (RRF, `k = 60`) over each arm's
//!    `row_number()` rank, then either (a) sort by fused score desc + truncate to
//!    `top_k`, or (b) when the request opts into `mmr`, select `top_k` via MMR
//!    diversity re-ranking over the over-fetched pool (see [`mmr`]).
//!
//! `retrieval.mode` selects which arms run: `hybrid` (both), `vector`, or
//! `lexical`. An optional heuristic `rerank` (recency/authority) and MMR diversity
//! both operate on the over-fetched candidate pool before the final `top_k` cut.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgConnection;

use crate::domain::{RetrievalMode, RetrievalResult, RetrieveRequest};
use crate::error::Result;

/// RRF constant; dampens the contribution of low-ranked items (standard = 60).
const RRF_K: f64 = 60.0;

/// Postgres text-search configuration. Must match the `to_tsvector('english', …)`
/// used to generate the `chunks.tsv` column in migration `0005`.
const TS_LANG: &str = "english";

/// One row returned by an arm query, carrying its per-arm rank + the rerank signals.
#[derive(sqlx::FromRow)]
struct ArmRow {
    chunk_id: String,
    doc_id: String,
    text: String,
    source_uri: Option<String>,
    metadata: Value,
    rank: i64,
    /// Age of the chunk in seconds (`now() - created_at`) — the recency signal for rerank.
    age_secs: f64,
    /// Whether the chunk's document is OWNED (curated) vs tenant-public — the authority signal.
    authoritative: bool,
    /// The chunk's embedding, materialized ONLY when MMR is requested (else the SELECT projects
    /// `NULL::vector`, so the non-MMR path transfers no vectors). `None` also for a chunk that has no
    /// embedding (a lexical-arm hit that was never embedded) — MMR then applies no diversity penalty.
    embedding: Option<pgvector::Vector>,
}

/// Run permission-aware retrieval for `req` using the pre-computed query vector.
///
/// `conn` must be a connection with the tenant GUC already set (i.e. obtained
/// from [`crate::db::tenant_tx`]); RLS then enforces tenant isolation.
///
/// Document ACL: a chunk is only returned if its document is READABLE by the caller.
/// A document is readable when it is UNRESTRICTED (no `owners` and no `document_acl`
/// rows — the tenant-public default), OR the caller is an owner, OR a `document_acl`
/// grant matches the caller: a `user` grant equals `caller_principal_id`, and a
/// `group` grant is honored only when the caller is a member of that team per the
/// **`team_members` table** (the authoritative membership source). Group membership
/// is therefore NOT taken from the self-asserted `X-Team-Ids`/`scope` (which would
/// let a caller widen access by claiming a team) — the request `scope` remains an
/// orthogonal narrowing fence. Existing docs (empty owners/acl) stay public.
///
/// Team membership is written via the `teams.add_member`/`teams.remove_member` API,
/// so a `group` grant takes effect as soon as a member is provisioned there; `user`
/// grants and `owners` apply immediately.
/// `model_filter`: when `Some(model)`, the VECTOR arm only compares chunks embedded by that same
/// model (`chunks.embedding_model = model`), so a query vector is never cosine-compared across
/// incompatible embedding spaces after a model change (opt-in via `EMBEDDING_MODEL_CONSISTENCY`).
/// `None` disables it (any stored vector is compared). The lexical arm is model-agnostic and is
/// left unfiltered — a stale-model chunk surfacing via FTS is a relevance nuance, not a
/// distance-validity bug.
pub async fn search(
    conn: &mut PgConnection,
    req: &RetrieveRequest,
    query_embedding: &[f32],
    caller_principal_id: &str,
    model_filter: Option<&str>,
    mmr_lambda_default: f64,
) -> Result<Vec<RetrievalResult>> {
    let top_k = req.retrieval.top_k.max(1) as usize;
    // Over-fetch per arm so fusion (and MMR) has candidates to work with.
    let candidate_limit = (top_k * 4).max(50) as i64;

    // Materialize chunk embeddings back to Rust ONLY when MMR needs them for pairwise cosine (a
    // vector(1536) is ~6 KB); the non-MMR path projects `NULL::vector` and transfers none. The two
    // fragments are compile-time constants (never user input), so inlining them is injection-safe.
    let embed_proj = if req.retrieval.mmr {
        "c.embedding"
    } else {
        "NULL::vector"
    };

    let team_ids = &req.scope.team_ids;
    let project_ids = &req.scope.project_ids;

    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut rows: HashMap<String, ArmRow> = HashMap::new();

    let run_vector = matches!(
        req.retrieval.mode,
        RetrievalMode::Hybrid | RetrievalMode::Vector
    );
    let run_lexical = matches!(
        req.retrieval.mode,
        RetrievalMode::Hybrid | RetrievalMode::Lexical
    );

    if run_vector {
        let qvec = pgvector::Vector::from(query_embedding.to_vec());
        // Model-consistency predicate for the VECTOR arm. When the filter is on we INLINE the model
        // as a validated literal (NOT a bound param) so the planner can choose a per-model PARTIAL
        // HNSW index (migration 0022): a bound `embedding_model = $n` under a generic plan can't
        // match a partial index's literal predicate and falls back to the full graph — or, with the
        // old `($n IS NULL OR ...)` wrapper, to a full seq scan+sort — the filtered-HNSW recall
        // cliff (verified via EXPLAIN). `model_filter` is trusted config (EMBEDDING_MODEL); it is
        // re-validated here against a strict identifier allowlist that makes a `'`-escape impossible
        // before it is interpolated. Anything failing the allowlist is bound as a parameter instead
        // (still correct, just no partial-index acceleration) — an unvalidated string is NEVER
        // inlined. Results are identical regardless of which index is chosen; this only affects
        // recall/latency.
        let mut fallback_model: Option<&str> = None;
        let model_pred = match model_filter {
            None => "TRUE".to_string(),
            Some(m) if is_safe_embedding_model(m) => format!("c.embedding_model = '{m}'"),
            Some(m) => {
                fallback_model = Some(m);
                "c.embedding_model = $7".to_string()
            }
        };
        // $1 query vector, $2 team_ids, $3 project_ids, $4 candidate limit,
        // $5 caller principal_id (for the document ACL), $6 tenant_id (bound EXPLICITLY: chunks' PK
        // is (tenant_id, chunk_id) with a tenant-scoped chunk_id, so this is defense-in-depth beyond
        // RLS + a composite-PK-prefix hint). $7 = model fallback, present ONLY when the model wasn't
        // a safe literal (normally the model is inlined and there is no $7).
        let sql = format!(
            r#"
            SELECT c.chunk_id, c.doc_id, c.text, d.source_uri, c.metadata,
                   {embed_proj} AS embedding,
                   row_number() OVER (ORDER BY c.embedding <=> $1) AS rank,
                   extract(epoch FROM now() - c.created_at)::float8 AS age_secs,
                   (cardinality(d.owners) > 0) AS authoritative
            FROM chunks c
            JOIN documents d ON d.tenant_id = c.tenant_id AND d.doc_id = c.doc_id
            WHERE c.tenant_id = $6
              AND c.embedding IS NOT NULL
              AND {model_pred}
              AND (cardinality($2::text[]) = 0 OR d.team_scope && $2::text[])
              AND (cardinality($3::text[]) = 0
                   OR (d.metadata -> 'project_ids') ?| $3::text[])
              AND (
                    (cardinality(d.owners) = 0 AND NOT EXISTS (
                        SELECT 1 FROM document_acl a
                        WHERE a.tenant_id = d.tenant_id AND a.doc_id = d.doc_id))
                 OR $5 = ANY(d.owners)
                 OR EXISTS (
                        SELECT 1 FROM document_acl a
                        WHERE a.tenant_id = d.tenant_id AND a.doc_id = d.doc_id
                          AND ((a.grantee_type = 'user' AND a.grantee_id = $5)
                            OR (a.grantee_type = 'group' AND EXISTS (
                                  SELECT 1 FROM team_members m
                                  WHERE m.tenant_id = a.tenant_id
                                    AND m.principal_id = $5
                                    AND m.team_id = a.grantee_id))))
                  )
            ORDER BY c.embedding <=> $1
            LIMIT $4
            "#
        );
        let mut q = sqlx::query_as::<_, ArmRow>(&sql)
            .bind(qvec)
            .bind(team_ids)
            .bind(project_ids)
            .bind(candidate_limit)
            .bind(caller_principal_id)
            .bind(&req.tenant_id);
        if let Some(m) = fallback_model {
            q = q.bind(m);
        }
        let arm = q.fetch_all(&mut *conn).await?;
        fuse(&mut scores, &mut rows, arm);
    }

    if run_lexical {
        // $1 language, $2 query text, $3 team_ids, $4 project_ids, $5 limit,
        // $6 caller principal_id (for the document ACL), $7 tenant_id (explicit: chunks' PK is
        // (tenant_id, chunk_id) with a tenant-scoped chunk_id — defense-in-depth beyond RLS).
        let sql = format!(
            r#"
            WITH q AS (
                SELECT plainto_tsquery($1::regconfig, $2) AS tsq
            ), scored AS (
                SELECT c.chunk_id, c.doc_id, c.text, d.source_uri, c.metadata,
                       {embed_proj} AS embedding,
                       ts_rank(c.tsv, q.tsq) AS ts_rank_score,
                       extract(epoch FROM now() - c.created_at)::float8 AS age_secs,
                       (cardinality(d.owners) > 0) AS authoritative
                FROM q
                JOIN chunks c ON c.tenant_id = $7 AND c.tsv @@ q.tsq
                JOIN documents d ON d.tenant_id = c.tenant_id AND d.doc_id = c.doc_id
                WHERE (cardinality($3::text[]) = 0 OR d.team_scope && $3::text[])
                  AND (cardinality($4::text[]) = 0
                       OR (d.metadata -> 'project_ids') ?| $4::text[])
                  AND (
                        (cardinality(d.owners) = 0 AND NOT EXISTS (
                            SELECT 1 FROM document_acl a
                            WHERE a.tenant_id = d.tenant_id AND a.doc_id = d.doc_id))
                     OR $6 = ANY(d.owners)
                     OR EXISTS (
                            SELECT 1 FROM document_acl a
                            WHERE a.tenant_id = d.tenant_id AND a.doc_id = d.doc_id
                              AND ((a.grantee_type = 'user' AND a.grantee_id = $6)
                                OR (a.grantee_type = 'group' AND EXISTS (
                                      SELECT 1 FROM team_members m
                                      WHERE m.tenant_id = a.tenant_id
                                        AND m.principal_id = $6
                                        AND m.team_id = a.grantee_id))))
                      )
            )
            SELECT chunk_id, doc_id, text, source_uri, metadata, embedding,
                   row_number() OVER (ORDER BY ts_rank_score DESC, chunk_id) AS rank,
                   age_secs, authoritative
            FROM scored
            ORDER BY ts_rank_score DESC, chunk_id
            LIMIT $5
            "#
        );
        let arm = sqlx::query_as::<_, ArmRow>(&sql)
            .bind(TS_LANG)
            .bind(&req.query)
            .bind(team_ids)
            .bind(project_ids)
            .bind(candidate_limit)
            .bind(caller_principal_id)
            .bind(&req.tenant_id)
            .fetch_all(&mut *conn)
            .await?;
        fuse(&mut scores, &mut rows, arm);
    }

    // Opt-in heuristic rerank: nudge the RRF-fused candidates by recency + authority BEFORE
    // truncation, so the over-fetch (top_k*4) is exploited. Off = pure RRF (unchanged).
    if req.retrieval.rerank {
        rerank(&mut scores, &rows);
    }

    // Select the final top_k: MMR diversity re-rank when the request opts in, else the pure
    // relevance order. Both borrow `scores` (no consume) so the row map is still available below.
    let ranked: Vec<(String, f64)> = if req.retrieval.mmr {
        let lambda = req
            .retrieval
            .mmr_lambda
            .map(|l| l.clamp(0.0, 1.0))
            .unwrap_or(mmr_lambda_default);
        mmr(&scores, &rows, top_k, lambda)
            .into_iter()
            .map(|id| {
                let s = scores.get(&id).copied().unwrap_or(0.0);
                (id, s)
            })
            .collect()
    } else {
        // Sort by fused score desc, tie-broken by chunk_id for determinism, then truncate.
        let mut r: Vec<(String, f64)> = scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
        r.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        r.truncate(top_k);
        r
    };

    let results = ranked
        .into_iter()
        .filter_map(|(chunk_id, score)| {
            rows.remove(&chunk_id).map(|r| RetrievalResult {
                chunk_id: r.chunk_id,
                doc_id: r.doc_id,
                score: score as f32,
                text: r.text,
                source_uri: r.source_uri,
                metadata: r.metadata,
            })
        })
        .collect();

    Ok(results)
}

/// True if `m` is a safe embedding-model identifier to inline into SQL as a literal: non-empty,
/// `<= 128` bytes, starting with an ASCII alphanumeric, and containing ONLY ASCII letters, digits,
/// and `._/-`. This intentionally excludes the single quote (and every other metacharacter), so an
/// inlined `'{m}'` literal can never be broken out of — inlining a value that passes this check is
/// injection-safe. The allowlist still covers every real embedding-model name (OpenAI
/// `text-embedding-3-small`, Cohere `embed-english-v3.0`, HF `sentence-transformers/all-MiniLM-L6-v2`).
fn is_safe_embedding_model(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 128
        && m.as_bytes()[0].is_ascii_alphanumeric()
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

/// Max fractional boost for a maximally-recent chunk (recency contributes up to +30%).
const W_RECENCY: f64 = 0.30;
/// Max fractional boost for an owned/curated document (authority contributes up to +15%).
const W_AUTHORITY: f64 = 0.15;
/// Recency decay time constant (e-folding): a chunk this old keeps `1/e` (~37%) of its recency
/// factor; the factor halves at `RECENCY_DECAY_SECS * ln 2` (~20.8 days). 30 days.
const RECENCY_DECAY_SECS: f64 = 30.0 * 24.0 * 3600.0;

/// Heuristic rerank: MULTIPLY each candidate's RRF score by a recency + authority boost. The
/// boost is on the RRF's natural scale, so it reorders NEAR-ties (adjacent RRF ranks differ only
/// ~1-2%) while a large relevance gap survives — recency/authority act as tiebreakers, not as a
/// relevance override. `recency = exp(-age / RECENCY_DECAY_SECS)` in `[0,1]` (a fresh chunk ~1,
/// decaying with age; clamped so a clock-skew future timestamp can't exceed 1). `authority` is 1 for an
/// owned/curated document, else 0. Self-contained (no network egress). Runs BEFORE the optional MMR
/// diversity re-rank (see [`mmr`]), which then uses the reranked score as its relevance term. NOTE:
/// this changes the returned `score` to the reranked value.
fn rerank(scores: &mut HashMap<String, f64>, rows: &HashMap<String, ArmRow>) {
    for (chunk_id, score) in scores.iter_mut() {
        let Some(r) = rows.get(chunk_id) else {
            continue;
        };
        let recency = (-r.age_secs.max(0.0) / RECENCY_DECAY_SECS).exp();
        let authority = if r.authoritative { 1.0 } else { 0.0 };
        *score *= 1.0 + W_RECENCY * recency + W_AUTHORITY * authority;
    }
}

/// MMR (Maximal-Marginal-Relevance) selection: greedily choose `top_k` chunk_ids that maximize
/// `λ·rel_norm(d) − (1−λ)·max_{s∈selected} cosine(embed_d, embed_s)`, where `rel_norm` is the fused
/// relevance score (RRF + optional heuristic rerank) MIN-MAX-normalized to `[0,1]` across the
/// candidate pool so it is comparable to the cosine term. `λ=1` reduces to pure relevance (the first
/// pick is always the most relevant, and the diversity term is weightless). A candidate with no
/// embedding neither imposes nor receives a diversity penalty (selected on relevance alone —
/// fail-safe). Deterministic: candidates are pre-sorted by relevance desc then chunk_id asc, and a
/// tie in the greedy step keeps that order.
fn mmr(
    scores: &HashMap<String, f64>,
    rows: &HashMap<String, ArmRow>,
    top_k: usize,
    lambda: f64,
) -> Vec<String> {
    // (chunk_id, relevance, embedding-or-None).
    let mut cands: Vec<(&String, f64, Option<&[f32]>)> = scores
        .iter()
        .filter_map(|(id, &s)| {
            rows.get(id)
                .map(|r| (id, s, r.embedding.as_ref().map(|v| v.as_slice())))
        })
        .collect();
    if cands.is_empty() {
        return Vec::new();
    }
    cands.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });

    // Min-max normalize relevance to [0,1]; an all-equal pool maps to 1.0 (diversity alone decides).
    let min = cands.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
    let max = cands.iter().map(|c| c.1).fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;

    let k = top_k.min(cands.len());
    let mut selected: Vec<Option<&[f32]>> = Vec::with_capacity(k);
    let mut chosen: Vec<String> = Vec::with_capacity(k);
    // Mark picks with a per-candidate flag rather than removing from a Vec (which shifts, O(N) each):
    // scanning `cands` in its pre-sorted order and skipping taken items keeps the tie-break stable.
    let mut taken = vec![false; cands.len()];

    for _ in 0..k {
        let mut best = 0usize;
        let mut best_val = f64::NEG_INFINITY;
        for (ci, &(_, rel, emb)) in cands.iter().enumerate() {
            if taken[ci] {
                continue;
            }
            let rel_norm = if span > 0.0 { (rel - min) / span } else { 1.0 };
            // Max cosine to any already-selected candidate WITH a comparable embedding. When there is
            // no such neighbor (nothing selected yet, or no embeddings to compare on either side)
            // there is NO diversity penalty — the first pick is then the most relevant. A negative
            // cosine is HONORED (an anti-correlated neighbor is a diversity BOOST), not clamped to 0,
            // matching the `max_{s} cosine` objective.
            let mut max_sim = f64::NEG_INFINITY;
            let mut has_neighbor = false;
            for s in &selected {
                if let (Some(d), Some(sel)) = (emb, s) {
                    max_sim = max_sim.max(cosine(d, sel));
                    has_neighbor = true;
                }
            }
            let val = if has_neighbor {
                lambda * rel_norm - (1.0 - lambda) * max_sim
            } else {
                lambda * rel_norm
            };
            if val > best_val {
                best_val = val;
                best = ci;
            }
        }
        taken[best] = true;
        let (id, _, emb) = cands[best];
        selected.push(emb);
        chosen.push(id.clone());
    }
    chosen
}

/// Cosine similarity of two equal-length vectors. Returns `0.0` on a length mismatch or a zero-norm
/// vector (fail-safe: a degenerate vector then imposes no diversity penalty).
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Accumulate one arm's rows into the RRF score + row-data maps.
fn fuse(scores: &mut HashMap<String, f64>, rows: &mut HashMap<String, ArmRow>, arm: Vec<ArmRow>) {
    for r in arm {
        *scores.entry(r.chunk_id.clone()).or_insert(0.0) += 1.0 / (RRF_K + r.rank as f64);
        rows.entry(r.chunk_id.clone()).or_insert(r);
    }
}

#[cfg(test)]
mod mmr_tests {
    use super::*;

    fn row(id: &str, emb: Option<Vec<f32>>) -> ArmRow {
        ArmRow {
            chunk_id: id.to_string(),
            doc_id: id.to_string(),
            text: String::new(),
            source_uri: None,
            metadata: Value::Null,
            rank: 0,
            age_secs: 0.0,
            authoritative: false,
            embedding: emb.map(pgvector::Vector::from),
        }
    }

    fn pool(items: &[(&str, f64, Vec<f32>)]) -> (HashMap<String, f64>, HashMap<String, ArmRow>) {
        let mut scores = HashMap::new();
        let mut rows = HashMap::new();
        for (id, s, emb) in items {
            scores.insert(id.to_string(), *s);
            rows.insert(id.to_string(), row(id, Some(emb.clone())));
        }
        (scores, rows)
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9); // orthogonal
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0); // zero-norm -> 0 (fail-safe)
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0); // length mismatch -> 0
    }

    #[test]
    fn mmr_lambda_one_is_pure_relevance_order() {
        // λ=1 zeroes the diversity term, so selection is exactly the relevance ranking regardless of
        // similarity — including two near-identical vectors staying adjacent at the top.
        let (scores, rows) = pool(&[
            ("a", 1.0, vec![1.0, 0.0, 0.0]),
            ("a2", 0.9, vec![0.99, 0.14, 0.0]), // ~cos 0.99 to a
            ("b", 0.5, vec![0.0, 0.0, 1.0]),    // orthogonal to a
        ]);
        assert_eq!(mmr(&scores, &rows, 3, 1.0), vec!["a", "a2", "b"]);
    }

    #[test]
    fn mmr_demotes_a_near_duplicate_for_diversity() {
        // Relevance a > a2 > b, but a2 is a near-duplicate of a. With diversity weighted (λ=0.3), the
        // second slot goes to the DIVERSE b, not the near-duplicate a2.
        let (scores, rows) = pool(&[
            ("a", 1.0, vec![1.0, 0.0, 0.0]),
            ("a2", 0.9, vec![0.99, 0.14, 0.0]),
            ("b", 0.5, vec![0.0, 0.0, 1.0]),
        ]);
        let picked = mmr(&scores, &rows, 2, 0.3);
        assert_eq!(picked[0], "a", "most relevant is still selected first");
        assert_eq!(
            picked[1], "b",
            "the near-duplicate a2 is demoted in favor of the diverse b"
        );
    }

    #[test]
    fn mmr_is_deterministic_on_ties() {
        // Equal relevance + orthogonal vectors: the pre-sort by chunk_id makes the order stable.
        let (scores, rows) = pool(&[
            ("y", 1.0, vec![0.0, 1.0, 0.0]),
            ("x", 1.0, vec![1.0, 0.0, 0.0]),
            ("z", 1.0, vec![0.0, 0.0, 1.0]),
        ]);
        assert_eq!(mmr(&scores, &rows, 3, 0.5), vec!["x", "y", "z"]);
    }

    #[test]
    fn mmr_honors_negative_cosine_as_a_diversity_boost() {
        // `a_sim` and `z_div` are equally relevant, but `z_div` is MORE anti-correlated to the first
        // pick `m` (cos ≈ -0.995 vs ≈ -0.498) and so is the more diverse choice. MMR must pick it for
        // slot 2 even though its chunk_id sorts LATER. Under a buggy 0-floor on the cosine penalty
        // both scores would tie at 0 and the id sort would wrongly pick the less-diverse `a_sim`.
        let (scores, rows) = pool(&[
            ("m", 1.0, vec![1.0, 0.0]),
            ("a_sim", 0.5, vec![-0.5, 0.87]),
            ("z_div", 0.5, vec![-1.0, 0.1]),
        ]);
        assert_eq!(mmr(&scores, &rows, 2, 0.5), vec!["m", "z_div"]);
    }

    #[test]
    fn mmr_handles_a_missing_embedding_by_relevance_only() {
        // A candidate with no embedding neither penalizes nor is penalized — it competes on relevance.
        let mut scores = HashMap::new();
        let mut rows = HashMap::new();
        scores.insert("a".to_string(), 1.0);
        rows.insert("a".to_string(), row("a", Some(vec![1.0, 0.0])));
        scores.insert("noemb".to_string(), 0.8);
        rows.insert("noemb".to_string(), row("noemb", None));
        let picked = mmr(&scores, &rows, 2, 0.5);
        assert_eq!(picked, vec!["a", "noemb"]);
    }
}

#[cfg(test)]
mod tests {
    use super::is_safe_embedding_model;

    #[test]
    fn accepts_real_embedding_model_names() {
        for m in [
            "text-embedding-3-small",
            "text-embedding-3-large",
            "text-embedding-ada-002",
            "embed-english-v3.0",
            "voyage-2",
            "sentence-transformers/all-MiniLM-L6-v2",
            "a",
        ] {
            assert!(
                is_safe_embedding_model(m),
                "should accept model name: {m:?}"
            );
        }
    }

    #[test]
    fn rejects_anything_that_could_break_out_of_a_sql_literal() {
        // A leading `'`, an embedded quote, a comment, whitespace, or any metacharacter must fail
        // the allowlist so it can never be inlined — these fall back to a bound parameter instead.
        for m in [
            "",                          // empty
            "'",                         // a bare quote
            "x'; DROP TABLE chunks; --", // classic injection
            "text-embedding-3-small'--", // trailing break-out
            "model with spaces",         // whitespace
            "model\nnewline",            // control char
            "café",                      // non-ASCII
            "-leading-hyphen",           // must start alphanumeric
            "model;",                    // semicolon
        ] {
            assert!(
                !is_safe_embedding_model(m),
                "must reject unsafe-to-inline value: {m:?}"
            );
        }
    }

    #[test]
    fn rejects_overlong_identifier() {
        assert!(!is_safe_embedding_model(&"a".repeat(129)));
        assert!(is_safe_embedding_model(&"a".repeat(128)));
    }
}
