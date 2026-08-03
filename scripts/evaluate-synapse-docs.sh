#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/evaluate-synapse-docs.sh [BASE_URL] TENANT_ID PRINCIPAL_ID

Runs eval/synapse-docs-golden.json against hybrid retrieval and gates recall@5
and mean reciprocal rank. Set SYNAPSE_CURL_CONFIG for verified-JWT auth.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
case $# in
  2) base_url=http://127.0.0.1:8080; tenant=$1; principal=$2 ;;
  3) base_url=${1%/}; tenant=$2; principal=$3 ;;
  *) usage >&2; exit 2 ;;
esac
for tool in curl jq; do
  command -v "$tool" >/dev/null 2>&1 || { printf 'Missing required tool: %s\n' "$tool" >&2; exit 2; }
done
golden=${GOLDEN_FILE:-eval/synapse-docs-golden.json}
[[ -r $golden ]] || { printf 'Golden set not found: %s\n' "$golden" >&2; exit 2; }
min_recall=${MIN_RECALL_AT_5:-0.80}
min_mrr=${MIN_MRR:-0.60}
curl_config=${SYNAPSE_CURL_CONFIG:-}
if [[ -n $curl_config ]]; then
  [[ -f $curl_config && -r $curl_config ]] || { printf 'SYNAPSE_CURL_CONFIG must be readable\n' >&2; exit 2; }
  mode=$(stat -c '%a' "$curl_config" 2>/dev/null || true)
  if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
    printf 'SYNAPSE_CURL_CONFIG must have no group/other permissions\n' >&2
    exit 2
  fi
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/synapse-eval.XXXXXX")
chmod 0700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT

total=$(jq 'length' "$golden")
((total > 0)) || { printf 'Golden set is empty\n' >&2; exit 2; }
hits=0
reciprocal_sum=0
for ((i=0; i<total; i++)); do
  query=$(jq -er ".[$i].query" "$golden")
  request="$work_dir/request-$i.json"
  response="$work_dir/response-$i.json"
  jq -n \
    --arg tenant_id "$tenant" \
    --arg principal_id "$principal" \
    --arg query "$query" \
    '{
      tenant_id: $tenant_id,
      principal_id: $principal_id,
      query: $query,
      retrieval: {mode: "hybrid", top_k: 5, rerank: true, include_graph: false}
    }' > "$request"
  args=(
    --silent
    --show-error
    --output "$response"
    --write-out '%{http_code}'
    --request POST
    --header 'content-type: application/json'
    --header "X-Principal-Id: $principal"
    --header "X-Tenant-Id: $tenant"
    --header 'X-Role: member'
    --data-binary "@$request"
  )
  [[ -z $curl_config ]] || args+=(--config "$curl_config")
  http_status=$(curl "${args[@]}" "$base_url/retrieve")
  [[ $http_status =~ ^2 ]] || { printf 'Retrieval query %d failed with HTTP %s\n' "$((i + 1))" "$http_status" >&2; exit 1; }

  rank=$(jq -r --argjson expected "$(jq ".[$i].expected_doc_ids" "$golden")" '
    [.results[].doc_id] as $actual
    | first(range(0; $actual | length) as $rank
        | select($expected | index($actual[$rank]))
        | ($rank + 1)) // 0
  ' "$response")
  if ((rank > 0)); then
    hits=$((hits + 1))
    reciprocal_sum=$(awk -v sum="$reciprocal_sum" -v rank="$rank" 'BEGIN {printf "%.9f", sum + 1/rank}')
    printf 'PASS  query=%d rank=%d %s\n' "$((i + 1))" "$rank" "$query"
  else
    printf 'FAIL  query=%d expected document absent from top 5: %s\n' "$((i + 1))" "$query"
  fi
done

recall=$(awk -v hits="$hits" -v total="$total" 'BEGIN {printf "%.6f", hits/total}')
mrr=$(awk -v sum="$reciprocal_sum" -v total="$total" 'BEGIN {printf "%.6f", sum/total}')
printf 'Golden-set result: queries=%d recall@5=%s MRR=%s\n' "$total" "$recall" "$mrr"
failed=0
if awk -v actual="$recall" -v minimum="$min_recall" 'BEGIN {exit !(actual < minimum)}'; then
  printf 'FAIL  recall@5 is below %s\n' "$min_recall" >&2
  failed=1
fi
if awk -v actual="$mrr" -v minimum="$min_mrr" 'BEGIN {exit !(actual < minimum)}'; then
  printf 'FAIL  MRR is below %s\n' "$min_mrr" >&2
  failed=1
fi
exit "$failed"
