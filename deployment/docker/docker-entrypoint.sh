#!/bin/sh
#
# docker-entrypoint.sh — container entrypoint for the official
# ghcr.io/heliosdatabase/heliosdb-nano image.
#
# Passes every argument straight to `heliosdb-nano`, with two conveniences for
# `start` (the default command):
#
#   * Authentication. Nano refuses `trust` auth on a non-loopback listener, and
#     a container must listen on 0.0.0.0 to be reachable. When no `--auth` flag
#     is given, the password comes from HELIOSDB_PASSWORD or from the file named
#     by HELIOSDB_PASSWORD_FILE (Docker/Kubernetes secrets), and the server
#     starts with `--auth scram-sha-256`. With neither set the container exits
#     with an explanation instead of crash-looping.
#
#   * HTTP API. The HTTP listener (port 8080: /health, REST, branches, vector
#     stores) does NOT use the PostgreSQL password, so it is bound to
#     127.0.0.1 inside the container unless you pass `--http-listen` or set
#     HELIOSDB_HTTP_LISTEN=0.0.0.0. The image HEALTHCHECK works either way.
#     Only publish 8080 on a trusted network.
#
#   * TLS. When no `--tls-cert` flag is given, a self-signed certificate is
#     generated once under $HELIOSDB_TLS_DIR (default /data/tls) and the server
#     offers TLS, so `sslmode=require` works out of the box. Bring your own
#     certificate with HELIOSDB_TLS_CERT + HELIOSDB_TLS_KEY (paths inside the
#     container), or set HELIOSDB_TLS=off to disable TLS.
#
# Any other subcommand (`repl`, `--version`, `dump`, …) is executed unchanged.

set -eu

BIN=heliosdb-nano

# `docker run IMAGE heliosdb-nano …` → drop the redundant program name.
if [ "${1:-}" = "$BIN" ]; then
  shift
fi
# No arguments, or only options (`docker run IMAGE --port 6543`) → `start …`.
if [ "$#" -eq 0 ] || [ "${1#-}" != "$1" ]; then
  if [ "$#" -eq 0 ] || { [ "$1" != "--version" ] && [ "$1" != "-V" ] \
                         && [ "$1" != "--help" ] && [ "$1" != "-h" ]; }; then
    set -- start --data-dir "${HELIOSDB_DATA_DIR:-/data}" --listen 0.0.0.0 "$@"
  fi
fi

if [ "${1:-}" != "start" ]; then
  exec "$BIN" "$@"
fi

# ── inspect the `start` arguments ─────────────────────────────────────────
has_auth=0; has_tls=0; has_http_listen=0; listen="127.0.0.1"; memory=0; data_dir=""
prev=""
for arg in "$@"; do
  case "$prev" in
    --listen) listen="$arg" ;;
    -d|--data-dir) data_dir="$arg" ;;
  esac
  case "$arg" in
    --auth|--auth=*) has_auth=1 ;;
    --tls-cert|--tls-cert=*) has_tls=1 ;;
    --http-listen|--http-listen=*) has_http_listen=1 ;;
    --listen=*) listen="${arg#--listen=}" ;;
    --data-dir=*) data_dir="${arg#--data-dir=}" ;;
    -m|--memory) memory=1 ;;
  esac
  prev="$arg"
done

# ── HTTP API listener (unauthenticated — loopback unless asked) ───────────
if [ "$has_http_listen" -eq 0 ]; then
  set -- "$@" --http-listen "${HELIOSDB_HTTP_LISTEN:-127.0.0.1}"
fi

# ── authentication ───────────────────────────────────────────────────────
if [ "$has_auth" -eq 0 ]; then
  password="${HELIOSDB_PASSWORD:-}"
  if [ -z "$password" ] && [ -n "${HELIOSDB_PASSWORD_FILE:-}" ]; then
    [ -r "$HELIOSDB_PASSWORD_FILE" ] \
      || { echo "error: HELIOSDB_PASSWORD_FILE=$HELIOSDB_PASSWORD_FILE is not readable" >&2; exit 1; }
    password="$(cat "$HELIOSDB_PASSWORD_FILE")"
  fi
  if [ -n "$password" ]; then
    set -- "$@" --auth scram-sha-256 --password "$password"
    unset password
  else
    case "$listen" in
      127.0.0.1|::1|localhost) : ;;
      *)
        cat >&2 <<'EOF'
error: no password configured.

HeliosDB Nano only allows passwordless (trust) connections on loopback, and a
container listens on 0.0.0.0. Set a password, for example:

  docker run -d -p 5432:5432 -e HELIOSDB_PASSWORD=change-me \
    -v heliosdb_data:/data ghcr.io/heliosdatabase/heliosdb-nano:latest

or mount a secret file and set HELIOSDB_PASSWORD_FILE=/run/secrets/<name>.
EOF
        exit 1
        ;;
    esac
  fi
fi

# ── TLS ──────────────────────────────────────────────────────────────────
tls_mode="${HELIOSDB_TLS:-auto}"
if [ "$has_tls" -eq 0 ] && [ "$tls_mode" != "off" ]; then
  if [ -n "${HELIOSDB_TLS_CERT:-}" ] || [ -n "${HELIOSDB_TLS_KEY:-}" ]; then
    [ -r "${HELIOSDB_TLS_CERT:-}" ] && [ -r "${HELIOSDB_TLS_KEY:-}" ] \
      || { echo "error: HELIOSDB_TLS_CERT and HELIOSDB_TLS_KEY must both name readable files" >&2; exit 1; }
    set -- "$@" --tls-cert "$HELIOSDB_TLS_CERT" --tls-key "$HELIOSDB_TLS_KEY"
  else
    if [ "$memory" -eq 1 ] || [ -z "$data_dir" ]; then
      tls_dir="${HELIOSDB_TLS_DIR:-/tmp/heliosdb-tls}"
    else
      tls_dir="${HELIOSDB_TLS_DIR:-$data_dir/tls}"
    fi
    cert="$tls_dir/server.crt"; key="$tls_dir/server.key"
    if [ ! -s "$cert" ] || [ ! -s "$key" ]; then
      if ( umask 077 && mkdir -p "$tls_dir" ) 2>/dev/null \
         && ( umask 077 && openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
                -subj "/CN=${HELIOSDB_TLS_CN:-heliosdb-nano}" \
                -keyout "$key" -out "$cert" ) >/dev/null 2>&1; then
        echo "entrypoint: generated a self-signed TLS certificate in $tls_dir" >&2
      else
        echo "entrypoint: warning: could not create a TLS certificate in $tls_dir; starting without TLS" >&2
        cert=""
      fi
    fi
    if [ -n "$cert" ]; then
      set -- "$@" --tls-cert "$cert" --tls-key "$key"
    fi
  fi
fi

exec "$BIN" "$@"
