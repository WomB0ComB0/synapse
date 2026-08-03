# API contract

> Part of the synapse design docs. See also
> [`architecture.md`](architecture.md) · [`data-model.md`](data-model.md) ·
> [`governance.md`](governance.md) · [`roadmap.md`](roadmap.md) · [`research-report.md`](research-report.md).
>
> Endpoints are wired in `src/api/` (one module per group) and their request/response bodies are the
> DTOs in `src/domain.rs`. Runnable copies of every example live in
> [`../examples/requests.http`](../examples/requests.http).

## Principles

The API surface is deliberately **small and boring**: retrieval, context, skills, tool execution,
runs, audit — plus health/readiness. Schemas are explicit and stable; there are **no hidden,
prompt-dependent contracts**. Everything a client needs to call an agent capability is a typed JSON
body, not a paragraph of prompt text.

**Status: pre-1.0 implementation.** The documented persistence, retrieval, policy, audit, durable
workflow, and MCP paths are live. Production posture is fail-closed; see the authentication and
operations notes below.

## Authentication

Production uses a verified JWT from `Authorization: Bearer <token>`. Synapse supports rotating
RS256 JWKS, a static RS256 public key, or HS256; signed `sub`, `tenant`, `teams`, and `role` claims
drive identity. `AUTH_JWT_AUDIENCE` prevents cross-service token reuse and `AUTH_JWT_ISSUER` prevents accepting tokens minted by another issuer.

When JWT settings are absent, the extractor accepts trusted headers for local development or a
trusted identity-aware gateway:

| Header | Required | Maps to |
| --- | --- | --- |
| `X-Principal-Id` | **yes** (401 if missing) | `principal_id` |
| `X-Tenant-Id` | tenant-scoped operations | `tenant_id` |
| `X-Team-Ids` | no (comma-separated) | `team_ids` |
| `X-Role` | no; cannot self-assert admin | `role` |

Every governed request passes `PolicyGateway::authorize` before data access. The authenticated
tenant is set in the `app.tenant_id` Postgres GUC, and FORCE RLS remains the storage boundary.
`SYNAPSE_ENV=production` refuses trusted-header-only auth and requires both a JWT audience and issuer.

All error responses share the shape `{ "error": { "code": "...", "message": "..." } }` via
`error::Error`'s `IntoResponse` (variants: `NotFound`, `BadRequest`,
`Unauthorized`, `Forbidden`, `Conflict`, `TooManyRequests`, `Upstream`, `Db`, `Internal`).
When per-tenant rate limiting is enabled, an over-quota request is `429` (`too_many_requests`)
with a `Retry-After` header (seconds to wait).

## Endpoint index

| Method | Path | Handler | Purpose |
| --- | --- | --- | --- |
| POST | `/mcp` | `api/mcp.rs` | Stateless MCP Streamable HTTP tools for coding agents |
| POST | `/skills.register` | `api/skills.rs` | Register an immutable skill version |
| POST | `/skills.get` | `api/skills.rs` | Read latest or explicit skill version |
| POST | `/documents.ingest` | `api/documents.rs` | Persist canonical text/chunks and attempt embeddings |
| POST | `/documents.reembed` | `api/documents.rs` | Rebuild vectors with the configured model |
| POST | `/documents.grant` | `api/documents.rs` | Add a document ACL grant |
| POST | `/documents.revoke` | `api/documents.rs` | Remove a document ACL grant |
| POST | `/context.upsert` | `api/context.rs` | Upsert principal/team/org context |
| POST | `/context.get` | `api/context.rs` | Read governed context |
| POST | `/retrieve` | `api/retrieve.rs` | Hybrid ACL-filtered retrieval |
| POST | `/tool.execute` | `api/tools.rs` | Execute an enabled tenant tool contract |
| POST | `/tools.register` | `api/tools.rs` | Admin: create/update a tool contract |
| POST | `/tools.list` | `api/tools.rs` | List tenant tool contracts |
| POST | `/tools.decide` | `api/tools.rs` | Admin: approve/deny a standalone execution |
| POST | `/tools.rollback` | `api/tools.rs` | Admin: invoke registered compensation |
| POST | `/runs.start` | `api/runs.rs` | Start an idempotent durable run |
| POST | `/runs.resume` | `api/runs.rs` | Resume a suspended run |
| POST | `/teams.*` | `api/teams.rs` | Manage teams and membership |
| POST/DELETE | `/admin/revocations` | `api/revocations.rs` | Revoke or clear subject tokens |
| GET | `/audit/events` | `api/audit.rs` | Query tenant audit events |
| GET | `/health` | `api/health.rs` | Liveness |
| GET | `/ready` | `api/health.rs` | Database readiness |

---

## POST `/mcp`

Stateless MCP Streamable HTTP for coding agents. Send JSON-RPC `initialize`, `ping`, `tools/list`,
or `tools/call` to the same authenticated URL. Synapse negotiates MCP `2025-11-25`, `2025-06-18`,
and `2025-03-26`; notifications receive `202 Accepted`. Browser `Origin` requests are rejected by
default to protect localhost deployments from DNS rebinding.

