# synapse

**synapse** is an *organizational brain* — a governed service that gives AI agents and human teams
a shared, permission-aware layer of skills, memory, documents, retrieval, tools, and durable
workflows.

> **Status: pre-1.0, operational.** The core storage, Gemini/OpenAI embeddings, ACL-filtered
> retrieval, skill/context APIs, audited tool execution, durable runs, inbound MCP server, and
> crash-recovery workers are implemented. The API can still change before 1.0; production mode
> enforces the required authentication and availability controls at startup.

## What is an "organizational brain"?

There is no single vendor definition, so synapse adopts an *architectural* one: an organizational
brain is the **governed layer** that sits outside any one agent and gives every agent and team
access to reusable skills, bounded memory, enterprise documents, permission-aware retrieval, and
policy-guarded tool execution through a stable orchestration and audit surface.

The design threads a few hard-won principles:

- **Keep the brain outside any one agent.** Agents are clients; the brain is shared infrastructure.
- **Separate the canonical source-of-truth from derived retrieval artifacts.** Documents and
  metadata are authoritative; vectors, sparse indexes, and graph edges are *rebuildable*.
- **Make ACLs queryable**, not UI-only — access control lives at the namespace / shard / row level
  (Postgres RLS + JSONB ACLs), so retrieval can filter by principal before ranking.
- **Support both synchronous request/response and durable async** orchestration with resumable runs.
- **Hybrid retrieval first, graph later.** Start with pgvector + lexical + rerank; add a knowledge
  graph as an optional signal.
- **Version everything that changes meaning** — skills, prompts, chunking, embedding model, eval sets.
- **Separate personal memory from org knowledge and minimize PII.**
- **Require audit + approval before autonomous writes.**

## The 7 MVP components

1. **Document ingestion pipeline** — normalize sources into documents + chunks, then fan out to the
   vector index, (optional) graph, and context service.
2. **Canonical metadata & policy store** — Postgres + JSONB + Row-Level Security as the source of
   truth for documents, ACLs, principals, and policy.
3. **Hybrid retrieval** — pgvector vectors + lexical/sparse terms + a rerank stage, filtered by ACL.
4. **Context service** — per-principal / team / org context (role, approval limits, preferred tools,
   active projects, data-classification flags).
5. **Skill registry** — versioned, discoverable skills with input/output JSON Schemas, required
   tools, triggers, and policy tags.
6. **Tool / connector gateway** — a policy-guarded MCP-style gateway for tool execution with
   optional human-approval gates.
7. **Durable orchestration + observability** — start/resume long-running runs, human-in-the-loop
   callbacks, and end-to-end audit + tracing.

See [`docs/architecture.md`](docs/architecture.md) for the reference diagram,
[`docs/operations.md`](docs/operations.md) for release and production operations, and [`docs/`](docs/)
for the full design notes, data model, and API contract.

## API surface

All `POST` endpoints take and return JSON. Production deployments use verified JWTs. Trusted
`X-Principal-Id`, `X-Tenant-Id`, `X-Team-Ids`, and `X-Role` headers remain available for local
development or a trusted identity-aware gateway.

| Method | Path                | Purpose                                                        |
| ------ | ------------------- | -------------------------------------------------------------- |
| POST   | `/skills.register`  | Register or version a skill in the skill registry.             |
| POST   | `/documents.ingest` | Ingest a document; kicks off chunking + embedding + indexing.  |
| POST   | `/documents.reembed`| Rebuild vectors with the configured embedding model.          |
| POST   | `/context.upsert`   | Upsert principal / team / org context.                         |
| POST   | `/retrieve`         | Hybrid (vector + lexical + rerank) ACL-filtered retrieval.     |
| POST   | `/mcp`              | MCP Streamable HTTP endpoint for coding agents.                |
| POST   | `/tool.execute`     | Execute an enabled, schema-validated tenant tool.               |
| POST   | `/tools.register`    | Admin: create/update a tool contract and approval policy.       |
| POST   | `/tools.list`        | List tenant tool contracts.                                     |
| POST   | `/tools.decide`      | Admin: approve or deny a standalone pending execution.          |
| POST   | `/tools.rollback`    | Admin: run registered compensation exactly once.                |
| POST   | `/runs.start`       | Start a durable workflow run.                                  |
| POST   | `/runs.resume`      | Resume a suspended run with a token + resume input.            |
| GET    | `/audit/events`     | Query the audit event log.                                     |
| GET    | `/health`           | Liveness probe — always returns `{"status":"ok"}`.             |
| GET    | `/ready`            | Readiness probe — pings the database (`SELECT 1`); 503 if down.|

## Quickstart

