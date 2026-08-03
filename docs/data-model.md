# Data model, storage & retrieval

> Part of the synapse design docs. See also
> [`architecture.md`](architecture.md) · [`api.md`](api.md) · [`governance.md`](governance.md) ·
> [`roadmap.md`](roadmap.md) · [`research-report.md`](research-report.md).
>
> The canonical shapes below are implemented as Rust structs in `src/domain.rs` and as Postgres
> tables in `migrations/`. The two must stay in lockstep — the structs drive the API/JSON surface;
> the migrations own the tables.

## Canonical records vs derived artifacts

The single most important modeling decision: **separate canonical source-of-truth from derived
retrieval artifacts.**

- **Canonical records** are the stable business objects: source document metadata, access rules,
  team structure, skill definitions, connector registrations, principal context, and run history.
  They are authoritative and governed.
- **Derived artifacts** are everything a retrieval pipeline *computes*: text chunks, embeddings,
  sparse terms, similarity scores, graph triples, and rerank results. They are **rebuildable**.

Why it matters: re-indexing becomes safe (blow away chunks, regenerate from documents), governance
stays anchored to stable objects instead of transient outputs, and every derived row can be
*explained* later — provided you stamp it with the source version, chunking-profile version, and
embedding-model version that produced it.

In synapse: `documents` (migration `0004`) is canonical; `chunks` (migration `0005`) is derived and
carries `ON DELETE CASCADE` from its parent document, plus `embedding_model` + `embedding_dimensions`
+ a `document_version` in `metadata` so a rebuild is explainable.

## The four canonical schemas

These mirror the report's proposed shapes exactly and are the contract for the API in
[`api.md`](api.md). Optional fields default so callers can send minimal payloads.

### 1. Skill — `src/domain.rs::Skill` · table `skills` (migration `0006`)

A versioned, governed capability an agent can invoke. **Required: `skill_id`, `version`, `name`.**

```json
{
  "skill_id": "skill.approval.draft_response",
  "version": "1.2.0",
  "name": "Draft Approval Response",
  "summary": "Draft a response for approval workflows",
  "owners": ["team-ops"],
  "triggers": ["approval request", "legal review"],
  "input_schema": {
    "type": "object",
    "properties": { "request_id": { "type": "string" }, "context": { "type": "string" } },
    "required": ["request_id"]
  },
  "output_schema": {
    "type": "object",
    "properties": { "draft": { "type": "string" }, "citations": { "type": "array", "items": { "type": "string" } } },
    "required": ["draft"]
  },
  "required_tools": ["tool.search_docs", "tool.create_draft"],
  "policy_tags": ["read:team_docs", "write:drafts", "human_approval_required"],
  "examples": [{ "input": { "request_id": "REQ-123" }, "output": { "draft": "..." } }]
}
```

Storage notes: primary key is `(tenant_id, skill_id, version)` — skills are **immutable by version**.
An `is_latest` flag (enforced by a partial unique index `WHERE is_latest`) marks the newest version
of each `skill_id`. `input_schema`/`output_schema` are JSON Schema stored as JSONB; `policy_tags`,
`triggers`, and `required_tools` are `text[]` with GIN indexes for discovery.

### 2. Context / Principal — `src/domain.rs::Context` · table `context` (migration `0003`)

Durable per-principal state that should influence behavior but is **not** a document corpus — role,
group memberships, preferred tools, current project, approval authority, environment policy. Carry
only the **minimum** durable info; do not overload chat history for this.

```json
{
  "principal_id": "user_9281",
  "tenant_id": "acme",
  "team_ids": ["finance", "procurement"],
  "role": "manager",
  "location": "US",
  "approval_limit_usd": 25000,
  "preferred_tools": ["slack", "google_drive", "erp"],
  "active_projects": ["vendor-onboarding-q3"],
  "policy_overrides": ["requires_human_approval_for_wire_changes"],
  "data_classification": { "contains_pii": false, "special_category": false },
  "updated_at": "2026-07-01T00:00:00Z"
}
```

Storage notes: identity anchors in `principals` (migration `0002`); the *mutable* personalization
document lives in `context` so personal memory can be minimized and governed independently of org
knowledge. `data_classification` drives PII handling (see [`governance.md`](governance.md)).

### 3. Document — `src/domain.rs::Document` · table `documents` (migration `0004`)

Canonical source identity + access control kept **adjacent to** content metadata. This is the
immutable record; chunks/vectors/graph edges derive from it.

