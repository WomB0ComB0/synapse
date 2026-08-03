#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/ingest-synapse-docs.sh [BASE_URL] TENANT_ID PRINCIPAL_ID

Ingests README.md and the maintained architecture/API/operations docs as a bounded,
principal-readable corpus. In verified-JWT mode, set SYNAPSE_CURL_CONFIG to a
mode-0600 curl config containing the Authorization header. In trusted-header mode,
the supplied tenant and principal are sent through X-* headers.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
case $# in
  2) base_url=http://127.0.0.1:8080; tenant=$1; principal=$2 ;;
  3) base_url=${1%/}; tenant=$2; principal=$3 ;;
  *) usage >&2; exit 2 ;;
esac
[[ $base_url =~ ^https?:// ]] || { printf 'BASE_URL must use http:// or https://\n' >&2; exit 2; }
[[ -n $tenant && -n $principal ]] || { printf 'TENANT_ID and PRINCIPAL_ID must be non-empty\n' >&2; exit 2; }
for tool in curl jq sha256sum git; do
  command -v "$tool" >/dev/null 2>&1 || { printf 'Missing required tool: %s\n' "$tool" >&2; exit 2; }
done

curl_config=${SYNAPSE_CURL_CONFIG:-}
if [[ -n $curl_config ]]; then
  [[ -f $curl_config && -r $curl_config ]] || { printf 'SYNAPSE_CURL_CONFIG must be readable\n' >&2; exit 2; }
  mode=$(stat -c '%a' "$curl_config" 2>/dev/null || true)
  if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
    printf 'SYNAPSE_CURL_CONFIG must have no group/other permissions\n' >&2
    exit 2
  fi
fi

files=(
  README.md
  docs/architecture.md
  docs/api.md
  docs/data-model.md
  docs/governance.md
  docs/operations.md
  docs/roadmap.md
)
for file in "${files[@]}"; do
  [[ -r $file ]] || { printf 'Required corpus file is missing: %s\n' "$file" >&2; exit 2; }
done

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/synapse-ingest.XXXXXX")
chmod 0700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT
commit=$(git rev-parse HEAD)
repository=$(git config --get remote.origin.url 2>/dev/null || printf 'local')
ingested=0
queued=0
replayed=0

for file in "${files[@]}"; do
  slug=${file%.md}
  slug=${slug//\//-}
  slug=${slug//./-}
  slug=${slug,,}
  doc_id="synapse-$slug"
  title=$(awk '/^# / {sub(/^# /, ""); print; exit}' "$file")
  [[ -n $title ]] || title=$file
  version=$(sha256sum "$file" | awk '{print $1}')
  request="$work_dir/request.json"
  response="$work_dir/response.json"
  jq -n \
    --arg doc_id "$doc_id" \
    --arg tenant_id "$tenant" \
    --arg source_uri "https://github.com/WomB0ComB0/synapse/blob/$commit/$file" \
    --arg title "$title" \
    --arg version "$version" \
    --arg principal "$principal" \
    --arg path "$file" \
    --arg commit "$commit" \
    --arg repository "$repository" \
    --rawfile content "$file" \
    '{
      doc_id: $doc_id,
      tenant_id: $tenant_id,
      team_scope: [],
      source_system: "git",
      source_uri: $source_uri,
      title: $title,
      content_type: "text/markdown",
      language: "en",
      version: $version,
      owners: [$principal],
      acl: {users: [$principal], groups: [], inherit_from_source: false},
      metadata: {
        corpus: "synapse-maintained-docs",
        repository: $repository,
        path: $path,
        git_commit: $commit
      },
      content: $content
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
  http_status=$(curl "${args[@]}" "$base_url/documents.ingest")
  if [[ ! $http_status =~ ^2 ]]; then
    message=$(jq -r '.message // .error // "unknown API error"' "$response" 2>/dev/null || printf 'invalid error response')
    printf 'FAIL  %s returned HTTP %s: %s\n' "$file" "$http_status" "$message" >&2
    exit 1
  fi
  status=$(jq -er '.status' "$response")
  chunks=$(jq -r '.chunks_ingested // .chunks_queued // 0' "$response")
  printf '%-28s %-18s chunks=%s\n' "$doc_id" "$status" "$chunks"
  case "$status" in
    ingested|reembedded) ingested=$((ingested + 1)) ;;
    queued|embedding_failed) queued=$((queued + 1)) ;;
    replayed) replayed=$((replayed + 1)) ;;
  esac
done

printf 'Corpus complete: ingested=%d queued=%d replayed=%d documents=%d\n' \
  "$ingested" "$queued" "$replayed" "${#files[@]}"
