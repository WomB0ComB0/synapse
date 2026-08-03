#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/load-smoke.sh [BASE_URL] [PATH]

Defaults: BASE_URL=http://127.0.0.1:8080 PATH=/ready

Environment:
  REQUESTS=100             completed requests to measure
  CONCURRENCY=10           maximum concurrent curl processes
  WARMUP_REQUESTS=5        unmeasured warmups
  METHOD=GET               HTTP method
  BODY_FILE=               optional request body file
  LOAD_CURL_CONFIG=        optional mode-0600 curl config for auth headers
  EXPECTED_STATUS_CLASS=2  accepted first status digit
  MAX_P95_MS=750           SLO gate
  MAX_ERROR_RATE=0.01      SLO gate as fraction
  REQUEST_TIMEOUT_SECS=15  per-request timeout

Use only read-only or explicitly idempotent endpoints. Secret headers belong in
LOAD_CURL_CONFIG, not command-line arguments.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -le 2 ]] || { usage >&2; exit 2; }
base_url=${1:-http://127.0.0.1:8080}
path=${2:-/ready}
requests=${REQUESTS:-100}
concurrency=${CONCURRENCY:-10}
warmups=${WARMUP_REQUESTS:-5}
method=${METHOD:-GET}
expected_class=${EXPECTED_STATUS_CLASS:-2}
max_p95_ms=${MAX_P95_MS:-750}
max_error_rate=${MAX_ERROR_RATE:-0.01}
timeout_secs=${REQUEST_TIMEOUT_SECS:-15}

[[ $base_url =~ ^https?:// ]] || { printf 'BASE_URL must use http:// or https://\n' >&2; exit 2; }
[[ $path == /* ]] || { printf 'PATH must begin with /\n' >&2; exit 2; }
[[ $requests =~ ^[1-9][0-9]*$ ]] || { printf 'REQUESTS must be a positive integer\n' >&2; exit 2; }
[[ $concurrency =~ ^[1-9][0-9]*$ ]] || { printf 'CONCURRENCY must be a positive integer\n' >&2; exit 2; }
[[ $warmups =~ ^[0-9]+$ ]] || { printf 'WARMUP_REQUESTS must be a non-negative integer\n' >&2; exit 2; }
[[ $expected_class =~ ^[1-5]$ ]] || { printf 'EXPECTED_STATUS_CLASS must be 1-5\n' >&2; exit 2; }
command -v curl >/dev/null 2>&1 || { printf 'curl is required\n' >&2; exit 2; }

curl_config=${LOAD_CURL_CONFIG:-}
if [[ -n $curl_config ]]; then
  [[ -f $curl_config && -r $curl_config ]] || { printf 'LOAD_CURL_CONFIG must be a readable file\n' >&2; exit 2; }
  mode=$(stat -c '%a' "$curl_config" 2>/dev/null || true)
  if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
    printf 'LOAD_CURL_CONFIG must have no group/other permissions\n' >&2
    exit 2
  fi
fi
body_file=${BODY_FILE:-}
if [[ -n $body_file && ! -r $body_file ]]; then
  printf 'BODY_FILE must be readable\n' >&2
  exit 2
fi

url="${base_url%/}$path"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/synapse-load.XXXXXX")
chmod 0700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT

request_once() {
  local output=$1
  local -a args=(
    --silent
    --show-error
    --output /dev/null
    --request "$method"
    --connect-timeout 5
    --max-time "$timeout_secs"
    --write-out '%{http_code} %{time_total}\n'
  )
  [[ -z $curl_config ]] || args+=(--config "$curl_config")
  [[ -z $body_file ]] || args+=(--header 'content-type: application/json' --data-binary "@$body_file")
  if ! curl "${args[@]}" "$url" > "$output" 2>/dev/null; then
    printf '000 0\n' > "$output"
  fi
}

for ((i=1; i<=warmups; i++)); do
  request_once "$work_dir/warmup-$i"
done

started_ns=$(date +%s%N)
active=0
for ((i=1; i<=requests; i++)); do
  request_once "$work_dir/result-$i" &
  active=$((active + 1))
  if ((active >= concurrency)); then
    wait -n
    active=$((active - 1))
  fi
done
wait
finished_ns=$(date +%s%N)

cat "$work_dir"/result-* > "$work_dir/results"
awk '{print $2}' "$work_dir/results" | sort -n > "$work_dir/times"
percentile_ms() {
  local percentile=$1
  awk -v p="$percentile" '{value[NR]=$1} END { idx=int(NR*p); if (idx < NR*p) idx++; if (idx < 1) idx=1; printf "%.3f", value[idx]*1000 }' "$work_dir/times"
}
p50_ms=$(percentile_ms 0.50)
p95_ms=$(percentile_ms 0.95)
p99_ms=$(percentile_ms 0.99)
errors=$(awk -v expected="$expected_class" 'substr($1,1,1) != expected {count++} END {print count+0}' "$work_dir/results")
error_rate=$(awk -v errors="$errors" -v total="$requests" 'BEGIN {printf "%.6f", errors/total}')
wall_ms=$(awk -v start="$started_ns" -v finish="$finished_ns" 'BEGIN {printf "%.3f", (finish-start)/1000000}')
throughput=$(awk -v total="$requests" -v wall="$wall_ms" 'BEGIN {printf "%.3f", total/(wall/1000)}')

printf 'Synapse load smoke\n'
printf '  target:       %s %s\n' "$method" "$url"
printf '  requests:     %s\n' "$requests"
printf '  concurrency:  %s\n' "$concurrency"
printf '  errors:       %s (%s)\n' "$errors" "$error_rate"
printf '  throughput:   %s req/s\n' "$throughput"
printf '  latency p50:  %s ms\n' "$p50_ms"
printf '  latency p95:  %s ms\n' "$p95_ms"
printf '  latency p99:  %s ms\n' "$p99_ms"
printf '  wall:         %s ms\n' "$wall_ms"

failed=0
if awk -v actual="$p95_ms" -v limit="$max_p95_ms" 'BEGIN {exit !(actual > limit)}'; then
  printf 'FAIL  p95 %s ms exceeds %s ms\n' "$p95_ms" "$max_p95_ms" >&2
  failed=1
else
  printf 'PASS  p95 is within %s ms\n' "$max_p95_ms"
fi
if awk -v actual="$error_rate" -v limit="$max_error_rate" 'BEGIN {exit !(actual > limit)}'; then
  printf 'FAIL  error rate %s exceeds %s\n' "$error_rate" "$max_error_rate" >&2
  failed=1
else
  printf 'PASS  error rate is within %s\n' "$max_error_rate"
fi
exit "$failed"
