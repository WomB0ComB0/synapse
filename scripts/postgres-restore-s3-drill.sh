#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/lib/postgres.sh
source "$script_dir/lib/postgres.sh"

usage() {
  cat <<'USAGE'
Usage: SYNAPSE_RESTORE_DRILL=YES RESTORE_DATABASE_URL=... \
  SYNAPSE_BACKUP_BUCKET=... scripts/postgres-restore-s3-drill.sh

Downloads an exact immutable S3 object version, verifies its checksum,
encryption, and retention metadata, decrypts it into an owner-only temporary
workspace, and runs the guarded PostgreSQL restore drill.

Required:
  SYNAPSE_RESTORE_DRILL=YES
  RESTORE_DATABASE_URL                 isolated target containing restore/drill/test
  SYNAPSE_BACKUP_BUCKET

Optional:
  SOURCE_DATABASE_URL                 refuses an identical source/target
  AWS_PROFILE                         default: synapse-backup
  AWS_REGION                          default: us-west-2
  SYNAPSE_BACKUP_S3_PREFIX            default: backups
  SYNAPSE_BACKUP_S3_KEY               default: newest encrypted backup
  SYNAPSE_BACKUP_S3_VERSION_ID        default: newest version for selected key
  SYNAPSE_BACKUP_PASSPHRASE_FILE      default:
    ~/.local/share/synapse/backup-keys/synapse-backup.passphrase
  SYNAPSE_BACKUP_EVIDENCE_ROOT        default:
    ~/.local/share/synapse/recovery-evidence
  SYNAPSE_RESTORE_WORK_ROOT           default: <evidence-root>/.restore-work
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -eq 0 ]] || { usage >&2; exit 2; }
[[ ${SYNAPSE_RESTORE_DRILL:-} == YES ]] || {
  printf 'Set SYNAPSE_RESTORE_DRILL=YES to acknowledge destructive target replacement.\n' >&2
  exit 2
}
: "${RESTORE_DATABASE_URL:?RESTORE_DATABASE_URL must identify an isolated drill database}"
: "${SYNAPSE_BACKUP_BUCKET:?SYNAPSE_BACKUP_BUCKET must be set}"

for tool in aws base64 date find gpg jq openssl sha256sum shred stat tar; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

aws_profile=${AWS_PROFILE:-synapse-backup}
aws_region=${AWS_REGION:-us-west-2}
s3_prefix=${SYNAPSE_BACKUP_S3_PREFIX:-backups}
requested_key=${SYNAPSE_BACKUP_S3_KEY:-}
requested_version=${SYNAPSE_BACKUP_S3_VERSION_ID:-}
passphrase_file=${SYNAPSE_BACKUP_PASSPHRASE_FILE:-"$HOME/.local/share/synapse/backup-keys/synapse-backup.passphrase"}
evidence_root=${SYNAPSE_BACKUP_EVIDENCE_ROOT:-"$HOME/.local/share/synapse/recovery-evidence"}
work_root=${SYNAPSE_RESTORE_WORK_ROOT:-"$evidence_root/.restore-work"}

