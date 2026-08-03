#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

usage() {
  cat <<'USAGE'
Usage: MIGRATION_DATABASE_URL=... SYNAPSE_BACKUP_BUCKET=... \
  scripts/postgres-backup-s3.sh

Creates a verified logical PostgreSQL backup, packages and encrypts it with GPG,
uploads the encrypted archive to S3, and verifies the stored checksum, size,
server-side encryption, and Object Lock retention.

Required:
  MIGRATION_DATABASE_URL
  SYNAPSE_BACKUP_BUCKET

Optional:
  AWS_PROFILE                         default: synapse-backup
  AWS_REGION                          default: us-west-2
  SYNAPSE_BACKUP_S3_PREFIX            default: backups
  SYNAPSE_BACKUP_PASSPHRASE_FILE      default:
    ~/.local/share/synapse/backup-keys/synapse-backup.passphrase
  SYNAPSE_BACKUP_ROOT                 default:
    ~/.local/share/synapse/backups
  SYNAPSE_ENCRYPTED_BACKUP_ROOT       default:
    ~/.local/share/synapse/encrypted-backups
  SYNAPSE_BACKUP_EVIDENCE_ROOT        default:
    ~/.local/share/synapse/recovery-evidence
  SYNAPSE_BACKUP_STATUS_FILE          default:
    <evidence-root>/latest-s3-success.json
  SYNAPSE_BACKUP_LOCAL_KEEP           default: 7
    Number of most-recent encrypted local copies to retain after upload.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -eq 0 ]] || { usage >&2; exit 2; }
: "${MIGRATION_DATABASE_URL:?MIGRATION_DATABASE_URL must be set in the process environment}"
: "${SYNAPSE_BACKUP_BUCKET:?SYNAPSE_BACKUP_BUCKET must be set in the process environment}"

for tool in aws base64 date find gpg jq openssl sha256sum stat tar; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

aws_profile=${AWS_PROFILE:-synapse-backup}
aws_region=${AWS_REGION:-us-west-2}
s3_prefix=${SYNAPSE_BACKUP_S3_PREFIX:-backups}
passphrase_file=${SYNAPSE_BACKUP_PASSPHRASE_FILE:-"$HOME/.local/share/synapse/backup-keys/synapse-backup.passphrase"}
backup_root=${SYNAPSE_BACKUP_ROOT:-"$HOME/.local/share/synapse/backups"}
encrypted_root=${SYNAPSE_ENCRYPTED_BACKUP_ROOT:-"$HOME/.local/share/synapse/encrypted-backups"}
evidence_root=${SYNAPSE_BACKUP_EVIDENCE_ROOT:-"$HOME/.local/share/synapse/recovery-evidence"}
status_file=${SYNAPSE_BACKUP_STATUS_FILE:-"$evidence_root/latest-s3-success.json"}
local_keep=${SYNAPSE_BACKUP_LOCAL_KEEP:-7}

[[ $local_keep =~ ^[0-9]+$ && $local_keep -ge 1 ]] || {
  printf 'SYNAPSE_BACKUP_LOCAL_KEEP must be an integer of at least 1.\n' >&2
  exit 2
}
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
mkdir -p "$backup_root" "$encrypted_root" "$evidence_root"
chmod 0700 "$backup_root" "$encrypted_root" "$evidence_root"
[[ $(dirname -- "$status_file") == "$evidence_root" ]] || {
  printf 'SYNAPSE_BACKUP_STATUS_FILE must be directly under the evidence root.\n' >&2
  exit 2
}

staging_root=$(mktemp -d "$backup_root/.s3-staging.XXXXXX")
tmp_encrypted=$(mktemp "$encrypted_root/.synapse-backup.XXXXXX.gpg")
put_result=$(mktemp)
head_result=$(mktemp)
tmp_status=
cleanup() {
  if [[ -n ${staging_root:-} ]]; then rm -rf -- "$staging_root"; fi
  if [[ -n ${tmp_encrypted:-} ]]; then rm -f -- "$tmp_encrypted"; fi
  if [[ -n ${tmp_status:-} ]]; then rm -f -- "$tmp_status"; fi
  rm -f -- "$put_result" "$head_result"
}
trap cleanup EXIT

