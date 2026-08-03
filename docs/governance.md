# Governance, access control & compliance

> Part of the synapse design docs. See also
> [`architecture.md`](architecture.md) · [`data-model.md`](data-model.md) · [`api.md`](api.md) ·
> [`roadmap.md`](roadmap.md) · [`research-report.md`](research-report.md).
>
> Governance is what separates a demo brain from an organizational one. These are not optional
> "enterprise extras" — they are the load-bearing rules. The policy seam in code is
> `auth::policy::PolicyGateway` (`src/auth/policy.rs`), invoked on every request.

## The five governance rules

### 1. The source system is the source of truth for access

Unless there is an explicit reason to override, **access decisions inherit from the system that owns
the content.** Connectors index content *and mirror permissions* down to the document level, so
policy does not drift between systems. Glean mirrors source-of-truth ACLs into an isolated tenant;
Atlassian Rovo keeps connector permissions in sync — same principle.

In synapse this is captured on the [`Document`](data-model.md#3-document--srcdomainrsdocument--table-documents-migration-0004)
ACL: `acl.inherit_from_source` (default `true`) means "this grant is mirrored from the source." The
normalized `document_acl` table carries an `inherited boolean` flag per grant so mirrored grants are
distinguishable from ones set locally in synapse. Ingestion (`POST /documents.ingest`) is responsible
for populating those inherited grants from the connector.

### 2. Tenant & team boundaries live in storage AND query — not UI logic

Isolation must be enforced **where the query executes.** synapse operationalizes this two ways:

- **Tenant isolation via Row-Level Security.** Every tenant-scoped table carries a plain-text
  `tenant_id`. At request time the app sets a per-transaction GUC from `X-Tenant-Id`:

  ```sql
  SELECT set_config('app.tenant_id', $1, true);   -- $1 = the request's X-Tenant-Id
  ```

  RLS policies (migration `0009`) compare each row against `app_current_tenant_id()`:

  ```sql
  USING      (tenant_id = app_current_tenant_id())
  WITH CHECK (tenant_id = app_current_tenant_id())
  ```

  `current_setting('app.tenant_id', true)` returns `NULL` when unset, and `tenant_id = NULL` is never
  true — so an **unset guard sees zero rows**. That is deliberate **fail-closed / deny-by-default**
  behavior. Policies are installed on all 14 tenant-scoped tables (`tenants`, `principals`, `teams`,
  `team_members`, `context`, `documents`, `document_acl`, `chunks`, `skills`, `runs`, `run_events`,
  `run_checkpoints`, `tool_executions`, `audit_events`) with `FORCE ROW LEVEL SECURITY` so even the
  table owner is subject to them. The app must run as a **non-owner** role; provisioning/cross-tenant
  jobs use a dedicated `BYPASSRLS` admin role.

- **Team isolation via queryable ACLs.** Team scope and per-document grants are **normalized and
  indexed** — not UI-only. `document_acl` holds `(grantee_type: user|group, grantee_id, permission)`
  rows; `team_members` holds queryable membership edges; `documents.team_scope` is a GIN-indexed
  `text[]`. Retrieval resolves "which docs can this principal see?" as a real query **before**
  ranking (see [Hybrid retrieval](data-model.md#hybrid-retrieval)), so unauthorized chunks are never
  scored. This mirrors Pinecone namespaces / Weaviate tenant shards, adapted to Postgres.

### 3. RBAC plus source-aware exceptions

Durable brains need **two layers** of access logic:

- **Platform roles (RBAC)** control administrative surfaces — who can register skills, provision
  tenants, configure connectors, or read the full audit log. Carried by a signed JWT claim in production (or a trusted gateway header in development) and
  checked by `PolicyGateway::authorize`; an unverified header cannot self-assert `admin`.
- **Source-derived permissions** control knowledge/tool access — what documents this principal can
  retrieve, which tools they can execute. These come from mirrored ACLs (rule 1) and `context`
  policy fields (`approval_limit_usd`, `policy_overrides`), not from the platform role alone.

The two combine: a platform `admin` role does **not** implicitly grant read access to a document
whose source ACL excludes them — knowledge access still flows through source-aware ACLs. RBAC is resolved from verified role claims and tenant role assignments. Retrieval then applies
source-aware owners and normalized user/group ACLs before either ranking arm sees a chunk.

### 4. Audit everything that changes behavior or causes a side effect

A real brain needs observable, queryable records for: connector configuration changes, **skill
version changes**, **permission changes**, **tool executions**, **run state transitions**, and
**human approvals**. synapse's append-only [`audit_events`](data-model.md) table (migration `0008`)
is the trail; `GET /audit/events` reads it. Its shape:

```json
{ "event_id", "tenant_id", "principal_id", "action", "resource", "outcome", "ts", "metadata" }
```

Design choices that matter: `principal_id` is deliberately **not** a foreign key, so audit survives
principal deletion and can record external actors; `outcome` records `allow`/`deny`/`success`/`error`
so denials are auditable, not just successes; indexes on `(tenant_id, ts DESC)`, `action`,
`resource`, and a GIN index on `metadata` make the trail queryable. Every write path is expected to
append an event. Where the platform lacks a built-in audit facility for some dimension, that gap is
called out explicitly rather than assumed covered.

### 5. PII minimization and purpose limitation

Personal data must be **adequate, relevant, and limited to what is necessary** (data minimization),
and safeguards tailored to impact level. For brain design that means:

- Do **not** dump entire HR/CRM/support datasets into long-term memory by default.
- **Tag sensitive content early.** [`Context.data_classification`](data-model.md#2-context--principal--srcdomainrscontext--table-context-migration-0003)
  (`{ contains_pii, special_category }`) and chunk-level `metadata.contains_pii` carry the flags that
  drive handling.
- **Separate interaction transcripts from reusable semantic facts.** Personal `context` lives in its
  own table, apart from org knowledge (`documents`/`chunks`), so personal memory can be minimized and
  governed independently.
- **Define retention independently** for documents, runs, traces, and memories.

## The tool / connector gateway

Every external action passes through **one** governance choke point — `tools::gateway`
(`src/tools/gateway.rs`, `src/mcp.rs`) — never through ad-hoc prompt instructions. The gateway owns
auth, tool metadata, allowed operations, approval requirements, rate limits, and audit logging.

A real outbound connector is deny-by-default. The tenant-owned `tool_definitions` registry
(migration `0029`) is the server authority for each tool's JSON input schema, minimum connector
scopes, enabled state, approval mode, rollback handler, and monotonic revision. `$ref` is rejected
from schemas, connector credentials are sent only to the configured allowlisted HTTPS host in
production, and a request cannot weaken registry-required approval. Unregistered, disabled,
under-scoped, or schema-invalid calls stop before network dispatch.

Invocations are recorded in `tool_executions` with a definition revision and a durable status
machine (`pending → approved|denied → executed|failed`). `POST /tools.decide` revalidates policy at
decision time. Dispatch snapshots the selected rollback handler onto the execution ledger, so a
later registry update cannot redirect compensation; `POST /tools.rollback` invokes that handler at
most once.
Registry mutation, decisions, and compensation are admin-only and audited. Read actions may often
be autonomous; **sensitive write actions carry approval classes and escalation paths.**

## Human approval as a first-class primitive

Human approval is a **policy primitive, not a UX fallback.** It is durable: a run or tool call
suspends, persists its state, and resumes only after a decision.

- Durable runs: `POST /runs.start` with `callbacks.human_approval: true` can suspend at an interrupt
  point, persisting a `run_checkpoints` row with an opaque `token`; `POST /runs.resume` matches
  `(run_id, token)` and applies `resume_input` (e.g. `{ "approved": true }`). See
  [sync vs async](architecture.md#synchronous-requestresponse-vs-durable-asynchronous).
- Standalone tool calls: registry or request `approval_mode: required` records the call as
  `pending` and performs no side effect until an admin calls `POST /tools.decide`. Run-owned tool
  gates continue through `POST /runs.resume` so checkpoint tokens remain authoritative.
- Compensation: an admin may call `POST /tools.rollback` only after a successful execution and only
  when the original contract names an enabled rollback tool. The database permits one compensation
  execution per original, making operator retries side-effect free.

The operational stance: **read actions may be autonomous; sensitive writes require approval + audit
before they run.**

## Versioning — everything that can silently change behavior

Versioning must cover more than code. Skills, tool schemas, prompts, **chunking strategies**,
**embedding-model versions**, **source document versions**, graph-extraction runs, and evaluation
datasets all change behavior. The safe pattern is to **stamp every derived artifact** with the source
version, chunking-profile version, and embedding-model version, so a rebuild can be *explained*
later.

synapse implements this:

- **Skills** are immutable by `(tenant_id, skill_id, version)`; a partial unique index tracks
  `is_latest` per `skill_id` (migration `0006`). New behavior = new version, never a silent edit.
- **Documents** carry a `version` stamp (e.g. `sha256:...`) as the source-of-truth marker.
- **Chunks** record `embedding_model` + `embedding_dimensions` and the source `document_version` in
  `metadata`, so the derived vector index is regenerable and explainable (migration `0005`).

## How this maps to code

| Rule | Enforced / seeded in |
| --- | --- |
| Source-of-truth ACLs | `documents.acl` + `document_acl.inherited` (migration `0004`); populated by `api/documents.rs` |
| Tenant isolation (RLS) | `app.tenant_id` GUC + policies (migrations `0001`, `0009`) |
| Team isolation (queryable ACLs) | `document_acl`, `team_members`, `documents.team_scope` (migrations `0002`, `0004`) |
| RBAC + source-aware exceptions | `auth::policy` action matrix + tenant roles; document owners/ACL predicates in `retrieval/hybrid.rs` |
| Audit everything | `audit_events` (migration `0008`), `src/audit.rs`, `GET /audit/events` |
| PII minimization | `context.data_classification`, chunk `metadata.contains_pii` (migrations `0003`, `0005`) |
| Governed tools + human approval | `tool_definitions`/`tool_executions` (migrations `0007`, `0029`), `src/tools/`, `src/api/tools.rs` |
| Versioning | immutable `skills`, document `version`, chunk provenance (migrations `0005`, `0006`) |

The staged order in which these are hardened — retrieval isolation first, autonomous writes last —
is the subject of [`roadmap.md`](roadmap.md).
