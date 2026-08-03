#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/local-observability-smoke.sh [ENV_FILE]

Checks the local Collector, Prometheus, Tempo, and Grafana deployment. Grafana
credentials are read as data from a mode-0600 file and are never printed.

Default:
  ENV_FILE=$HOME/.config/synapse/observability.env
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
  usage
  exit 0
fi
[[ $# -le 1 ]] || { usage >&2; exit 2; }
for tool in curl jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

env_file=${1:-"${SYNAPSE_OBSERVABILITY_ENV_FILE:-$HOME/.config/synapse/observability.env}"}
[[ -f $env_file && ! -L $env_file && -r $env_file ]] || {
  printf 'ENV_FILE must be a readable, regular, non-symlink file\n' >&2
  exit 2
}
mode=$(stat -c '%a' "$env_file" 2>/dev/null || true)
if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
  printf 'ENV_FILE must have no group/other permissions\n' >&2
  exit 2
fi

read_value() {
  local key=$1
  awk -F= -v key="$key" '$1 == key {sub(/^[^=]*=/, ""); print; exit}' "$env_file"
}
admin_user=$(read_value GRAFANA_ADMIN_USER)
admin_password=$(read_value GRAFANA_ADMIN_PASSWORD)
[[ -n $admin_user && -n $admin_password ]] || {
  printf 'Grafana credentials are missing from ENV_FILE\n' >&2
  exit 2
}

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/synapse-observability-smoke.XXXXXX")
chmod 0700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT
curl_config=$work_dir/grafana.curl
printf 'user = "%s:%s"\n' "$admin_user" "$admin_password" > "$curl_config"
chmod 0600 "$curl_config"
unset admin_password
curl_args=(--connect-timeout 3 --max-time 10)

check_http() {
  local name=$1 url=$2 output=$3
  if curl "${curl_args[@]}" --fail --silent --show-error --output "$output" "$url"; then
    printf 'PASS  %s\n' "$name"
  else
    printf 'FAIL  %s\n' "$name" >&2
    return 1
  fi
}

check_http collector-health http://127.0.0.1:13133/ "$work_dir/collector.json"
check_http prometheus-ready http://127.0.0.1:9090/-/ready "$work_dir/prometheus.txt"
check_http tempo-ready http://127.0.0.1:3200/ready "$work_dir/tempo.txt"
check_http grafana-health http://127.0.0.1:3000/api/health "$work_dir/grafana.json"

curl "${curl_args[@]}" --fail --silent --show-error --get \
  --data-urlencode 'query=count({__name__=~"synapse_.*|http_server_.*"})' \
  --output "$work_dir/metrics.json" \
  http://127.0.0.1:9090/api/v1/query
jq -e '.status == "success" and ((.data.result[0].value[1] // "0") | tonumber) > 0' \
  "$work_dir/metrics.json" >/dev/null || {
  printf 'FAIL  Prometheus has no Synapse metrics\n' >&2
  exit 1
}
printf 'PASS  Prometheus contains Synapse metrics\n'

curl "${curl_args[@]}" --fail --silent --show-error --get \
  --data-urlencode 'q={ resource.service.name = "synapse" }' \
  --data-urlencode 'limit=1' \
  --output "$work_dir/traces.json" \
  http://127.0.0.1:3200/api/search
jq -e '(.traces // []) | length > 0' "$work_dir/traces.json" >/dev/null || {
  printf 'FAIL  Tempo has no Synapse traces\n' >&2
  exit 1
}
printf 'PASS  Tempo contains Synapse traces\n'

for uid in prometheus tempo; do
  curl "${curl_args[@]}" --fail --silent --show-error --config "$curl_config" \
    --output "$work_dir/datasource-$uid.json" \
    "http://127.0.0.1:3000/api/datasources/uid/$uid/health"
  jq -e '(.status // "") | ascii_downcase == "ok" or ascii_downcase == "success"' \
    "$work_dir/datasource-$uid.json" >/dev/null || {
    printf 'FAIL  Grafana datasource %s is unhealthy\n' "$uid" >&2
    exit 1
  }
  printf 'PASS  Grafana datasource %s is healthy\n' "$uid"
done

curl "${curl_args[@]}" --fail --silent --show-error --config "$curl_config" \
  --output "$work_dir/dashboard.json" \
  http://127.0.0.1:3000/api/dashboards/uid/synapse-operations
jq -e '.dashboard.uid == "synapse-operations"' "$work_dir/dashboard.json" >/dev/null || {
  printf 'FAIL  Synapse dashboard is not provisioned\n' >&2
  exit 1
}
printf 'PASS  Synapse dashboard is provisioned\n'
printf 'PASS  local observability stack\n'
