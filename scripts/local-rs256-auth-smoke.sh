#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/local-rs256-auth-smoke.sh [BASE_URL]

Validates a running static-RS256 Synapse boundary without printing bearer tokens:
health remains public; missing, tampered, and wrong-audience tokens are rejected;
tenant-mismatched requests fail; the local principal can read its own context.

Required environment is the same as local-rs256-token.sh.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
  usage
  exit 0
fi
[[ $# -le 1 ]] || { usage >&2; exit 2; }

base_url=${1:-http://127.0.0.1:8080}
base_url=${base_url%/}
[[ $base_url =~ ^https?:// ]] || {
  printf 'BASE_URL must use http:// or https://\n' >&2
  exit 2
}
for tool in curl jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
principal=${SYNAPSE_PRINCIPAL:-agent:ralph}
tenant=${SYNAPSE_TENANT:-}
[[ -n $tenant ]] || {
  printf 'SYNAPSE_TENANT is required\n' >&2
  exit 2
}

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/synapse-auth-smoke.XXXXXX")
chmod 0700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT

write_config() {
  local token=$1 target=$2
  umask 077
  printf 'header = "Authorization: Bearer %s"\n' "$token" > "$target"
  chmod 0600 "$target"
}

request() {
  local name=$1 expected=$2 path=$3 config=${4:-} body=${5:-}
  local response="$work_dir/$name.json" status
  local args=(
    --silent
    --show-error
    --output "$response"
    --write-out '%{http_code}'
    --request POST
    --header 'content-type: application/json'
    --header 'X-Principal-Id: attacker:spoofed'
    --header 'X-Tenant-Id: attacker-tenant'
  )
  [[ -z $config ]] || args+=(--config "$config")
  [[ -z $body ]] || args+=(--data "$body")
  status=$(curl "${args[@]}" "$base_url$path")
  if [[ $status != "$expected" ]]; then
    printf 'FAIL  %-22s expected=%s actual=%s\n' "$name" "$expected" "$status" >&2
    return 1
  fi
  printf 'PASS  %-22s http=%s\n' "$name" "$status"
}

health_status=$(curl --silent --show-error --output "$work_dir/health.json" --write-out '%{http_code}' "$base_url/health")
[[ $health_status == 200 ]] || {
  printf 'FAIL  public-health          expected=200 actual=%s\n' "$health_status" >&2
  exit 1
}
printf 'PASS  %-22s http=200\n' public-health

context_body=$(jq -cn --arg principal_id "$principal" '{principal_id:$principal_id}')
request missing-token 401 /context.get '' "$context_body"

valid_token=$("$script_dir/local-rs256-token.sh")
valid_config=$work_dir/valid.curl
write_config "$valid_token" "$valid_config"
request valid-token 200 /context.get "$valid_config" "$context_body"
jq -e --arg principal "$principal" --arg tenant "$tenant" \
  '.principal_id == $principal and .tenant_id == $tenant' "$work_dir/valid-token.json" >/dev/null || {
  printf 'FAIL  valid token identity did not come from signed claims\n' >&2
  exit 1
}
printf 'PASS  signed identity overrides spoofed headers\n'

tampered_token=${valid_token%?}
if [[ ${valid_token: -1} == A ]]; then tampered_token+=B; else tampered_token+=A; fi
tampered_config=$work_dir/tampered.curl
write_config "$tampered_token" "$tampered_config"
request tampered-signature 401 /context.get "$tampered_config" "$context_body"

wrong_audience_token=$(AUTH_JWT_AUDIENCE=not-synapse "$script_dir/local-rs256-token.sh")
wrong_audience_config=$work_dir/wrong-audience.curl
write_config "$wrong_audience_token" "$wrong_audience_config"
request wrong-audience 401 /context.get "$wrong_audience_config" "$context_body"

retrieve_body=$(jq -cn --arg principal_id "$principal" \
  '{tenant_id:"not-the-signed-tenant",principal_id:$principal_id,query:"auth boundary",retrieval:{top_k:1}}')
request tenant-mismatch 403 /retrieve "$valid_config" "$retrieve_body"

printf 'PASS  local RS256 authentication boundary\n'