```json
{
  "doc_id": "doc_8742",
  "tenant_id": "acme",
  "team_scope": ["finance"],
  "source_system": "google_drive",
  "source_uri": "gdrive://file/abc",
  "title": "Quarterly close checklist",
  "content_type": "application/pdf",
  "language": "en",
  "version": "sha256:...",
  "created_at": "2026-06-20T14:00:00Z",
  "updated_at": "2026-06-28T09:12:00Z",
  "owners": ["user_9281"],
  "acl": { "users": ["user_9281"], "groups": ["finance-managers"], "inherit_from_source": true },
  "metadata": { "fiscal_quarter": "Q2", "department": "Finance" }
}
```

Storage notes: the `acl` object is stored **inline as a JSONB snapshot** *and* normalized into a
queryable `document_acl` table (each grant targets a `user` or `group`, with an `inherited` flag for
grants mirrored from the source system). That dual representation is deliberate — see "Make ACLs
queryable" in [`governance.md`](governance.md). `team_scope`, `owners`, and `metadata` get GIN
indexes; `title` gets a trigram GIN index for fuzzy lexical matching. The primary key is the
**composite** `(tenant_id, doc_id)`: a `doc_id` is unique only *within* a tenant, so two tenants can
each own a document called `"runbook"` without colliding (a globally-unique `doc_id` would allow
cross-tenant namespace collisions and existence-probing). `document_acl` references it via a
composite `(tenant_id, doc_id)` foreign key.

### 4. Chunk — `src/domain.rs::Chunk` · table `chunks` (migration `0005`)

The derived, retrievable unit. Preserves parent-child relationships and chunk provenance so results
are debuggable — many systems store vectors with too little metadata to explain *why* a result
appeared.

```json
{
  "chunk_id": "chunk_8742_07",
  "doc_id": "doc_8742",
  "tenant_id": "acme",
  "ordinal": 7,
  "section_path": ["Close Process", "Reconciliation"],
  "text": "Reconcile outstanding journal entries before lock.",
  "token_count": 31,
  "char_start": 6112,
  "char_end": 6180,
  "embedding_model": "text-embedding-3-small",
  "embedding_dimensions": 1536,
  "vector_ref": "vec://brain/chunk_8742_07",
  "sparse_terms_ref": "bm25://brain/chunk_8742_07",
  "metadata": { "document_version": "sha256:...", "contains_pii": false }
}
```

Storage notes: the actual dense vector is a pgvector `vector(1536)` column (nullable — a chunk may
exist before its embedding is computed during async ingestion); `vector_ref`/`sparse_terms_ref` are
optional pointers if you offload vectors/BM25 to an external store. A generated `tsv tsvector`
(`to_tsvector('english', text)`) column powers lexical search. In the Rust domain the raw vector is
represented as `Vec<f32>` (see `src/retrieval/embed.rs`), bound into the `vector(1536)` column via
`pgvector::Vector` at the storage boundary. Keys are **per-tenant**: `chunk_id` is minted with the
tenant + doc prefix (`"{tenant}::{doc}::chunk::{ordinal:04}"`), the primary key is
`(tenant_id, chunk_id)`, positional uniqueness is `(tenant_id, doc_id, ordinal)`, and the parent
reference is a composite `(tenant_id, doc_id)` foreign key to `documents` — so two tenants can both
have a `"runbook"` and its chunks without collision.

## Storage choices

A durable **transactional store remains essential** even in an "AI-native" stack. synapse uses
**PostgreSQL** as the spine:

- **Postgres + JSONB** for canonical metadata, ACLs, skill manifests, context, run metadata, and
  audit-adjacent state. JSONB gives flexible per-object metadata without schema churn; every JSONB
  column that is queried gets a GIN index (`jsonb_path_ops`).
- **pgvector** (`CREATE EXTENSION vector`) for dense embeddings, with an **HNSW** ANN index using
  cosine distance (`vector_cosine_ops`, the `<=>` operator) at pgvector defaults `m = 16,
  ef_construction = 64`. Migration `0005` documents the trade-off and leaves an **IVFFlat**
  alternative (`WITH (lists = 100)`) as a commented `TODO` for a cheaper-build / different
  recall-latency profile. HNSW vs IVFFlat is a build-cost / memory / recall trade-off; tune per
  corpus.
- **Row-Level Security (RLS)** for query-time tenant isolation — a plain-text `tenant_id` on every
  tenant-scoped table plus fail-closed policies keyed on the `app.tenant_id` GUC. Full detail in
  [`governance.md`](governance.md).
