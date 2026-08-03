#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/local-rs256-token.sh

Mints a short-lived RS256 JWT on standard output.

Environment:
  SYNAPSE_LOCAL_RS256_PRIVATE_KEY  signing key path
  AUTH_JWT_ISSUER                 required issuer claim
  AUTH_JWT_AUDIENCE               required audience claim
  SYNAPSE_PRINCIPAL               subject, default agent:ralph
  SYNAPSE_TENANT                  required tenant claim
  SYNAPSE_ROLE                    optional viewer, member, or admin claim
  SYNAPSE_LOCAL_RS256_TTL_SECS    token lifetime, 60-3600; default 900

The output is a bearer credential. Do not log or persist it.
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
  usage
  exit 0
fi
[[ $# -eq 0 ]] || { usage >&2; exit 2; }

for tool in openssl jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'Missing required tool: %s\n' "$tool" >&2
    exit 2
  }
done

private_key=${SYNAPSE_LOCAL_RS256_PRIVATE_KEY:-"$HOME/.local/share/synapse/local-rs256/private.pem"}
issuer=${AUTH_JWT_ISSUER:-}
audience=${AUTH_JWT_AUDIENCE:-}
principal=${SYNAPSE_PRINCIPAL:-agent:ralph}
tenant=${SYNAPSE_TENANT:-}
role=${SYNAPSE_ROLE:-}
ttl=${SYNAPSE_LOCAL_RS256_TTL_SECS:-900}

[[ $private_key == /* && -f $private_key && ! -L $private_key ]] || {
  printf 'Signing key must be an absolute, regular, non-symlink file: %s\n' "$private_key" >&2
  exit 2
}
mode=$(stat -c '%a' "$private_key" 2>/dev/null || true)
if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
  printf 'Signing key must have no group/other permissions\n' >&2
  exit 2
fi
[[ -n $issuer && -n $audience && -n $principal && -n $tenant ]] || {
  printf 'AUTH_JWT_ISSUER, AUTH_JWT_AUDIENCE, SYNAPSE_PRINCIPAL, and SYNAPSE_TENANT are required\n' >&2
  exit 2
}
if [[ ! $ttl =~ ^[0-9]+$ ]] || ((ttl < 60 || ttl > 3600)); then
  printf 'SYNAPSE_LOCAL_RS256_TTL_SECS must be an integer from 60 through 3600\n' >&2
  exit 2
fi
case $role in
  '' | viewer | member | admin) ;;
  *)
    printf 'SYNAPSE_ROLE must be viewer, member, admin, or unset\n' >&2
    exit 2
    ;;
esac
openssl rsa -in "$private_key" -check -noout >/dev/null 2>&1 || {
  printf 'Signing key is not a valid RSA private key\n' >&2
  exit 2
}

base64url() {
  openssl base64 -A | tr '+/' '-_' | tr -d '='
}

now=$(date +%s)
not_before=$((now - 5))
expires=$((now + ttl))
jti=$(openssl rand -hex 16)
header=$(printf '%s' '{"alg":"RS256","typ":"JWT"}' | base64url)
payload=$(
  jq -cn \
    --arg iss "$issuer" \
    --arg aud "$audience" \
    --arg sub "$principal" \
    --arg tenant "$tenant" \
    --arg role "$role" \
    --arg jti "$jti" \
    --argjson iat "$now" \
    --argjson nbf "$not_before" \
    --argjson exp "$expires" \
    '{
      iss: $iss,
      aud: $aud,
      sub: $sub,
      tenant: $tenant,
      teams: [],
      iat: $iat,
      nbf: $nbf,
      exp: $exp,
      jti: $jti
    } + if $role == "" then {} else {role: $role} end'
)
payload=$(printf '%s' "$payload" | base64url)
unsigned=$header.$payload
signature=$(
  printf '%s' "$unsigned" |
    openssl dgst -sha256 -sign "$private_key" -binary |
    base64url
)
printf '%s.%s\n' "$unsigned" "$signature"
