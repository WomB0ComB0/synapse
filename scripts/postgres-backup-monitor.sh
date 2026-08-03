#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/postgres-backup-monitor.sh

Fails when the latest verified S3 backup-success record is missing, malformed,
in the future, or older than SYNAPSE_BACKUP_MAX_AGE_SECS.

Optional:
  SYNAPSE_BACKUP_EVIDENCE_ROOT  default:
    ~/.local/share/synapse/recovery-evidence
  SYNAPSE_BACKUP_STATUS_FILE    default:
    <evidence-root>/latest-s3-success.json
  SYNAPSE_BACKUP_MAX_AGE_SECS   default: 129600 (36 hours)
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -eq 0 ]] || { usage >&2; exit 2; }

for tool in date jq stat; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

evidence_root=${SYNAPSE_BACKUP_EVIDENCE_ROOT:-"$HOME/.local/share/synapse/recovery-evidence"}
status_file=${SYNAPSE_BACKUP_STATUS_FILE:-"$evidence_root/latest-s3-success.json"}
max_age_secs=${SYNAPSE_BACKUP_MAX_AGE_SECS:-129600}

[[ $max_age_secs =~ ^[0-9]+$ && $max_age_secs -ge 3600 ]] || {
  printf 'SYNAPSE_BACKUP_MAX_AGE_SECS must be an integer of at least 3600.\n' >&2
  exit 2
}
[[ -f $status_file && ! -L $status_file ]] || {
  printf 'No verified S3 backup-success record exists at %s.\n' "$status_file" >&2
  exit 1
}
status_mode=$(stat -c '%a' "$status_file")
[[ $status_mode == 400 || $status_mode == 600 ]] || {
  printf 'Backup status file must have mode 0400 or 0600, got %s.\n' "$status_mode" >&2
  exit 1
}

completed_at=$(jq -er 'select(.status == "success") | .completed_at' "$status_file")
completed_epoch=$(date --date="$completed_at" +%s)
now_epoch=$(date +%s)
age_secs=$((now_epoch - completed_epoch))

[[ $age_secs -ge -300 ]] || {
  printf 'Latest backup completion time is more than five minutes in the future: %s.\n' \
    "$completed_at" >&2
  exit 1
}
[[ $age_secs -le $max_age_secs ]] || {
  printf 'Latest verified S3 backup is stale: age=%ss limit=%ss completed=%s.\n' \
    "$age_secs" "$max_age_secs" "$completed_at" >&2
  exit 1
}

printf 'Latest verified S3 backup is current: age=%ss limit=%ss completed=%s.\n' \
  "$age_secs" "$max_age_secs" "$completed_at"