- **`pg_trgm`** for fuzzy trigram matching on titles/identifiers; **`pgcrypto`** for
  `gen_random_uuid()` surrogate keys.

Dedicated vector stores (Pinecone, Weaviate) become attractive when multitenancy, scale, or managed
operational features outweigh the simplicity of keeping everything in Postgres. For an MVP, keeping
canonical metadata, ACLs, and vectors **in one Postgres** wins on operational simplicity and
metadata unification; revisit later if scale demands it. The `vector_ref` / `sparse_terms_ref` fields
exist precisely to make that later migration non-breaking.

## Governed tool contracts and executions

`tool_definitions` (migration `0029`) is the tenant-owned server authority for outbound connector
calls. Its composite `(tenant_id, tool_id)` key, FORCE RLS policy, JSON input schema, connector
scope set, approval mode, rollback tool, enabled flag, and monotonic revision make policy explicit
and queryable. An update increments `revision`; execution intents snapshot it for auditability.

`tool_executions` stores the immutable call arguments plus lifecycle state and result. Standalone
approval records `decided_by`, `decision_reason`, and `decided_at`. `rollback_of` links compensation
to its original execution, and a tenant-scoped unique index permits at most one compensation record
per original. Migration `0030` also snapshots `rollback_tool_id` when dispatch is authorized, so a
later registry edit cannot redirect compensation for an already-completed side effect. Connector
calls happen outside database transactions, with a committed `approved` intent first; stale intents
are reconciled by the durable worker after the configured safety window.

## Hybrid retrieval

**Hybrid search is the default best starting point, not an enhancement.** Enterprise queries usually
combine exact identifiers, policy names, acronyms, *and* semantic paraphrase in the same request, so
neither pure-vector nor pure-lexical alone is enough.

synapse's `POST /retrieve` (see [`api.md`](api.md)) exposes `retrieval.mode` of `hybrid` (default),
`vector`, or `lexical`, plus `top_k`, `rerank`, and `include_graph`. The pipeline
(`src/retrieval/hybrid.rs`):

1. **ACL filter first** — resolve the principal's visible document set (via `document_acl` +
   `team_members`) *before* ranking, so nothing unauthorized is ever scored. RLS provides the
   tenant fence underneath.
2. **Dense retrieval** — pgvector HNSW cosine nearest-neighbor over `chunks.embedding`.
3. **Lexical retrieval** — Postgres full-text over the generated `chunks.tsv` (BM25-style /
   `to_tsvector`), the exact-identifier path.
4. **Fusion** — combine the two result sets (e.g. reciprocal rank fusion) into one candidate list.
5. **Rerank/diversify** — optional recency/authority reranking and MMR diversity selection over an
   over-fetched candidate pool before the final `top_k` cut.
6. **Optional graph expansion** (when `include_graph: true`) — reserved for later; see
   [`roadmap.md`](roadmap.md).

Each hit returns `{ chunk_id, doc_id, score, text, source_uri, metadata }` plus a `trace_id` so the
result is auditable and debuggable.

## Chunking

There is **no universally correct chunk size.** Prefer **structural and semantic** chunking over
fixed windows:

- Preserve document structure (headings, paragraphs, semantic coherence) — `section_path` records it.
- Keep **parent-child references** (`doc_id`, `ordinal`) so a chunk always traces back to its source.
- Store neighboring chunk IDs / ordinals for **adjacency expansion** at retrieval time.
- Reserve very large chunks for agentic-retrieval cases where whole sections matter more than pure
  recall.

Stamp every chunk with the source document version, chunking profile, and embedding model so a later
rebuild is explainable. Synapse persists canonical source text and a content hash; derived chunks and
vectors are generation-guarded and rebuildable through durable embedding jobs.

## Graph augmentation (later)

Use a knowledge graph **selectively.** If the problem is keyword + semantic retrieval over documents,
hybrid search is usually enough. If the problem is cross-document reasoning over ownership, workflows,
entities, or policy dependencies ("who owns this," "what depends on this," "which policy applies"), a
graph layer can materially improve retrieval and explainability. The strongest graphs **participate
in retrieval** (vector + keyword + traversal together), rather than being a detached "AI add-on." In
synapse the `include_graph` flag and the reference-architecture's optional Knowledge Graph node are
the seam; it is deliberately deferred — see the migration path in [`roadmap.md`](roadmap.md).
