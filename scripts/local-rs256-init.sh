#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/local-rs256-init.sh [KEY_DIRECTORY]

Creates a 3072-bit RSA signing key and matching public key. The private key is
created with mode 0600 and is never printed. Existing keys are validated and
left unchanged.

Default:
  KEY_DIRECTORY=$HOME/.local/share/synapse/local-rs256
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

key_dir=${1:-"${SYNAPSE_LOCAL_RS256_KEY_DIR:-$HOME/.local/share/synapse/local-rs256}"}
[[ $key_dir == /* ]] || {
  printf 'KEY_DIRECTORY must be an absolute path\n' >&2
  exit 2
}
[[ ! -L $key_dir ]] || {
  printf 'Refusing symbolic-link key directory: %s\n' "$key_dir" >&2
  exit 2
}

umask 077
mkdir -p -- "$key_dir"
chmod 0700 -- "$key_dir"
private_key=$key_dir/private.pem
public_key=$key_dir/public.pem

validate_existing() {
  [[ -f $private_key && ! -L $private_key && -f $public_key && ! -L $public_key ]] || {
    printf 'Key directory contains an incomplete or unsafe keypair: %s\n' "$key_dir" >&2
    return 1
  }
  local mode derived
  mode=$(stat -c '%a' "$private_key")
  if [[ ! $mode =~ ^[0-7][0-7]?[0-7]?$ ]] || (( (8#$mode & 077) != 0 )); then
    printf 'Private key must have no group/other permissions: %s\n' "$private_key" >&2
    return 1
  fi
  openssl rsa -in "$private_key" -check -noout >/dev/null 2>&1 || {
    printf 'Existing private key is invalid: %s\n' "$private_key" >&2
    return 1
  }
  derived=$(mktemp "$key_dir/.public-check.XXXXXX")
  if ! openssl pkey -in "$private_key" -pubout -out "$derived" 2>/dev/null; then
    rm -f -- "$derived"
    printf 'Could not derive the existing public key\n' >&2
    return 1
  fi
  if ! cmp -s "$derived" "$public_key"; then
    rm -f -- "$derived"
    printf 'Existing public key does not match the private key\n' >&2
    return 1
  fi
  rm -f -- "$derived"
}

if [[ -e $private_key || -e $public_key ]]; then
  validate_existing
  printf 'Existing local RS256 keypair is valid\n'
  printf '  private: %s\n' "$private_key"
  printf '  public:  %s\n' "$public_key"
  exit 0
fi

private_tmp=$(mktemp "$key_dir/.private.XXXXXX")
public_tmp=$(mktemp "$key_dir/.public.XXXXXX")
cleanup() {
  rm -f -- "${private_tmp:-}" "${public_tmp:-}"
}
trap cleanup EXIT

openssl genpkey \
  -algorithm RSA \
  -pkeyopt rsa_keygen_bits:3072 \
  -out "$private_tmp" >/dev/null 2>&1
openssl pkey -in "$private_tmp" -pubout -out "$public_tmp" 2>/dev/null
chmod 0600 "$private_tmp"
chmod 0644 "$public_tmp"
mv -- "$private_tmp" "$private_key"
mv -- "$public_tmp" "$public_key"
private_tmp=
public_tmp=

printf 'Created local RS256 keypair\n'
printf '  private: %s\n' "$private_key"
printf '  public:  %s\n' "$public_key"
