# Operations

This runbook covers a single-host systemd deployment. Containers may use the same release,
configuration, migration, and verification sequence. Keep runtime and migration database roles
separate: the service role must remain a non-owner subject to FORCE RLS; only the migration role may
own schema objects.

## Release artifact

Build from a reviewed commit with the lockfile enforced:

```bash
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release --locked --bin synapse
sha256sum target/release/synapse
```

Install into a commit-addressed directory rather than executing from a mutable checkout:

```bash
release=$(git rev-parse HEAD)
sudo install -d -o root -g root -m 0755 "/opt/synapse/releases/$release"
sudo install -o root -g root -m 0755 target/release/synapse \
  "/opt/synapse/releases/$release/synapse"
sudo install -d -o root -g root -m 0755 \
  "/opt/synapse/releases/$release/scripts/lib"
sudo install -o root -g root -m 0755 \
  scripts/postgres-backup.sh \
  scripts/postgres-backup-s3.sh \
  scripts/postgres-backup-monitor.sh \
  scripts/postgres-restore-drill.sh \
  scripts/postgres-restore-s3-drill.sh \
  scripts/systemd-failure-alert.sh \
  "/opt/synapse/releases/$release/scripts/"
sudo install -o root -g root -m 0644 scripts/lib/postgres.sh \
  "/opt/synapse/releases/$release/scripts/lib/postgres.sh"
sudo ln -sfn "/opt/synapse/releases/$release" /opt/synapse/current.new
sudo mv -Tf /opt/synapse/current.new /opt/synapse/current
```

Record the commit and SHA-256 digest in the release record. Keep at least one previous compatible
binary for rollback. The checked-in [`synapse.service`](../deploy/systemd/synapse.service) executes
`/opt/synapse/current/synapse` as an unprivileged `synapse` user.

## Continuous integration

The `ci` workflow runs on GitHub-hosted `ubuntu-latest` for `pull_request`, `master` pushes, and
explicit `workflow_dispatch`. It starts an isolated pgvector PostgreSQL service and runs formatting,
clippy (`-D warnings`), the complete DB-gated test suite, shell syntax checks, and whitespace checks.
The `security` workflow runs a weekly RustSec (`cargo audit`) dependency audit. Neither workflow
requires repository secrets, and checkout credential persistence is disabled.

## Configuration and secrets

Create `/etc/synapse/synapse.env` as root-owned mode `0600`. Never place the migration-owner URL in
this runtime file. A production process must set `SYNAPSE_ENV=production`; startup then fails closed
unless these controls are present:

- a non-owner `DATABASE_URL` whose role is subject to RLS;
- verified JWT through JWKS, an RS256 public key, or an HS256 secret;
- `AUTH_JWT_AUDIENCE` and `AUTH_JWT_ISSUER`;
- Gemini or OpenAI embedding credentials and `EMBEDDING_MODEL_CONSISTENCY=true`;
- `RATE_LIMIT_ENABLED=true`, `INGEST_IDEMPOTENCY_ENABLED=true`, and `WORKER_ENABLED=true`;
- for outbound tools, an HTTPS `MCP_ENDPOINT`, a dedicated `MCP_AUTH_TOKEN` or
  `MCP_AUTH_TOKEN_FILE`, least-privilege `MCP_SCOPES`, and an exact `MCP_ALLOWED_HOSTS` entry.

Prefer a platform secret manager or systemd credentials over long-lived plaintext values. If an
environment file is used, rotate it atomically, restart the service, verify readiness, and revoke the
old credential. Keep connector credentials separate from identity-provider signing keys.

Prefer `MCP_AUTH_TOKEN_FILE` for connector credentials. In production it must be an absolute path
to a regular file with no group/other permissions. Synapse validates it at startup and reloads it
before each logical tool call; atomically replace the file to rotate credentials without a restart.
In-flight retries keep the credential they started with. Revoke the old token after a probe call
using the replacement succeeds.

Run the secret-safe preflight against the exact runtime file before installing or restarting the
service. It parses assignments as data and never sources the file or prints credential values:

```bash
scripts/production-preflight.sh --env-file /etc/synapse/synapse.env --check-db
```

The check intentionally fails while trusted-header auth is active. For a real issuer, configure one
verification mode (prefer `AUTH_JWKS_URL`), then set its exact `AUTH_JWT_AUDIENCE` and
`AUTH_JWT_ISSUER`. Tokens must carry signed `sub`, `tenant`, optional `teams`, and optional `role`
claims. Use short token lifetimes; enable `AUTH_REVOCATION_ENABLED` when immediate per-principal
revocation is required.