The fourteen tools cover retrieval, document ingest/re-embedding, context get/upsert, skill
get/register, runs start/resume, policy-governed execution, and tool registry/list/decision/rollback.
Identity fields are injected from the JWT or trusted principal and cannot be overridden in tool
arguments. Registry mutation, decisions, and rollback remain admin-gated when invoked through MCP.

## POST `/skills.register`

Register or version a skill in the [skill registry](data-model.md#1-skill--srcdomainrsskill--table-skills-migration-0006).
Body is the full `Skill`. Skills are immutable by version; re-posting a new `version` for the same
`skill_id` creates a new row and re-points `is_latest`.

```json
{
  "skill_id": "skill.summarize_incident",
  "version": "1.0.0",
  "name": "Summarize Incident",
  "summary": "Summarize an incident from linked runbooks and telemetry.",
  "owners": ["team-ops"],
  "triggers": ["summarize incident", "incident recap"],
  "input_schema": { "type": "object", "properties": { "incident_id": { "type": "string" } }, "required": ["incident_id"] },
  "output_schema": { "type": "object", "properties": { "summary": { "type": "string" } } },
  "required_tools": ["retrieve"],
  "policy_tags": ["internal", "pii-safe"],
  "examples": [{ "input": { "incident_id": "INC-42" }, "output": { "summary": "..." } }]
}
```

Response: `{ "skill_id": "...", "version": "...", "status": "registered" }`.

## POST `/documents.ingest`

Ingest one canonical [`Document`](data-model.md#3-document--srcdomainrsdocument--table-documents-migration-0004);
an optional `content` field carries raw text to chunk + embed. The document metadata is flattened at
the top level. This is the entry point of the ingestion pipeline: persist canonical metadata + ACL,
then fan out to chunk/embed/index.

```json
{
  "doc_id": "doc-runbook-drone-recovery",
  "tenant_id": "acme",
  "team_scope": ["team-air"],
  "source_system": "confluence",
  "source_uri": "https://wiki.example.com/air/drone-recovery",
  "title": "Drone Recovery Runbook",
  "content_type": "text/markdown",
  "language": "en",
  "version": "7",
  "owners": ["team-air"],
  "acl": { "users": [], "groups": ["team-air"], "inherit_from_source": true },
  "metadata": { "criticality": "high" },
  "content": "# Drone Recovery\n1. Confirm last known GPS...\n2. Dispatch recovery unit..."
}
```

Response: `{ "doc_id": "...", "status": "ingested", "chunks_ingested": 1 }`. Canonical text,
metadata, ACLs, and lexical chunks commit before the provider call. A transient Gemini/OpenAI failure
returns `"queued"`; lexical retrieval remains available and the worker retries with bounded backoff.
Exhausted jobs report `"embedding_failed"` without losing canonical data.

When `INGEST_IDEMPOTENCY_ENABLED=true`, a byte-identical re-ingest returns `"replayed"` and skips
re-chunking/re-embedding. Any content, metadata, owner, or ACL change creates a new generation.

## POST `/documents.reembed`

Queues a fresh generation for an existing document, clears stale vectors, and attempts the configured
embedding model immediately. Canonical text, metadata, and ACLs are unchanged.

Request: `{ "doc_id": "doc-runbook-drone-recovery" }`.

Response: `{ "doc_id": "...", "status": "reembedded|queued|embedding_failed", "chunks_queued": 1 }`.

## POST `/context.upsert`

Upsert a principal's [`Context`](data-model.md#2-context--principal--srcdomainrscontext--table-context-migration-0003)
profile. Kept separate from chat history; PII is minimized and flagged via `data_classification`.

```json
{
  "principal_id": "alice@acme",
  "tenant_id": "acme",
  "team_ids": ["team-ops", "team-air"],
  "role": "operator",
  "location": "US-CA",
  "approval_limit_usd": 500,
  "preferred_tools": ["retrieve", "jira"],
  "active_projects": ["wildfire-2026"],
  "policy_overrides": [],
  "data_classification": { "contains_pii": false, "special_category": false }
}
```

Response: `{ "principal_id": "...", "status": "upserted" }`.

## POST `/retrieve`

Hybrid, ACL-filtered retrieval (see [Hybrid retrieval](data-model.md#hybrid-retrieval)). `mode` is
`hybrid` (default), `vector`, or `lexical`; `scope` layers team/project fences on top of ACLs;
`rerank` and `include_graph` are optional stages.

```json
{
  "tenant_id": "acme",
  "principal_id": "user_9281",
  "query": "What is our vendor approval threshold for software renewals?",
  "scope": {
    "team_ids": ["procurement"],
    "project_ids": ["vendor-onboarding-q3"]
  },
  "retrieval": {
    "mode": "hybrid",
    "top_k": 12,
    "rerank": true,
    "include_graph": false
  }
}
```

Response: `{ "results": [{ "chunk_id", "doc_id", "score", "text", "source_uri", "metadata" }], "trace_id": "..." }`.

## POST `/tool.execute`

Policy-guarded tool/connector execution through the [tool gateway](governance.md#the-tool--connector-gateway).
With a real outbound connector configured, `tool_id` must be enabled in the tenant-owned registry,
its arguments must match the registered JSON Schema, and the connector credential must declare all
registered scopes. The request's `policy.approval_mode` may strengthen the server policy but cannot
weaken a registry-required approval. Unregistered, disabled, invalid, or under-scoped calls fail
before network dispatch.

```json
{
  "tenant_id": "acme",
  "principal_id": "user_9281",
  "tool_id": "erp.create_purchase_request",
  "arguments": {
    "vendor": "ExampleSoft",
    "amount_usd": 8400
  },
  "policy": {
    "approval_mode": "required",
    "reason": "new vendor request"
  }
}
```

Response: `{ "tool_id": "...", "execution_id": "...", "status": "pending|executed", "output": {}, "requires_approval": true }`.

## POST `/tools.register`

Create or update one tenant-owned outbound tool contract. This operation is admin-only. Updating a
contract increments its `revision`; execution intents snapshot that revision. Schemas are bounded,
compiled at registration, and may not contain `$ref`, preventing schema-driven network/filesystem
resolution.

```json
{
  "tool_id": "erp.create_purchase_request",
  "description": "Create a purchase request in ERP",
  "input_schema": {
    "type": "object",
    "required": ["vendor", "amount_usd"],
    "properties": {
      "vendor": { "type": "string" },
      "amount_usd": { "type": "number", "minimum": 0 }
    },
    "additionalProperties": false
  },
  "required_scopes": ["purchase-requests:write"],
  "approval_mode": "required",
  "rollback_tool_id": "erp.cancel_purchase_request",
  "enabled": true
}
```

Response: `{ "tool": { ...contract, "revision": 1 }, "status": "registered" }`.

## POST `/tools.list`

List the authenticated tenant's current contracts in deterministic `tool_id` order. Viewer/read-only
roles may call it. Body: `{}`. Response: `{ "tools": [ ... ] }`.

## POST `/tools.decide`

Admin-only approval or denial for a standalone `pending` execution returned by `/tool.execute`.
Run-owned approvals continue through `/runs.resume`. Approval revalidates the current contract,
schema, enabled state, and connector scopes immediately before dispatch. Repeating an already
executed approval or denial returns the stored result without repeating the side effect.

```json
{ "execution_id": "uuid", "decision": "approve", "reason": "approved in change CHG-42" }
```

## POST `/tools.rollback`

Admin-only compensation for an `executed` call. The execution must have snapshotted a registered
rollback tool when it was dispatched; later registry edits cannot redirect that choice. Synapse
passes the original execution id, tool id, arguments, result, and operator reason to the compensation
tool. A uniqueness constraint guarantees one rollback execution per original; retries replay the
existing compensation outcome.

```json
{ "execution_id": "uuid", "reason": "vendor request cancelled" }
```

## POST `/runs.start`

Start a durable workflow run (see [durable orchestration](architecture.md#synchronous-requestresponse-vs-durable-asynchronous)).
`callbacks.human_approval` and `callbacks.webhook` are the event-driven seam: a run can suspend for a
human decision or an external callback and resume later.

```json
{
  "tenant_id": "acme",
  "run_type": "workflow",
  "workflow_id": "wf.procurement.vendor_review",
  "input": {
    "request_id": "REQ-123"
  },
  "callbacks": {
    "human_approval": true,
    "webhook": "https://example.internal/callbacks/run"
  }
}
```

Response: `{ "run_id": "...", "status": "...", "resume_token": "..." }`. The `resume_token` is the
opaque `token` a suspended (e.g. awaiting-approval) run hands back.

## POST `/runs.resume`

Resume a suspended run. `token` must match an open checkpoint for `run_id`
(`run_checkpoints`, migration `0007`); `resume_input` is applied to the stored run state.

```json
{
  "run_id": "run-123",
  "token": "resume-abc",
  "resume_input": { "approved": true }
}
```

Response: same `RunResponse` shape as `runs.start`.

## GET `/audit/events`

Query the append-only audit log (see [Audit everything](governance.md#4-audit-everything-that-changes-behavior-or-causes-a-side-effect)).
Scoped to the caller's tenant. Supports cursor pagination.

Response: `{ "events": [{ "event_id", "tenant_id", "principal_id", "action", "resource", "outcome", "ts", "metadata" }], "next_cursor": null }`.

## GET `/health` and GET `/ready`

- `/health` — liveness; always `200` with `{"status":"ok"}`. No dependencies touched.
- `/ready` — readiness; runs `SELECT 1` against the pool. `200` `{"status":"ready"}` when the DB
  answers, `503` otherwise. The pool is created with `connect_lazy`, so the process boots without a
  live DB and `/ready` is the honest signal of whether it can serve data.

## Notes for implementers

- The tenant guard (`app.tenant_id`) must be set from `X-Tenant-Id` **before** any tenant query, or
  RLS returns zero rows by design (fail-closed).
- Write paths must append an [`AuditEvent`](data-model.md) and, for sensitive writes, pass the
  approval gate before performing side effects.
- Keep DTOs and the JSON here in sync with `src/domain.rs`; they also drive `schemas/*.json` and the
  generated OpenAPI surface.
