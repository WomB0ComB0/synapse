#!/usr/bin/env bash
set -euo pipefail

if (($# == 0)); then
  printf 'Usage: scripts/run-with-local-rs256-token.sh COMMAND [ARG ...]\n' >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
export SYNAPSE_TOKEN
SYNAPSE_TOKEN=$("$script_dir/local-rs256-token.sh")
unset SYNAPSE_LOCAL_RS256_PRIVATE_KEY SYNAPSE_LOCAL_RS256_KEY_DIR
unset SYNAPSE_LOCAL_RS256_TTL_SECS SYNAPSE_ROLE AUTH_JWT_ISSUER AUTH_JWT_AUDIENCE
exec "$@"
