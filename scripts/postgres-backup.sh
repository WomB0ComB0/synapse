#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/lib/postgres.sh
source "$script_dir/lib/postgres.sh"

usage() {
  cat <<'USAGE'
Usage: MIGRATION_DATABASE_URL=... scripts/postgres-backup.sh [OUTPUT_DIRECTORY]

Creates a mode-0700 logical-backup directory containing a custom-format dump,
SHA-256 checksum, and non-secret manifest. Use a migration/backup role that can
see every tenant; a FORCE-RLS runtime role can produce an incomplete dump.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -le 1 ]] || { usage >&2; exit 2; }
: "${MIGRATION_DATABASE_URL:?MIGRATION_DATABASE_URL must be set in the process environment}"
for tool in pg_dump pg_restore psql sha256sum jq python3; do
  command -v "$tool" >/dev/null 2>&1 || { printf 'Missing required tool: %s\n' "$tool" >&2; exit 2; }
done

umask 077
output_root=${1:-backups}
mkdir -p "$output_root"
chmod 0700 "$output_root"
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
name="synapse-logical-$timestamp"
tmp_dir=$(mktemp -d "$output_root/.${name}.tmp.XXXXXX")
final_dir="$output_root/$name"
trap 'rm -rf "$tmp_dir"' EXIT

dump_name=synapse.dump
dump_path="$tmp_dir/$dump_name"
pg_env_from_url "$MIGRATION_DATABASE_URL"
unset MIGRATION_DATABASE_URL
printf 'Creating consistent custom-format PostgreSQL dump...\n'
pg_dump \
  --format=custom \
  --compress=6 \
  --file="$dump_path"

# Parsing the archive catches truncation and format corruption before publication.
pg_restore --list "$dump_path" >/dev/null
(
  cd "$tmp_dir"
  sha256sum "$dump_name" > SHA256SUMS
)

migration_count=$(psql -X -v ON_ERROR_STOP=1 -Atqc   "SELECT count(*) FROM public._sqlx_migrations WHERE success")
failed_migrations=$(psql -X -v ON_ERROR_STOP=1 -Atqc   "SELECT count(*) FROM public._sqlx_migrations WHERE NOT success")
clear_pg_env
git_commit=$(git rev-parse HEAD 2>/dev/null || printf 'unknown')
dump_sha=$(awk '{print $1}' "$tmp_dir/SHA256SUMS")

jq -n \
  --arg created_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg git_commit "$git_commit" \
  --arg pg_dump_version "$(pg_dump --version)" \
  --arg dump_file "$dump_name" \
  --arg dump_sha256 "$dump_sha" \
  --argjson successful_migrations "$migration_count" \
  --argjson failed_migrations "$failed_migrations" \
  '{
    format: "postgres-custom",
    created_at: $created_at,
    git_commit: $git_commit,
    pg_dump_version: $pg_dump_version,
    dump_file: $dump_file,
    dump_sha256: $dump_sha256,
    successful_migrations: $successful_migrations,
    failed_migrations: $failed_migrations
  }' > "$tmp_dir/manifest.json"

chmod 0600 "$dump_path" "$tmp_dir/SHA256SUMS" "$tmp_dir/manifest.json"
mv "$tmp_dir" "$final_dir"
trap - EXIT
printf 'Backup complete: %s\n' "$final_dir"
printf 'Store this directory on encrypted, access-controlled storage before treating it as durable.\n'