For a single-host deployment without an external identity provider, use the local static-RS256
workflow. It stores the signing key outside the repository, installs only the public key in
Synapse's runtime environment, and mints a short-lived token in memory for each caller process:

```bash
scripts/local-rs256-init.sh
scripts/local-rs256-configure-env.sh /etc/synapse/synapse.env

AUTH_JWT_ISSUER=urn:synapse:local \
AUTH_JWT_AUDIENCE=synapse-api \
SYNAPSE_TENANT=acme \
SYNAPSE_PRINCIPAL=agent:demo \
scripts/run-with-local-rs256-token.sh demo synapse live-test demo
scripts/local-rs256-auth-smoke.sh http://127.0.0.1:8080
```

The signing key defaults to `~/.local/share/synapse/local-rs256/private.pem` with mode `0600`;
Synapse must never receive that file. Tokens default to a 15-minute lifetime and may be shortened
with `SYNAPSE_LOCAL_RS256_TTL_SECS`. Wrap unattended Ralph commands with
`run-with-local-rs256-token.sh` so each invocation gets a fresh token instead of persisting a bearer
credential in an environment file. For a one-off administrative operation, set
`SYNAPSE_ROLE=admin` only on the token-minting command; never persist it in the service or caller
environment. This local issuer is appropriate for a single trusted host. Use a rotating JWKS identity
provider before distributing callers across hosts or trust boundaries.

## Database migration

Back up the database and verify restore access before a production migration. Apply migrations with
the owner connection supplied only to the command:

```bash
sqlx migrate info --database-url "$MIGRATION_DATABASE_URL"
sqlx migrate run --database-url "$MIGRATION_DATABASE_URL"
```

Then verify the runtime role and migration ledger:

```sql
SELECT version, success FROM _sqlx_migrations ORDER BY version DESC LIMIT 5;
SELECT current_user, rolsuper, rolbypassrls
FROM pg_roles
WHERE rolname = current_user;
```

The runtime role must not be a superuser, must not bypass RLS, and must not own tenant tables. Schema
migrations are forward-only. Confirm binary compatibility before rolling a binary back across an
already-applied migration.

## Service installation

```bash
sudo install -o root -g root -m 0644 deploy/systemd/synapse.service \
  /etc/systemd/system/synapse.service
sudo systemd-analyze verify /etc/systemd/system/synapse.service
sudo systemctl daemon-reload
sudo systemctl enable --now synapse.service
```

The unit denies host filesystem writes outside its systemd-managed state/runtime directories,
removes capabilities, restricts namespaces and address families, and runs without privilege gains.
If a platform requires an additional writable path, add the narrow path explicitly rather than
weakening `ProtectSystem` or `ProtectHome` globally.

## Deployment verification

Do not route traffic until readiness succeeds:

```bash
curl --fail --silent --show-error http://127.0.0.1:8080/health
curl --fail --silent --show-error http://127.0.0.1:8080/ready
systemctl is-active --quiet synapse.service
journalctl -u synapse.service --since=-5m --no-pager
```

Also verify one authenticated read for each enabled surface: retrieval, inbound MCP `tools/list`, and
`/tools.list`. In a staging tenant, exercise register, schema rejection, approval, connector
idempotency, and rollback before enabling write-capable production contracts. Never create a
temporary production administrator solely for a smoke test; use a real short-lived identity from the
configured issuer.

Seed and evaluate the maintained Synapse documentation corpus with the same principal that will query
it. In verified-JWT mode, put the bearer header in a mode-`0600` curl config and set
`SYNAPSE_CURL_CONFIG` for both commands:

```bash
scripts/ingest-synapse-docs.sh http://127.0.0.1:8080 acme agent:demo
scripts/evaluate-synapse-docs.sh http://127.0.0.1:8080 acme agent:demo
```

The ingestion command uses deterministic document ids and content hashes, so unchanged reruns replay
without embedding work. The evaluation gates recall@5 and mean reciprocal rank against
`eval/synapse-docs-golden.json`; update that maintained set whenever corpus responsibilities change.

## Monitoring and SLOs

Set `OTEL_EXPORTER_OTLP_ENDPOINT` to the collector's HTTP/protobuf base endpoint, normally
`http://127.0.0.1:4318`. Synapse appends `/v1/traces` and `/v1/metrics`; collector credentials belong
in `OTEL_EXPORTER_OTLP_HEADERS`. It exports traces plus bounded-cardinality
`synapse.http.server.requests`, `http.server.request.duration`, and
`http.server.active_requests` metrics labeled only by method, matched route, and status.

