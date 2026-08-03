#!/usr/bin/env bash
set -uo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/lib/postgres.sh
source "$script_dir/lib/postgres.sh"

usage() {
  cat <<'USAGE'
Usage: scripts/production-preflight.sh [--env-file PATH] [--check-db] [--check-http] [--base-url URL]

Reads an env file as data; it never sources or executes it. Secret values are never printed.
Optional live checks use DATABASE_URL through PGDATABASE and call only /health and /ready.
USAGE
}

env_file=".env"
check_db=false
check_http=false
base_url=""
while (($#)); do
  case "$1" in
    --env-file)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      env_file=$2
      shift 2
      ;;
    --check-db)
      check_db=true
      shift
      ;;
    --check-http)
      check_http=true
      shift
      ;;
    --base-url)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      base_url=${2%/}
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

[[ -r "$env_file" ]] || { printf 'Cannot read env file: %s\n' "$env_file" >&2; exit 2; }

declare -A values=()
declare -A present=()
trim() {
  local value=$1
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

while IFS= read -r raw || [[ -n "$raw" ]]; do
  raw=${raw%$'\r'}
  [[ "$raw" =~ ^[[:space:]]*# ]] && continue
  [[ "$raw" =~ ^[[:space:]]*$ ]] && continue
  if [[ "$raw" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
    key=${BASH_REMATCH[2]}
    value=$(trim "${BASH_REMATCH[3]}")
    if [[ ${#value} -ge 2 ]]; then
      if [[ ${value:0:1} == '"' && ${value: -1} == '"' ]] ||
         [[ ${value:0:1} == "'" && ${value: -1} == "'" ]]; then
        value=${value:1:${#value}-2}
      fi
    fi
    present["$key"]=1
    values["$key"]=$value
  fi
done < "$env_file"

failures=0
warnings=0
pass() { printf 'PASS  %s\n' "$*"; }
info() { printf 'INFO  %s\n' "$*"; }
warn() { printf 'WARN  %s\n' "$*"; warnings=$((warnings + 1)); }
fail() { printf 'FAIL  %s\n' "$*"; failures=$((failures + 1)); }

is_set() { [[ -n ${present[$1]:-} && -n ${values[$1]:-} ]]; }
is_true() {
  local value=${values[$1]:-}
  value=${value,,}
  [[ $value == 1 || $value == true || $value == yes || $value == on || $value == enable || $value == enabled ]]
}
require_set() {
  if is_set "$1"; then pass "$1 is set"; else fail "$1 is missing or empty"; fi
}
require_true() {
  if is_true "$1"; then pass "$1 is enabled"; else fail "$1 must be true"; fi
}

printf 'Synapse production preflight: %s\n\n' "$env_file"

if [[ -f $env_file && ! -L $env_file ]]; then
  pass 'runtime env is a regular, non-symlink file'
else
  fail 'runtime env must be a regular, non-symlink file'
fi
env_mode=$(stat -c '%a' "$env_file" 2>/dev/null || true)
if [[ $env_mode =~ ^[0-7][0-7]?[0-7]?$ ]] && (( (8#$env_mode & 077) == 0 )); then
  pass 'runtime env has no group/other permissions'
else
  fail 'runtime env must have no group/other permissions (recommended mode 0600)'
fi

if [[ ${values[SYNAPSE_ENV]:-} == production || ${values[SYNAPSE_ENV]:-} == prod ]]; then
  pass 'SYNAPSE_ENV is production'
else
  fail 'SYNAPSE_ENV must be production'
fi
require_set DATABASE_URL
if is_set MIGRATION_DATABASE_URL; then
  fail 'MIGRATION_DATABASE_URL must not be present in the runtime environment'
else
  pass 'migration-owner credential is absent from runtime env'
fi

provider=${values[EMBEDDING_PROVIDER]:-}
provider=${provider,,}
case "$provider" in
  gemini|google)
    pass 'Gemini embedding provider selected'
    if is_set GEMINI_API_KEY || is_set EMBEDDING_API_KEY; then
      pass 'Gemini embedding credential is set'
    else
      fail 'Gemini requires GEMINI_API_KEY or EMBEDDING_API_KEY'
    fi
    ;;
  openai|openai-compatible|compatible)
    pass 'OpenAI-compatible embedding provider selected'
    if is_set OPENAI_API_KEY || is_set EMBEDDING_API_KEY; then
      pass 'OpenAI-compatible embedding credential is set'
    else
      fail 'OpenAI-compatible embeddings require OPENAI_API_KEY or EMBEDDING_API_KEY'
    fi
    ;;
  mock|local|none|'') fail 'production requires Gemini or OpenAI embeddings' ;;
  *) fail "unknown EMBEDDING_PROVIDER: $provider" ;;
esac
require_set EMBEDDING_MODEL
require_true EMBEDDING_MODEL_CONSISTENCY

jwt_modes=0
is_set AUTH_JWKS_URL && jwt_modes=$((jwt_modes + 1))
is_set AUTH_JWT_PUBLIC_KEY && jwt_modes=$((jwt_modes + 1))
is_set AUTH_JWT_SECRET && jwt_modes=$((jwt_modes + 1))
if ((jwt_modes == 0)); then
  fail 'verified JWT is missing: configure AUTH_JWKS_URL, AUTH_JWT_PUBLIC_KEY, or AUTH_JWT_SECRET'
elif ((jwt_modes > 1)); then
  warn 'multiple JWT verification modes are set; JWKS takes precedence over public key and HS256'
else
  pass 'one verified JWT mode is configured'
fi
require_set AUTH_JWT_AUDIENCE
require_set AUTH_JWT_ISSUER
if is_true AUTH_REVOCATION_ENABLED; then
  pass 'stateful token revocation is enabled'
else
  warn 'AUTH_REVOCATION_ENABLED is off; rely on short token TTLs or enable revocation'
fi

require_true RATE_LIMIT_ENABLED
require_true INGEST_IDEMPOTENCY_ENABLED
require_true WORKER_ENABLED

if is_set MCP_ENDPOINT; then
  pass 'outbound MCP endpoint is configured'
  if is_set MCP_AUTH_TOKEN && is_set MCP_AUTH_TOKEN_FILE; then
    fail 'set only one of MCP_AUTH_TOKEN or MCP_AUTH_TOKEN_FILE'
  elif is_set MCP_AUTH_TOKEN || is_set MCP_AUTH_TOKEN_FILE; then
    pass 'outbound MCP credential is configured'
  else
    fail 'outbound MCP endpoint requires a token or token file in production'
  fi
  require_set MCP_ALLOWED_HOSTS
  if is_set MCP_SCOPES; then pass 'connector scopes are declared'; else warn 'MCP_SCOPES is empty'; fi
  if is_set MCP_AUTH_TOKEN_FILE; then
    token_file=${values[MCP_AUTH_TOKEN_FILE]}
    if [[ $token_file == /* ]]; then pass 'MCP_AUTH_TOKEN_FILE is absolute'; else fail 'MCP_AUTH_TOKEN_FILE must be absolute'; fi
    if [[ -f $token_file && -r $token_file ]]; then
      mode=$(stat -c '%a' "$token_file" 2>/dev/null || true)
      if [[ $mode =~ ^[0-7][0-7]?[0-7]?$ ]] && (( (8#$mode & 077) == 0 )); then
        pass 'MCP_AUTH_TOKEN_FILE has private permissions'
      else
        fail 'MCP_AUTH_TOKEN_FILE must have no group/other permissions'
      fi
    else
      fail 'MCP_AUTH_TOKEN_FILE is not a readable regular file'
    fi
  fi
else
  info 'outbound MCP is disabled; tool registry may remain empty'
  if is_set MCP_AUTH_TOKEN || is_set MCP_AUTH_TOKEN_FILE; then
    fail 'connector credential is set without MCP_ENDPOINT'
  fi
fi

if is_set OTEL_EXPORTER_OTLP_ENDPOINT || is_set OTEL_ENDPOINT; then
  pass 'OTLP telemetry endpoint is configured'
else
  warn 'OTLP telemetry is disabled; JSON logs remain available'
fi

if $check_db; then
  if ! is_set DATABASE_URL; then
    fail 'database check skipped because DATABASE_URL is missing'
  elif ! command -v psql >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
    fail 'database check requires psql and python3'
  else
    db_url=${values[DATABASE_URL]}
    if pg_env_from_url "$db_url"; then
      unset db_url
      role_state=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
        "SELECT CASE WHEN rolsuper OR rolbypassrls THEN 'privileged' ELSE 'rls-enforcing' END FROM pg_roles WHERE rolname = current_user" 2>/dev/null || true)
      if [[ $role_state == rls-enforcing ]]; then pass 'runtime database role cannot bypass RLS'; else fail 'runtime database role check failed or role is privileged'; fi

      owner_count=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') AND c.relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user) AND c.relname <> '_sqlx_migrations'" 2>/dev/null || true)
      if [[ $owner_count == 0 ]]; then pass 'runtime role owns no application tables'; else fail 'runtime role owns application tables or ownership check failed'; fi

      migration_count=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
        "SELECT count(*) FROM _sqlx_migrations WHERE success" 2>/dev/null || true)
      if [[ $migration_count =~ ^[0-9]+$ && $migration_count -ge 30 ]]; then
        pass "migration ledger has $migration_count successful migrations"
      else
        fail 'migration ledger is missing, incomplete, or unreachable'
      fi
      clear_pg_env
    else
      unset db_url
      fail 'DATABASE_URL could not be parsed for the database checks'
    fi
  fi
fi

if $check_http; then
  if [[ -z $base_url ]]; then
    bind=${values[BIND_ADDR]:-127.0.0.1:8080}
    if [[ $bind == 0.0.0.0:* ]]; then bind="127.0.0.1:${bind##*:}"; fi
    base_url="http://$bind"
  fi
  if curl --fail --silent --show-error --max-time 10 "$base_url/health" >/dev/null; then pass '/health is responding'; else fail '/health failed'; fi
  if curl --fail --silent --show-error --max-time 15 "$base_url/ready" >/dev/null; then pass '/ready is responding'; else fail '/ready failed'; fi
fi

printf '\nSummary: %d failure(s), %d warning(s)\n' "$failures" "$warnings"
((failures == 0))