[[ $SYNAPSE_BACKUP_BUCKET =~ ^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$ ]] || {
  printf 'SYNAPSE_BACKUP_BUCKET is not a valid S3 bucket name.\n' >&2
  exit 2
}
s3_prefix=${s3_prefix#/}
s3_prefix=${s3_prefix%/}
[[ -n $s3_prefix ]] || {
  printf 'SYNAPSE_BACKUP_S3_PREFIX must not be empty.\n' >&2
  exit 2
}
if [[ -n $requested_key && $requested_key != "$s3_prefix/"* ]]; then
  printf 'Requested S3 key must be under %s/.\n' "$s3_prefix" >&2
  exit 2
fi
[[ -z $requested_version || -n $requested_key ]] || {
  printf 'SYNAPSE_BACKUP_S3_VERSION_ID requires SYNAPSE_BACKUP_S3_KEY.\n' >&2
  exit 2
}
[[ -f $passphrase_file && ! -L $passphrase_file ]] || {
  printf 'Backup passphrase must be a regular, non-symlink file: %s\n' "$passphrase_file" >&2
  exit 2
}
passphrase_mode=$(stat -c '%a' "$passphrase_file")
[[ $passphrase_mode == 400 || $passphrase_mode == 600 ]] || {
  printf 'Backup passphrase must have mode 0400 or 0600, got %s.\n' "$passphrase_mode" >&2
  exit 2
}

umask 077
mkdir -p "$evidence_root" "$work_root"
chmod 0700 "$evidence_root" "$work_root"
workspace=$(mktemp -d "$work_root/s3-restore.XXXXXX")
cleanup() {
  clear_pg_env
  if [[ -n ${workspace:-} && -d $workspace ]]; then
    find "$workspace" -type f -exec shred --force --remove --zero -- {} + 2>/dev/null || true
    rm -rf -- "$workspace"
  fi
}
trap cleanup EXIT INT TERM

if [[ -n $requested_key ]]; then
  s3_key=$requested_key
else
  objects_result="$workspace/objects.json"
  aws s3api list-objects-v2 \
    --profile "$aws_profile" \
    --region "$aws_region" \
    --bucket "$SYNAPSE_BACKUP_BUCKET" \
    --prefix "$s3_prefix/" \
    --output json \
    --no-cli-pager > "$objects_result"
  s3_key=$(jq -er \
    '[.Contents[]?
      | select(.Size > 0)
      | select(.Key | endswith(".tar.gpg"))]
     | sort_by(.LastModified)
     | last.Key // error("no encrypted backup object exists under the prefix")' \
    "$objects_result")
fi

head_result="$workspace/head.json"
head_args=(
  s3api head-object
  --profile "$aws_profile"
  --region "$aws_region"
  --bucket "$SYNAPSE_BACKUP_BUCKET"
  --key "$s3_key"
  --checksum-mode ENABLED
  --output json
  --no-cli-pager
)
if [[ -n $requested_version ]]; then
  head_args+=(--version-id "$requested_version")
fi
aws "${head_args[@]}" > "$head_result"
version_id=$(jq -er '.VersionId' "$head_result")

remote_size=$(jq -er '.ContentLength' "$head_result")
remote_checksum_b64=$(jq -er '.ChecksumSHA256' "$head_result")
remote_metadata_sha=$(jq -er '.Metadata.sha256' "$head_result")
remote_encryption=$(jq -er '.ServerSideEncryption' "$head_result")
remote_lock_mode=$(jq -er '.ObjectLockMode' "$head_result")
retain_until=$(jq -er '.ObjectLockRetainUntilDate' "$head_result")
[[ $remote_encryption == AES256 ]] || {
  printf 'S3 encryption mismatch: expected AES256, got %s.\n' "$remote_encryption" >&2
  exit 1
}
[[ $remote_lock_mode == GOVERNANCE || $remote_lock_mode == COMPLIANCE ]] || {
  printf 'S3 Object Lock mode is not GOVERNANCE or COMPLIANCE: %s.\n' "$remote_lock_mode" >&2
  exit 1
}
[[ $(date --date="$retain_until" +%s) -gt $(date +%s) ]] || {
  printf 'S3 Object Lock retention has already expired: %s.\n' "$retain_until" >&2
  exit 1
}

encrypted_path="$workspace/backup.tar.gpg"
get_result="$workspace/get.json"
aws s3api get-object \
  --profile "$aws_profile" \
  --region "$aws_region" \
  --bucket "$SYNAPSE_BACKUP_BUCKET" \
  --key "$s3_key" \
  --version-id "$version_id" \
  --checksum-mode ENABLED \
  --output json \
  --no-cli-pager \
  "$encrypted_path" > "$get_result"
chmod 0600 "$encrypted_path"