Requires Rust (stable, edition 2021), Docker, and [`sqlx-cli`](https://crates.io/crates/sqlx-cli).

```bash
# 1. Start Postgres with the pgvector extension.
docker compose up -d db

# 2. Configure the environment (copy the example and edit as needed).
cp .env.example .env
set -a
. ./.env
set +a

# 3. Create the database schema. Local Docker uses one owner account. In
#    production, set MIGRATION_DATABASE_URL to a separate owner/admin connection;
#    DATABASE_URL should remain the non-owner runtime connection.
cargo install sqlx-cli --no-default-features --features rustls,postgres
sqlx migrate run --database-url "${MIGRATION_DATABASE_URL:-$DATABASE_URL}"

# 4. Run the service.
cargo run

# 5. Smoke-test it.
curl -s localhost:8080/health          # -> {"status":"ok"}
curl -s localhost:8080/ready           # -> ok, or 503 if the DB is unreachable
```

For a hardened deployment, set `SYNAPSE_ENV=production`; startup then requires verified JWT
authentication with an audience and issuer, Gemini or OpenAI embeddings, model consistency, rate
limiting, ingest idempotency, and the recovery worker. An outbound `MCP_ENDPOINT` additionally
requires HTTPS plus an exact `MCP_ALLOWED_HOSTS` entry. See [`.env.example`](.env.example).

## Coding agents over MCP

Synapse exposes a stateless Streamable HTTP MCP server at `POST /mcp`. Configure a coding agent
with the URL and a bearer token issued for Synapse:

```json
{
  "mcpServers": {
    "synapse": {
      "type": "http",
      "url": "https://synapse.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${SYNAPSE_TOKEN}"
      }
    }
  }
}
```

The server exposes governed retrieval, durable context, versioned skills, document ingestion and
re-embedding, durable runs, tool execution, and the admin-gated tool registry/decision/rollback
lifecycle. Calls reuse the same JWT verification, rate-limit, policy, RLS, idempotency, and audit
paths as REST. `MCP_ENDPOINT` configures Synapse's outbound connector and is not the inbound agent
URL. Before enabling a real connector, register every allowed tool with an input schema, minimum
connector scopes, approval mode, and optional rollback tool; unregistered calls are denied.

Never grant `CREATE`, table ownership, `BYPASSRLS`, or superuser privileges to
the role in `DATABASE_URL`. Schema changes belong to the migration role only.

You can also build and run everything in containers:

```bash
docker compose up --build
```

See [`examples/requests.http`](examples/requests.http) for ready-to-send example requests against
every endpoint.

## Repository layout

```
synapse/
├── Cargo.toml              # crate "synapse", edition 2021 (bin + lib)
├── src/
│   ├── main.rs             # config + telemetry + db pool + router; serves via tokio
│   ├── lib.rs              # module tree + pub fn app(state) -> axum::Router
│   ├── config.rs           # Config::from_env() (std::env only)
│   ├── error.rs            # Error enum + IntoResponse
│   ├── telemetry.rs        # structured JSON logs + optional OTLP traces
│   ├── db.rs               # bounded PgPool with RLS tenant transactions
│   ├── state.rs            # AppState { db, config }
│   ├── auth/               # Principal extractor + PolicyGateway
│   ├── domain/             # Skill, Context, Document, Chunk, Run, AuditEvent + DTOs
│   ├── api/                # one module per endpoint group + router()
│   ├── retrieval/          # hybrid.rs, embed.rs (Embedder trait + Mock)
│   ├── skills/             # skill registry
│   ├── context_service/    # principal/team/org context
│   ├── tools/              # tool/connector gateway
│   ├── orchestration/      # durable runs
│   ├── mcp/                # MCP client surface
│   └── audit/              # audit log
├── migrations/             # sqlx-cli migrations (run manually)
├── schemas/                # canonical *.json JSON Schemas
├── openapi/                # generated OpenAPI spec
├── docs/                   # architecture + design notes
├── examples/requests.http  # example requests for every endpoint
├── tests/                  # DB-free smoke + (de)serialization tests
├── docker-compose.yml      # pgvector/pgvector:pg16 + optional synapse service
└── Dockerfile              # multi-stage Rust build
```

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — reference architecture and component diagram.
- [`docs/`](docs/) — data model, retrieval design, policy/ACL model, and API contract.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to build, test, and submit changes.
- [`SECURITY.md`](SECURITY.md) — how to report a vulnerability.

## Tech stack

Rust · [Axum](https://github.com/tokio-rs/axum) · [sqlx](https://github.com/launchbadge/sqlx) ·
Postgres + [pgvector](https://github.com/pgvector/pgvector) · Tokio · tracing.

## License

Licensed under the [Apache License 2.0](LICENSE). Copyright 2026 WomB0ComB0.
