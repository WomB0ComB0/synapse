#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/lib/postgres.sh
source "$script_dir/lib/postgres.sh"

usage() {
  cat <<'USAGE'
Usage: SYNAPSE_RESTORE_DRILL=YES RESTORE_DATABASE_URL=... scripts/postgres-restore-drill.sh BACKUP_DIRECTORY

DESTRUCTIVE to the target database. The target database name must contain
"restore", "drill", or "test". The script refuses a source/target identity match
when SOURCE_DATABASE_URL is also supplied.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -eq 1 ]] || { usage >&2; exit 2; }
[[ ${SYNAPSE_RESTORE_DRILL:-} == YES ]] || {
  printf 'Set SYNAPSE_RESTORE_DRILL=YES to acknowledge destructive target replacement.\n' >&2
  exit 2
}
: "${RESTORE_DATABASE_URL:?RESTORE_DATABASE_URL must identify an isolated drill database}"
for tool in pg_restore psql sha256sum jq python3; do
  command -v "$tool" >/dev/null 2>&1 || { printf 'Missing required tool: %s\n' "$tool" >&2; exit 2; }
done

backup_dir=$1
[[ -d $backup_dir ]] || { printf 'Backup directory not found: %s\n' "$backup_dir" >&2; exit 2; }
manifest="$backup_dir/manifest.json"
checksums="$backup_dir/SHA256SUMS"
[[ -r $manifest && -r $checksums ]] || { printf 'Backup manifest or checksum file is missing.\n' >&2; exit 2; }
dump_file=$(jq -er '.dump_file' "$manifest")
dump_path="$backup_dir/$dump_file"
[[ -r $dump_path ]] || { printf 'Dump file is missing: %s\n' "$dump_path" >&2; exit 2; }
(
  cd "$backup_dir"
  sha256sum --check SHA256SUMS
)
pg_restore --list "$dump_path" >/dev/null

target_url=$RESTORE_DATABASE_URL
unset RESTORE_DATABASE_URL
pg_env_from_url "$target_url"
target_name=$(psql -X -v ON_ERROR_STOP=1 -Atqc 'SELECT current_database()')
if [[ ! ${target_name,,} =~ (restore|drill|test) ]]; then
  printf 'Refusing target database %q: its name must contain restore, drill, or test.\n' "$target_name" >&2
  clear_pg_env
  exit 2
fi

target_identity=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
  "SELECT coalesce(inet_server_addr()::text, 'local') || ':' || inet_server_port() || '/' || current_database()")
if [[ -n ${SOURCE_DATABASE_URL:-} ]]; then
  source_url=$SOURCE_DATABASE_URL
  unset SOURCE_DATABASE_URL
  clear_pg_env
  pg_env_from_url "$source_url"
  unset source_url
  source_identity=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
    "SELECT coalesce(inet_server_addr()::text, 'local') || ':' || inet_server_port() || '/' || current_database()")
  clear_pg_env
  pg_env_from_url "$target_url"
  if [[ $target_identity == "$source_identity" ]]; then
    printf 'Refusing restore: source and target database identities are identical.\n' >&2
    clear_pg_env
    exit 2
  fi
fi
unset target_url

effective_target=$(psql -X -v ON_ERROR_STOP=1 -Atqc 'SELECT current_database()')
if [[ $effective_target != "$target_name" ]]; then
  printf 'Refusing restore: effective target changed from %q to %q.\n' "$target_name" "$effective_target" >&2
  clear_pg_env
  exit 2
fi

printf 'Restoring into isolated target database: %s\n' "$target_name"
pg_restore \
  --dbname="$target_name" \
  --clean \
  --if-exists \
  --exit-on-error \
  --single-transaction \
  --no-owner \
  --no-privileges \
  "$dump_path"

read -r successful_migrations failed_migrations <<<"$(
  psql -X -v ON_ERROR_STOP=1 -AtF' ' -c \
    "SELECT count(*) FILTER (WHERE success), count(*) FILTER (WHERE NOT success)
       FROM public._sqlx_migrations"
)"
expected_migrations=$(jq -er '.successful_migrations' "$manifest")
[[ $successful_migrations == "$expected_migrations" ]] || {
  printf 'Migration count mismatch: expected %s, restored %s.\n' "$expected_migrations" "$successful_migrations" >&2
  exit 1
}
[[ $failed_migrations == 0 ]] || { printf 'Restore contains failed migrations.\n' >&2; exit 1; }

unsafe_rls_tables=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
  "SELECT count(*)
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'public'
      AND c.relkind IN ('r','p')
      AND EXISTS (
          SELECT 1 FROM pg_attribute a
           WHERE a.attrelid = c.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped
      )
      AND NOT (c.relrowsecurity AND c.relforcerowsecurity)")
[[ $unsafe_rls_tables == 0 ]] || { printf 'Restore has %s tenant tables without FORCE RLS.\n' "$unsafe_rls_tables" >&2; exit 1; }

bad_vectors=$(psql -X -v ON_ERROR_STOP=1 -Atqc \
  "SELECT count(*) FROM public.chunks
    WHERE (embedding IS NOT NULL AND public.vector_dims(embedding) <> 1536)
       OR (embedding_dimensions IS NOT NULL AND embedding_dimensions <> 1536)")
[[ $bad_vectors == 0 ]] || { printf 'Restore has %s chunks with invalid vector dimensions.\n' "$bad_vectors" >&2; exit 1; }

psql -X -v ON_ERROR_STOP=1 -P pager=off -c \
  "SELECT 'tenants' AS relation, count(*) FROM public.tenants
   UNION ALL SELECT 'documents', count(*) FROM public.documents
   UNION ALL SELECT 'chunks', count(*) FROM public.chunks
   UNION ALL SELECT 'skills', count(*) FROM public.skills
   UNION ALL SELECT 'runs', count(*) FROM public.runs
   UNION ALL SELECT 'tool_definitions', count(*) FROM public.tool_definitions
   UNION ALL SELECT 'tool_executions', count(*) FROM public.tool_executions
   ORDER BY relation"
clear_pg_env

printf 'Restore drill passed for %s: migrations=%s, FORCE-RLS gaps=0, vector-dimension errors=0.\n' \
  "$target_name" "$successful_migrations"