"$script_dir/postgres-backup.sh" "$staging_root"

shopt -s nullglob
backup_dirs=("$staging_root"/synapse-logical-*)
shopt -u nullglob
[[ ${#backup_dirs[@]} -eq 1 ]] || {
  printf 'Expected exactly one staged logical backup, found %s.\n' "${#backup_dirs[@]}" >&2
  exit 1
}

staged_backup=${backup_dirs[0]}
backup_name=$(basename -- "$staged_backup")
final_backup="$backup_root/$backup_name"
[[ ! -e $final_backup ]] || {
  printf 'Refusing to overwrite existing backup: %s\n' "$final_backup" >&2
  exit 1
}
mv -- "$staged_backup" "$final_backup"
rmdir -- "$staging_root"
staging_root=

encrypted_path="$encrypted_root/$backup_name.tar.gpg"
checksum_path="$encrypted_path.sha256"
[[ ! -e $encrypted_path && ! -e $checksum_path ]] || {
  printf 'Refusing to overwrite existing encrypted backup: %s\n' "$encrypted_path" >&2
  exit 1
}

printf 'Encrypting backup archive...\n'
tar --create --file=- --directory="$backup_root" "$backup_name" |
  gpg \
    --batch \
    --quiet \
    --yes \
    --pinentry-mode loopback \
    --passphrase-file "$passphrase_file" \
    --symmetric \
    --cipher-algo AES256 \
    --compress-algo none \
    --output "$tmp_encrypted"

# Packet authentication plus a tar listing proves the encrypted archive is
# decryptable and structurally complete before it leaves the host.
gpg \
  --batch \
  --quiet \
  --pinentry-mode loopback \
  --passphrase-file "$passphrase_file" \
  --decrypt "$tmp_encrypted" |
  tar --list --file=- >/dev/null

mv -- "$tmp_encrypted" "$encrypted_path"
tmp_encrypted=
sha256sum "$encrypted_path" > "$checksum_path"
chmod 0600 "$encrypted_path" "$checksum_path"

local_sha256=$(awk '{print $1}' "$checksum_path")
local_checksum_b64=$(openssl dgst -sha256 -binary "$encrypted_path" | base64 -w0)
local_size=$(stat -c '%s' "$encrypted_path")
backup_timestamp=${backup_name#synapse-logical-}
[[ $backup_timestamp =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || {
  printf 'Unexpected backup timestamp in %s.\n' "$backup_name" >&2
  exit 1
}
s3_key="$s3_prefix/${backup_timestamp:0:4}/${backup_timestamp:4:2}/${backup_timestamp:6:2}/$backup_name.tar.gpg"

printf 'Uploading encrypted archive to s3://%s/%s...\n' "$SYNAPSE_BACKUP_BUCKET" "$s3_key"
aws s3api put-object \
  --profile "$aws_profile" \
  --region "$aws_region" \
  --bucket "$SYNAPSE_BACKUP_BUCKET" \
  --key "$s3_key" \
  --body "$encrypted_path" \
  --content-type application/octet-stream \
  --server-side-encryption AES256 \
  --checksum-algorithm SHA256 \
  --checksum-sha256 "$local_checksum_b64" \
  --metadata "sha256=$local_sha256" \
  --output json \
  --no-cli-pager > "$put_result"

version_id=$(jq -er '.VersionId' "$put_result")
aws s3api head-object \
  --profile "$aws_profile" \
  --region "$aws_region" \
  --bucket "$SYNAPSE_BACKUP_BUCKET" \
  --key "$s3_key" \
  --version-id "$version_id" \
  --checksum-mode ENABLED \
  --output json \
  --no-cli-pager > "$head_result"

remote_size=$(jq -er '.ContentLength' "$head_result")
remote_checksum_b64=$(jq -er '.ChecksumSHA256' "$head_result")
remote_encryption=$(jq -er '.ServerSideEncryption' "$head_result")
remote_lock_mode=$(jq -er '.ObjectLockMode' "$head_result")
retain_until=$(jq -er '.ObjectLockRetainUntilDate' "$head_result")
remote_metadata_sha=$(jq -er '.Metadata.sha256' "$head_result")

[[ $remote_size == "$local_size" ]] || {
  printf 'S3 size mismatch: local=%s remote=%s.\n' "$local_size" "$remote_size" >&2
  exit 1
}
[[ $remote_checksum_b64 == "$local_checksum_b64" ]] || {
  printf 'S3 SHA-256 checksum mismatch.\n' >&2
  exit 1
}
[[ $remote_metadata_sha == "$local_sha256" ]] || {
  printf 'S3 checksum metadata mismatch.\n' >&2
  exit 1
}
[[ $remote_encryption == AES256 ]] || {
  printf 'S3 encryption mismatch: expected AES256, got %s.\n' "$remote_encryption" >&2
  exit 1
}
[[ $remote_lock_mode == GOVERNANCE || $remote_lock_mode == COMPLIANCE ]] || {
  printf 'S3 Object Lock mismatch: expected GOVERNANCE or COMPLIANCE, got %s.\n' \
    "$remote_lock_mode" >&2
  exit 1
}
retain_until_epoch=$(date --date="$retain_until" +%s)
minimum_retain_epoch=$(date --date='+29 days' +%s)
[[ $retain_until_epoch -ge $minimum_retain_epoch ]] || {
  printf 'S3 Object Lock retention is shorter than 29 days: %s.\n' "$retain_until" >&2
  exit 1
}

evidence_path="$evidence_root/$backup_name-s3.json"
jq -n \
  --arg verified_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg bucket "$SYNAPSE_BACKUP_BUCKET" \
  --arg region "$aws_region" \
  --arg key "$s3_key" \
  --arg version_id "$version_id" \
  --arg sha256 "$local_sha256" \
  --argjson size_bytes "$local_size" \
  --arg server_side_encryption "$remote_encryption" \
  --arg object_lock_mode "$remote_lock_mode" \
  --arg retain_until "$retain_until" \
  '{
    verified_at: $verified_at,
    bucket: $bucket,
    region: $region,
    key: $key,
    version_id: $version_id,
    sha256: $sha256,
    size_bytes: $size_bytes,
    server_side_encryption: $server_side_encryption,
    object_lock_mode: $object_lock_mode,
    retain_until: $retain_until
  }' > "$evidence_path"
chmod 0600 "$evidence_path"

tmp_status=$(mktemp "$evidence_root/.latest-s3-success.XXXXXX")
jq \
  --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  '. + {completed_at: $completed_at, status: "success"}' \
  "$evidence_path" > "$tmp_status"
chmod 0600 "$tmp_status"
mv -- "$tmp_status" "$status_file"
tmp_status=

# The plaintext logical dump has been encrypted, uploaded, and verified against
# the immutable S3 copy; it must not linger unencrypted on local disk.
rm -rf -- "$final_backup"

# Retain only the newest encrypted local copies. Names sort chronologically
# because they embed a fixed-width UTC timestamp.
mapfile -t local_encrypted < <(
  find "$encrypted_root" -maxdepth 1 -type f -name 'synapse-logical-*.tar.gpg' \
    -printf '%f\n' | sort
)
if [[ ${#local_encrypted[@]} -gt $local_keep ]]; then
  prune_count=$(( ${#local_encrypted[@]} - local_keep ))
  for old in "${local_encrypted[@]:0:prune_count}"; do
    rm -f -- "$encrypted_root/$old" "$encrypted_root/$old.sha256"
  done
fi

trap - EXIT
cleanup
printf 'S3 backup verified: s3://%s/%s (version %s)\n' \
  "$SYNAPSE_BACKUP_BUCKET" "$s3_key" "$version_id"
printf 'Recovery evidence: %s\n' "$evidence_path"