For a single-host deployment without an existing backend, the repository includes a pinned local
Collector, Prometheus, Tempo, and Grafana stack. All host ports bind to loopback; Grafana requires a
randomly generated local administrator credential, anonymous access is disabled, metrics are retained
for 15 days or 5 GB, and Tempo uses its 14-day local-storage default:

```bash
scripts/local-observability-init.sh
OBS_ENV="$HOME/.config/synapse/observability.env"
docker compose --env-file "$OBS_ENV" -f deploy/observability/compose.yml config --quiet
docker compose --env-file "$OBS_ENV" -f deploy/observability/compose.yml up -d
```

Set `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318` in Synapse's mode-`0600` runtime environment
and restart Synapse. Generate at least one authenticated request, then validate every signal and
provisioned Grafana resource:

```bash
scripts/local-observability-smoke.sh "$HOME/.config/synapse/observability.env"
```

Grafana is available at `http://127.0.0.1:3000`; the administrator username and password remain in
`~/.config/synapse/observability.env`. Prometheus and Tempo troubleshooting endpoints bind only to
`127.0.0.1:9090` and `127.0.0.1:3200`. This local stack is suitable for one trusted host; use object
storage and a separately authenticated Grafana/collector deployment before distributing telemetry
across machines.

Alert on:

- readiness failures and restart loops;
- request error rate and p95/p99 latency by route;
- embedding provider failures, retry exhaustion, and queued ingestion age;
- stale `approved` tool executions and failed compensations;
- suspended or retrying runs older than their workflow SLO;
- database pool saturation, lock waits, replication lag, and storage growth;
- authentication failures, policy denies, and unusual rate-limit activity.

Establish load baselines with production-like network distance and corpus size. The dependency-free
smoke harness gates p95 latency and error rate; its default target is the read-only `/ready` route:

```bash
REQUESTS=500 CONCURRENCY=25 MAX_P95_MS=750 scripts/load-smoke.sh http://127.0.0.1:8080 /ready
```

For authenticated routes, put the bearer header in a mode-`0600` curl config and set
`LOAD_CURL_CONFIG`; do not place bearer tokens in command-line arguments. Optimize measured
bottlenecks; remote PostgreSQL round trips often dominate small policy and catalog requests.

## Backup, restore, and rollback

Use provider-managed point-in-time recovery as the primary database recovery control. The logical
backup script adds a portable custom-format archive, integrity checksum, and non-secret manifest.
It requires the migration/backup role because the runtime role is intentionally constrained by RLS:

```bash
MIGRATION_DATABASE_URL="$MIGRATION_DATABASE_URL" scripts/postgres-backup.sh /secure/backups
```

For an immutable off-host copy, configure a private Object Lock-enabled S3 bucket and a dedicated
AWS profile that can only list, upload, and verify objects under the backup prefix. Keep the GPG
passphrase outside the bucket and preserve a separately controlled recovery copy. The S3 wrapper
verifies the logical dump, GPG authentication, archive structure, S3 checksum, stored size,
server-side encryption, and Object Lock metadata before reporting success. On success it deletes
the local plaintext dump and retains only the newest `SYNAPSE_BACKUP_LOCAL_KEEP` (default 7)
encrypted copies:

```bash
export SYNAPSE_BACKUP_BUCKET=synapse-backups-example-us-west-2
export SYNAPSE_BACKUP_PASSPHRASE_FILE=/etc/synapse/backup.passphrase
export SYNAPSE_BACKUP_ROOT=/var/lib/synapse/backups
export SYNAPSE_ENCRYPTED_BACKUP_ROOT=/var/lib/synapse/encrypted-backups
export SYNAPSE_BACKUP_EVIDENCE_ROOT=/var/lib/synapse/recovery-evidence
export AWS_PROFILE=synapse-backup
export AWS_REGION=us-west-2
MIGRATION_DATABASE_URL="$MIGRATION_DATABASE_URL" scripts/postgres-backup-s3.sh
```

For the checked-in systemd unit, put only non-file settings plus `MIGRATION_DATABASE_URL` in
root-owned mode-`0600` `/etc/synapse/backup.env`. Install the backup passphrase and the dedicated
AWS profile as root-only files. `LoadCredential` exposes private copies to the unprivileged service
for one invocation; the files do not need to be readable by `synapse`:

```bash
sudo install -d -o root -g root -m 0700 /etc/synapse
sudo install -o root -g root -m 0600 /secure/synapse-backup.passphrase \
  /etc/synapse/backup.passphrase
sudo install -o root -g root -m 0600 /secure/aws-credentials \
  /etc/synapse/aws-credentials
sudo install -o root -g root -m 0600 /secure/aws-config \
  /etc/synapse/aws-config
```

