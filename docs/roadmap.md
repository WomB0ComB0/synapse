# Roadmap & migration path

> Part of the synapse design docs. See also
> [`architecture.md`](architecture.md) · [`data-model.md`](data-model.md) · [`api.md`](api.md) ·
> [`governance.md`](governance.md) · [`research-report.md`](research-report.md).

## Principle: value at every stage, governance before power

"Brain" maturity should advance in **stages**, lowest-risk first. The safe sequence is:

> **permission-aware retrieval → skill registry → durable orchestration → graph augmentation →
> governed autonomous writes**

Each stage preserves value on its own, minimizes lock-in, and lets **governance mature before the
system gets more powerful**. This is also how current frameworks and suites expose their
capabilities: retrieval first, orchestration next, then governed action. Read actions may be
autonomous early; **write actions come last, only after audit trails, approval points, and rollback
paths exist** (see [`governance.md`](governance.md)).

Stages 1-3 are implemented. Stage 5 has an audited, approval-aware connector foundation but should
remain narrowly allow-listed until each external write has rollback and operational ownership. Graph
augmentation remains deliberately deferred until a measured retrieval evaluation justifies it.

```mermaid
flowchart LR
    S1[1. Permission-aware retrieval] --> S2[2. Skill registry]
    S2 --> S3[3. Durable orchestration]
    S3 --> S4[4. Graph augmentation]
    S4 --> S5[5. Governed autonomous writes]
```

---

## Stage 1 — Permission-aware document retrieval (delivered)

**Goal:** index a narrow, high-value corpus with source ACLs intact and expose a retrieval API.

- Stand up the ingestion pipeline: `POST /documents.ingest` persists canonical `documents` + mirrored
  `document_acl`, then chunks + embeds into `chunks`.
- Implement hybrid retrieval end-to-end in `src/retrieval/hybrid.rs`: ACL filter → pgvector HNSW +
  lexical `tsv` → fusion → (optional) rerank. See [Hybrid retrieval](data-model.md#hybrid-retrieval).
- Enforce isolation from day one: RLS tenant guard + queryable team ACLs (governance rules 1–2).
- Wire a real `Embedder` (replace the `Mock` in `src/retrieval/embed.rs`).

**Exit criteria:** an operator query returns correct, ACL-filtered, cited chunks with a `trace_id`;
permission-isolation tests pass. *This is where most FAQ/support/policy/knowledge-assistant value
already lives.*

## Stage 2 — Skill registry (delivered)

**Goal:** stop repetitive team workflows from living inside prompts.

- Make `POST /skills.register` persist versioned, immutable skills with JSON Schema input/output,
  `required_tools`, `policy_tags`, and `triggers` (`skills` table, `is_latest` flag).
- Let agents and (later) workflow nodes reference a skill by `(skill_id, version)` — no prompt
  copy-paste.
- Add skill-contract tests (validate example I/O against the declared schemas).

**Exit criteria:** a skill is discoverable, versioned, and invocable by contract; a new version never
silently mutates an old one. See the [Skill schema](data-model.md#1-skill--srcdomainrsskill--table-skills-migration-0006).

## Stage 3 — Durable workflow orchestration (delivered)

**Goal:** support work that spans approvals, callbacks, and failures without losing state.

- Implement the run state machine in `src/orchestration/runs.rs`: `POST /runs.start` →
  `runs` + `run_events`; suspend to a `run_checkpoints` row with an opaque `token`;
  `POST /runs.resume` matches `(run_id, token)` and applies `resume_input`.
- Add the event-driven seam: `callbacks.human_approval` + `callbacks.webhook`.
- This is the boundary where **human approval becomes a first-class primitive** (governance) — but
  still gating *read/plan* work and draft outputs, not yet live external writes.

**Exit criteria:** a run can pause for human sign-off or an external webhook and resume correctly
after a restart. See [sync vs async](architecture.md#synchronous-requestresponse-vs-durable-asynchronous).

## Stage 4 — Graph / graph-augmentation (planned, evidence-gated)

**Goal:** add entity/relationship reasoning **only where it materially improves outcomes.**

- Add the optional knowledge-graph layer behind the `retrieval.include_graph` flag; the
  reference-architecture already reserves the Knowledge Graph node.
- Populate the graph during ingestion (fan-out from the pipeline) for ownership, dependency,
  workflow, and policy-dependency edges — the "who owns this / what depends on this / which policy
  applies" queries.
- Keep the graph **participating in retrieval** (vector + keyword + traversal together), not a
  detached "AI add-on."

**Exit criteria:** multi-hop / relationship queries measurably beat hybrid-only retrieval on the
target corpus. If the org cannot maintain a useful ontology, **defer this stage** rather than ship
shelfware. See [Graph augmentation](data-model.md#graph-augmentation-later).

## Stage 5 — Governed autonomous writes (controlled expansion)

**Goal:** let agents take real external actions — safely.

- Turn on write-capable tools through the single [tool/connector gateway](governance.md#the-tool--connector-gateway):
  register schemas, connector scopes, approval modes, and rollback handlers before enabling them;
  executions persist the full `pending → approved|denied → executed|failed` state machine.
- Require, before any autonomous write ships: **audit trails** (`audit_events`), **approval points**
  (checkpoints / approval mode), and **rollback paths**.
- Classify actions: read actions may be autonomous; sensitive writes carry approval classes and
  escalation, bounded by `context.approval_limit_usd` and `policy_overrides`.

**Exit criteria:** an agent can, e.g., open a ticket or create a purchase request only after passing
policy + approval + audit — and the whole path is queryable in `GET /audit/events`. This is the most
powerful and the **last** stage for a reason.

---

## Current implementation

- Durable document ingestion commits canonical text and lexical chunks before provider calls;
  generation-guarded Gemini/OpenAI jobs retry after outages and can be explicitly re-embedded.
- Hybrid pgvector + PostgreSQL FTS retrieval enforces tenant RLS and document ACLs before ranking,
  with model-consistent partial HNSW indexes, MMR, and a DB-gated quality evaluation.
- Versioned skills, governed context, team/ACL management, rate limits, JWT/JWKS verification, token
  revocation, idempotency, audited tools, resumable runs, and crash recovery are implemented.
- Coding agents connect through the authenticated inbound MCP Streamable HTTP endpoint,
  including admin-gated tool registration, approval/denial, and compensation operations.
- Outbound tools are tenant-registered and schema/scope validated; sensitive actions can require
  durable standalone approval and invoke an exactly-once registered rollback handler.
- Production mode fails startup unless the security, consistency, idempotency, and recovery controls
  required for an operational deployment are enabled.

The repository now includes a maintained Synapse-doc corpus/golden set, OTLP request metrics,
SLO/load smoke gates, guarded logical backups, restore-drill verification, and file-backed connector
credential rotation. Remaining production work is operational: run those controls against the real
corpus and recovery environment, integrate a trusted JWT issuer and connector account, and add graph
augmentation only where it beats hybrid retrieval on the maintained golden set.