local_size=$(stat -c '%s' "$encrypted_path")
local_sha256=$(sha256sum "$encrypted_path" | awk '{print $1}')
local_checksum_b64=$(openssl dgst -sha256 -binary "$encrypted_path" | base64 -w0)
[[ $local_size == "$remote_size" ]] || {
  printf 'Downloaded S3 object size mismatch.\n' >&2
  exit 1
}
[[ $local_checksum_b64 == "$remote_checksum_b64" ]] || {
  printf 'Downloaded S3 object checksum mismatch.\n' >&2
  exit 1
}
[[ $local_sha256 == "$remote_metadata_sha" ]] || {
  printf 'Downloaded S3 object metadata checksum mismatch.\n' >&2
  exit 1
}

archive_path="$workspace/backup.tar"
gpg \
  --batch \
  --quiet \
  --pinentry-mode loopback \
  --passphrase-file "$passphrase_file" \
  --output "$archive_path" \
  --decrypt "$encrypted_path"
chmod 0600 "$archive_path"

mapfile -t archive_members < <(tar --list --file="$archive_path")
[[ ${#archive_members[@]} -gt 0 ]] || {
  printf 'Decrypted archive is empty.\n' >&2
  exit 1
}
backup_name=
for member in "${archive_members[@]}"; do
  [[ $member != /* && ! $member =~ (^|/)\.\.(/|$) ]] || {
    printf 'Unsafe path in decrypted archive.\n' >&2
    exit 1
  }
  member_root=${member%%/*}
  [[ $member_root =~ ^synapse-logical-[0-9]{8}T[0-9]{6}Z$ ]] || {
    printf 'Unexpected top-level path in decrypted archive: %s\n' "$member_root" >&2
    exit 1
  }
  if [[ -z $backup_name ]]; then
    backup_name=$member_root
  elif [[ $backup_name != "$member_root" ]]; then
    printf 'Decrypted archive contains more than one backup directory.\n' >&2
    exit 1
  fi
done
while IFS= read -r entry; do
  [[ ${entry:0:1} == - || ${entry:0:1} == d ]] || {
    printf 'Decrypted archive contains a non-regular, non-directory entry.\n' >&2
    exit 1
  }
done < <(tar --list --verbose --file="$archive_path")

extract_root="$workspace/extracted"
mkdir -p "$extract_root"
tar \
  --extract \
  --file="$archive_path" \
  --directory="$extract_root" \
  --no-same-owner \
  --no-same-permissions
backup_dir="$extract_root/$backup_name"

restore_url=$RESTORE_DATABASE_URL
pg_env_from_url "$restore_url"
target_name=$PGDATABASE
clear_pg_env

SYNAPSE_RESTORE_DRILL=YES \
RESTORE_DATABASE_URL="$restore_url" \
SOURCE_DATABASE_URL="${SOURCE_DATABASE_URL:-}" \
  "$script_dir/postgres-restore-drill.sh" "$backup_dir"

drill_timestamp=$(date -u +%Y%m%dT%H%M%SZ)
evidence_path="$evidence_root/$drill_timestamp-s3-restore-drill.json"
jq -n \
  --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg bucket "$SYNAPSE_BACKUP_BUCKET" \
  --arg region "$aws_region" \
  --arg key "$s3_key" \
  --arg version_id "$version_id" \
  --arg sha256 "$local_sha256" \
  --arg target_database "$target_name" \
  '{
    completed_at: $completed_at,
    status: "passed",
    bucket: $bucket,
    region: $region,
    key: $key,
    version_id: $version_id,
    sha256: $sha256,
    target_database: $target_database
  }' > "$evidence_path"
chmod 0600 "$evidence_path"

printf 'Exact-version S3 restore drill passed: key=%s version=%s target=%s\n' \
  "$s3_key" "$version_id" "$target_name"
printf 'Recovery evidence: %s\n' "$evidence_path"
