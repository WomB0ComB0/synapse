#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/local-observability-init.sh [ENV_FILE]

Creates the mode-0600 environment file used by the local Grafana container.
Existing credentials are validated and never replaced or printed.

Default:
  ENV_FILE=$HOME/.config/synapse/observability.env
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
  usage
  exit 0
fi
[[ $# -le 1 ]] || { usage >&2; exit 2; }
command -v openssl >/dev/null 2>&1 || {
  printf 'Missing required tool: openssl\n' >&2
  exit 2
}

env_file=${1:-"${SYNAPSE_OBSERVABILITY_ENV_FILE:-$HOME/.config/synapse/observability.env}"}
[[ $env_file == /* ]] || {
  printf 'ENV_FILE must be an absolute path\n' >&2
  exit 2
}
[[ ! -L $env_file ]] || {
  printf 'Refusing symbolic-link environment file: %s\n' "$env_file" >&2
  exit 2
}

validate_existing() {
  local mode key count
  [[ -f $env_file ]] || {
    printf 'Observability environment path is not a regular file: %s\n' "$env_file" >&2
    return 1
  }
  mode=$(stat -c '%a' "$env_file")
  if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
    printf 'Observability environment must have no group/other permissions\n' >&2
    return 1
  fi
  for key in GRAFANA_ADMIN_USER GRAFANA_ADMIN_PASSWORD GRAFANA_SECRET_KEY; do
    count=$(awk -F= -v key="$key" '$1 == key && length($2) > 0 {count++} END {print count+0}' "$env_file")
    [[ $count == 1 ]] || {
      printf 'Observability environment must contain exactly one non-empty %s assignment\n' "$key" >&2
      return 1
    }
  done
}

if [[ -e $env_file ]]; then
  validate_existing
  printf 'Existing observability credentials are valid: %s\n' "$env_file"
  exit 0
fi

umask 077
env_dir=$(dirname -- "$env_file")
mkdir -p -- "$env_dir"
chmod 0700 -- "$env_dir"
temp=$(mktemp "$env_dir/.observability.XXXXXX")
cleanup() {
  rm -f -- "${temp:-}"
}
trap cleanup EXIT

password=$(openssl rand -hex 24)
secret_key=$(openssl rand -hex 32)
printf 'GRAFANA_ADMIN_USER=synapse-admin\n' > "$temp"
printf 'GRAFANA_ADMIN_PASSWORD=%s\n' "$password" >> "$temp"
printf 'GRAFANA_SECRET_KEY=%s\n' "$secret_key" >> "$temp"
chmod 0600 "$temp"
mv -- "$temp" "$env_file"
temp=
unset password secret_key

printf 'Created observability credentials: %s\n' "$env_file"
printf 'Grafana user: synapse-admin\n'
