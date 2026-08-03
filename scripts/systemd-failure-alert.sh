#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/systemd-failure-alert.sh SYSTEMD_UNIT

Records a structured local failure event and optionally posts it to a webhook.
For a webhook, provide a mode-0400/0600 curl config containing the destination
URL and any authentication headers, either via SYNAPSE_ALERT_CURL_CONFIG or as a
systemd credential named alert-curl-config (auto-detected under
$CREDENTIALS_DIRECTORY). The URL and headers are never passed on the command line.

Optional:
  SYNAPSE_BACKUP_EVIDENCE_ROOT  default:
    ~/.local/share/synapse/recovery-evidence
  SYNAPSE_ALERT_CURL_CONFIG
  SYNAPSE_ALERT_REPEAT_SECS     default: 21600 (6 hours)
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then usage; exit 0; fi
[[ $# -eq 1 ]] || { usage >&2; exit 2; }

unit=$1
[[ $unit =~ ^[A-Za-z0-9_.@-]+\.service$ ]] || {
  printf 'Invalid systemd service name: %s\n' "$unit" >&2
  exit 2
}
for tool in date jq stat systemctl; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

evidence_root=${SYNAPSE_BACKUP_EVIDENCE_ROOT:-"$HOME/.local/share/synapse/recovery-evidence"}
alert_root="$evidence_root/alerts"
repeat_secs=${SYNAPSE_ALERT_REPEAT_SECS:-21600}
[[ $repeat_secs =~ ^[0-9]+$ && $repeat_secs -ge 300 ]] || {
  printf 'SYNAPSE_ALERT_REPEAT_SECS must be an integer of at least 300.\n' >&2
  exit 2
}

umask 077
mkdir -p "$alert_root"
chmod 0700 "$evidence_root" "$alert_root"

now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
now_epoch=$(date +%s)
result=$(systemctl show "$unit" --property=Result --value 2>/dev/null || true)
exec_status=$(systemctl show "$unit" --property=ExecMainStatus --value 2>/dev/null || true)
event_path="$alert_root/${now//:/-}-$unit.json"
marker_path="$alert_root/$unit.last-notified"

jq -n \
  --arg occurred_at "$now" \
  --arg unit "$unit" \
  --arg result "${result:-unknown}" \
  --arg exec_status "${exec_status:-unknown}" \
  --arg host "${HOSTNAME:-unknown}" \
  '{
    text: ("Synapse operation failed: " + $unit + " on " + $host),
    occurred_at: $occurred_at,
    unit: $unit,
    result: $result,
    exec_status: $exec_status,
    host: $host
  }' > "$event_path"
chmod 0600 "$event_path"

printf 'Synapse operation failed: unit=%s result=%s exec_status=%s evidence=%s\n' \
  "$unit" "${result:-unknown}" "${exec_status:-unknown}" "$event_path" >&2

alert_config=${SYNAPSE_ALERT_CURL_CONFIG:-}
if [[ -z $alert_config && -n ${CREDENTIALS_DIRECTORY:-} \
      && -f $CREDENTIALS_DIRECTORY/alert-curl-config ]]; then
  alert_config=$CREDENTIALS_DIRECTORY/alert-curl-config
fi
[[ -n $alert_config ]] || exit 0
command -v curl >/dev/null 2>&1 || {
  printf 'curl is required when an alert curl config is provided.\n' >&2
  exit 2
}
[[ -f $alert_config && ! -L $alert_config ]] || {
  printf 'Alert curl config must be a regular, non-symlink file.\n' >&2
  exit 2
}
config_mode=$(stat -c '%a' "$alert_config")
[[ $config_mode == 400 || $config_mode == 600 ]] || {
  printf 'Alert curl config must have mode 0400 or 0600, got %s.\n' "$config_mode" >&2
  exit 2
}

if [[ -f $marker_path ]]; then
  last_notified=$(cat "$marker_path")
  if [[ $last_notified =~ ^[0-9]+$ && $((now_epoch - last_notified)) -lt $repeat_secs ]]; then
    printf 'Webhook notification suppressed by the repeat interval.\n' >&2
    exit 0
  fi
fi

curl \
  --fail \
  --silent \
  --show-error \
  --request POST \
  --header 'Content-Type: application/json' \
  --config "$alert_config" \
  --data-binary "@$event_path" \
  --output /dev/null
printf '%s\n' "$now_epoch" > "$marker_path"
chmod 0600 "$marker_path"
