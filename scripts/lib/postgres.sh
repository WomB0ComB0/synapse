#!/usr/bin/env bash
# Shared libpq environment helpers. Source from a Bash script; do not execute directly.

pg_env_from_url() {
  local uri=$1
  local -a fields=()
  command -v python3 >/dev/null 2>&1 || {
    printf 'python3 is required to parse PostgreSQL URLs without exposing them in process arguments\n' >&2
    return 2
  }
  mapfile -d '' -t fields < <(
    SYNAPSE_PG_URI="$uri" python3 - <<'PYCODE'
import os
import sys
from urllib.parse import parse_qs, unquote, urlparse

raw = os.environ.get("SYNAPSE_PG_URI", "")
parsed = urlparse(raw)
if parsed.scheme not in {"postgres", "postgresql"} or not parsed.hostname:
    raise SystemExit("PostgreSQL URL must use postgres:// or postgresql:// and include a host")
query = parse_qs(parsed.query, keep_blank_values=True)
allowed = {
    "application_name",
    "channel_binding",
    "connect_timeout",
    "options",
    "sslcert",
    "sslkey",
    "sslmode",
    "sslrootcert",
}
unknown = sorted(set(query) - allowed)
if unknown:
    raise SystemExit("unsupported PostgreSQL URL option(s): " + ", ".join(unknown))

def one(name: str) -> str:
    values = query.get(name, [""])
    if len(values) != 1:
        raise SystemExit(f"PostgreSQL URL option {name} must occur once")
    return values[0]

values = [
    parsed.hostname or "",
    str(parsed.port or ""),
    unquote(parsed.username or ""),
    unquote(parsed.password or ""),
    unquote(parsed.path.lstrip("/")),
    one("sslmode"),
    one("sslrootcert"),
    one("sslcert"),
    one("sslkey"),
    one("channel_binding"),
    one("connect_timeout"),
    one("application_name"),
    one("options"),
]
for value in values:
    sys.stdout.buffer.write(value.encode("utf-8") + b"\0")
PYCODE
  )
  [[ ${#fields[@]} -eq 13 ]] || {
    printf 'Could not parse PostgreSQL URL\n' >&2
    return 2
  }

  export PGHOST=${fields[0]}
  export PGPORT=${fields[1]}
  export PGUSER=${fields[2]}
  export PGPASSWORD=${fields[3]}
  export PGDATABASE=${fields[4]}
  # Only export optional libpq parameters when the URL actually specified them.
  # Exporting an empty value overrides libpq's own default and, for
  # channel_binding, is rejected outright by PostgreSQL 18 clients
  # ("invalid channel_binding value: \"\"").
  _pg_set_optional PGSSLMODE "${fields[5]}"
  _pg_set_optional PGSSLROOTCERT "${fields[6]}"
  _pg_set_optional PGSSLCERT "${fields[7]}"
  _pg_set_optional PGSSLKEY "${fields[8]}"
  _pg_set_optional PGCHANNELBINDING "${fields[9]}"
  export PGCONNECT_TIMEOUT=${fields[10]:-15}
  export PGAPPNAME=${fields[11]:-synapse-operations}
  _pg_set_optional PGOPTIONS "${fields[12]}"
}

# Export NAME=VALUE when VALUE is non-empty; otherwise unset NAME so libpq falls
# back to its own default instead of receiving an invalid empty string.
_pg_set_optional() {
  if [[ -n $2 ]]; then
    export "$1"="$2"
  else
    unset "$1"
  fi
}

clear_pg_env() {
  unset PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE PGSSLMODE PGSSLROOTCERT
  unset PGSSLCERT PGSSLKEY PGCHANNELBINDING PGCONNECT_TIMEOUT PGAPPNAME PGOPTIONS
}
