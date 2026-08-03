#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/local-rs256-configure-env.sh ENV_FILE [PUBLIC_KEY]

Atomically configures a Synapse runtime env file for production static-RS256
verification. Alternative JWT modes are removed. The private key is never read.

Environment:
  AUTH_JWT_ISSUER      default urn:synapse:local
  AUTH_JWT_AUDIENCE    default synapse-api
USAGE
}

if [[ ${1:-} == -h || ${1:-} == --help ]]; then
  usage
  exit 0
fi
[[ $# -ge 1 && $# -le 2 ]] || { usage >&2; exit 2; }

command -v openssl >/dev/null 2>&1 || {
  printf 'Missing required tool: openssl\n' >&2
  exit 2
}

env_file=$1
public_key=${2:-"${SYNAPSE_LOCAL_RS256_PUBLIC_KEY:-$HOME/.local/share/synapse/local-rs256/public.pem}"}
issuer=${AUTH_JWT_ISSUER:-urn:synapse:local}
audience=${AUTH_JWT_AUDIENCE:-synapse-api}

[[ -f $env_file && ! -L $env_file ]] || {
  printf 'ENV_FILE must be a regular, non-symlink file: %s\n' "$env_file" >&2
  exit 2
}
[[ -f $public_key && ! -L $public_key && -r $public_key ]] || {
  printf 'PUBLIC_KEY must be a readable, regular, non-symlink file: %s\n' "$public_key" >&2
  exit 2
}
[[ $issuer =~ ^[A-Za-z0-9._:/-]+$ && $audience =~ ^[A-Za-z0-9._:/-]+$ ]] || {
  printf 'Issuer and audience may contain only letters, digits, dot, underscore, colon, slash, or hyphen\n' >&2
  exit 2
}
openssl rsa -pubin -in "$public_key" -noout >/dev/null 2>&1 || {
  printf 'PUBLIC_KEY is not a valid RSA public key\n' >&2
  exit 2
}

public_escaped=$(awk 'BEGIN { first=1 } { if (!first) printf "\\n"; printf "%s", $0; first=0 }' "$public_key")
[[ -n $public_escaped ]] || {
  printf 'PUBLIC_KEY is empty\n' >&2
  exit 2
}

env_dir=$(dirname -- "$env_file")
temp=$(mktemp "$env_dir/.synapse-env.XXXXXX")
cleanup() {
  rm -f -- "${temp:-}"
}
trap cleanup EXIT
chmod 0600 "$temp"

awk '
  /^[[:space:]]*#?[[:space:]]*(SYNAPSE_ENV|AUTH_JWT_SECRET|AUTH_JWT_PUBLIC_KEY|AUTH_JWKS_URL|AUTH_JWT_AUDIENCE|AUTH_JWT_ISSUER|AUTH_REVOCATION_ENABLED)=/ {
    next
  }
  { print }
' "$env_file" > "$temp"

cat >> "$temp" <<EOF

# Local static-RS256 caller authentication. The signing key is intentionally
# stored outside this service environment; Synapse receives only the public key.
SYNAPSE_ENV=production
AUTH_JWT_PUBLIC_KEY="$public_escaped"
AUTH_JWT_AUDIENCE=$audience
AUTH_JWT_ISSUER=$issuer
AUTH_REVOCATION_ENABLED=true
EOF

mv -- "$temp" "$env_file"
temp=
printf 'Configured static RS256 verification in %s\n' "$env_file"
printf '  public key: %s\n' "$public_key"
printf '  issuer:     %s\n' "$issuer"
printf '  audience:   %s\n' "$audience"