Install the system backup, hourly freshness monitor, and local failure recorder:

```bash
sudo install -o root -g root -m 0644 deploy/systemd/synapse-backup.service \
  /etc/systemd/system/synapse-backup.service
sudo install -o root -g root -m 0644 deploy/systemd/synapse-backup.timer \
  /etc/systemd/system/synapse-backup.timer
sudo install -o root -g root -m 0644 deploy/systemd/synapse-backup-monitor.service \
  /etc/systemd/system/synapse-backup-monitor.service
sudo install -o root -g root -m 0644 deploy/systemd/synapse-backup-monitor.timer \
  /etc/systemd/system/synapse-backup-monitor.timer
sudo install -o root -g root -m 0644 deploy/systemd/synapse-operation-alert@.service \
  /etc/systemd/system/synapse-operation-alert@.service
sudo systemd-analyze verify /etc/systemd/system/synapse-backup.service \
  /etc/systemd/system/synapse-backup.timer \
  /etc/systemd/system/synapse-backup-monitor.service \
  /etc/systemd/system/synapse-backup-monitor.timer \
  /etc/systemd/system/synapse-operation-alert@.service
sudo systemctl daemon-reload
sudo systemctl start synapse-backup.service
sudo systemctl start synapse-backup-monitor.service
sudo systemctl enable --now synapse-backup.timer synapse-backup-monitor.timer
```

The monitor fails when no verified S3 success record exists or the latest one is older than 36
hours. Failed backup, monitor, and restore units create mode-`0600` JSON evidence under
`/var/lib/synapse/recovery-evidence/alerts` and emit an error to the journal. For external delivery,
put a curl config containing the webhook URL and authentication headers in a mode-`0400` file owned
by `synapse`. Either set its path as `SYNAPSE_ALERT_CURL_CONFIG` in `/etc/synapse/backup-alert.env`
or, preferably, deliver it as a systemd credential by uncommenting the `LoadCredential=alert-curl-config`
line in `synapse-operation-alert@.service`; the script auto-detects it under `$CREDENTIALS_DIRECTORY`.
Notifications for the same unit are limited to one every six hours.

The local custom dump is not durable until the off-host verification succeeds. Run the destructive
restore only against a separately created database whose name contains `restore`, `drill`, or
`test`:

```bash
SYNAPSE_RESTORE_DRILL=YES \
RESTORE_DATABASE_URL="$RESTORE_DATABASE_URL" \
SOURCE_DATABASE_URL="$MIGRATION_DATABASE_URL" \
  scripts/postgres-restore-drill.sh /secure/backups/synapse-logical-YYYYMMDDTHHMMSSZ
```

The drill validates the checksum and archive, migration count, failed migrations, FORCE RLS on every
tenant table, vector dimensions, and representative relation counts. Add corpus-specific retrieval
queries to the release evidence after real documents have been ingested.

To prove the off-host recovery path instead of a local dump, use the exact-version S3 wrapper. It
selects the newest object version by default, verifies S3 checksum/encryption/Object Lock metadata,
rejects unsafe archive members, shreds temporary plaintext, and then invokes the same guarded drill:

```bash
SYNAPSE_RESTORE_DRILL=YES \
RESTORE_DATABASE_URL="$RESTORE_DATABASE_URL" \
SOURCE_DATABASE_URL="$MIGRATION_DATABASE_URL" \
SYNAPSE_BACKUP_BUCKET="$SYNAPSE_BACKUP_BUCKET" \
  scripts/postgres-restore-s3-drill.sh
```

A quarterly timer is provided but must not be enabled until `/etc/synapse/restore-drill.env` points
to a dedicated database whose name contains `restore`, `drill`, or `test`. Install and manually run
the service once before enabling the timer:

```bash
sudo install -o root -g root -m 0644 deploy/systemd/synapse-restore-drill.service \
  /etc/systemd/system/synapse-restore-drill.service
sudo install -o root -g root -m 0644 deploy/systemd/synapse-restore-drill.timer \
  /etc/systemd/system/synapse-restore-drill.timer
sudo systemctl daemon-reload
sudo systemctl start synapse-restore-drill.service
sudo systemctl enable --now synapse-restore-drill.timer
```

Do not reuse the production database as the target. Keep the drill database inaccessible to the
runtime role, and remove or recreate it when the retained restored data is no longer required.

For an application rollback, atomically repoint `/opt/synapse/current` to a known compatible release,
restart, and run the verification sequence. Do not reverse SQL migrations manually during an
incident. If the new schema is not backward-compatible, restore service with a forward fix or use the
predefined database recovery procedure.
