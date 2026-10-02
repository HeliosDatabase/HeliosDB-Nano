#!/bin/sh
#
# docker-entrypoint.sh — container entrypoint for the official
# ghcr.io/heliosdatabase/heliosdb-nano image.
#
# Passes every argument straight to `heliosdb-nano`, with these conveniences
# for `start` (the default command):
#
#   * Data directory and listen address. Unless `--data-dir`/`--memory` or
#     `--listen` are given, `start` gets `--data-dir $HELIOSDB_DATA_DIR`
#     (default /data) and `--listen $HELIOSDB_LISTEN` (default 0.0.0.0). Mount
#     your volume at /data (the only mount point the image pre-creates for
#     uid 999 — a named volume mounted at any other new path is root-owned);
#     a HELIOSDB_DATA_DIR below /data is created on start. The directory
#     must be writable by the container user (uid 999 by default); if it is
#     not, the container exits with an explanation instead of failing inside
#     RocksDB.
#
#   * Authentication. Nano refuses `trust` auth on a non-loopback listener, and
#     a container must listen on 0.0.0.0 to be reachable. When no `--auth` flag
#     is given, the password comes from HELIOSDB_PASSWORD or from the file named
#     by HELIOSDB_PASSWORD_FILE (Docker/Kubernetes secrets), and the server
#     starts with `--auth scram-sha-256`. With neither set the container exits
#     with an explanation instead of crash-looping. The password is handed to
#     the server through the environment or `--password-file`, never on the
#     command line, so it does not show up in the host's process table (`ps`).
#     (Release binaries up to 4.41.0 lack that support; with those the
#     entrypoint falls back to `--password` and prints a warning.)
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
    set -- start "$@"
  fi
fi

if [ "${1:-}" != "start" ]; then
  exec "$BIN" "$@"
fi
# `start --help` / `start -h`: nothing to set up.
for arg in "$@"; do
  case "$arg" in --help|-h) exec "$BIN" "$@" ;; esac
done

# ── inspect the `start` arguments ─────────────────────────────────────────
has_auth=0; has_password=0; has_tls=0; has_http_listen=0; has_listen=0
listen=""; memory=0; data_dir=""
prev=""
for arg in "$@"; do
  case "$prev" in
    --listen) listen="$arg" ;;
    -d|--data-dir) data_dir="$arg" ;;
  esac
  case "$arg" in
    --auth|--auth=*) has_auth=1 ;;
    --password|--password=*|--password-file|--password-file=*) has_password=1 ;;
    --tls-cert|--tls-cert=*) has_tls=1 ;;
    --http-listen|--http-listen=*) has_http_listen=1 ;;
    --listen) has_listen=1 ;;
    --listen=*) has_listen=1; listen="${arg#--listen=}" ;;
    --data-dir=*) data_dir="${arg#--data-dir=}" ;;
    -m|--memory) memory=1 ;;
  esac
  prev="$arg"
done

# ── data directory + listen address (honour HELIOSDB_DATA_DIR / _LISTEN) ──
if [ "$memory" -eq 0 ] && [ -z "$data_dir" ]; then
  data_dir="${HELIOSDB_DATA_DIR:-/data}"
  set -- "$@" --data-dir "$data_dir"
fi
if [ "$has_listen" -eq 0 ]; then
  listen="${HELIOSDB_LISTEN:-0.0.0.0}"
  set -- "$@" --listen "$listen"
fi

if [ "$memory" -eq 0 ]; then
  mkdir -p "$data_dir" 2>/dev/null || true
  if [ ! -d "$data_dir" ] || [ ! -w "$data_dir" ]; then
    uid="$(id -u)"; gid="$(id -g)"
    cat >&2 <<EOF
error: the data directory $data_dir is not writable by the container user (uid $uid, gid $gid).

Common causes and fixes:
  * A host bind mount (-v ./data:/data) that Docker created as root. Use a
    named volume instead (-v heliosdb_data:/data), or hand the directory to
    the container user first:  sudo chown -R $uid:$gid ./data
  * Kubernetes: set securityContext runAsUser/runAsGroup/fsGroup to 999 so
    the volume is writable, and mount it at the data directory.
  * A named volume mounted somewhere other than /data. Docker creates the
    mount point root-owned when it does not exist in the image, and only
    /data is pre-created for uid 999. Mount the volume at /data and, if you
    want a subdirectory, set HELIOSDB_DATA_DIR=/data/<name> (it is created
    on start): -v heliosdb_data:/data -e HELIOSDB_DATA_DIR=/data/heliosdb
  * HELIOSDB_DATA_DIR points at a path that is not a writable volume.
EOF
    exit 1
  fi
fi

# ── HTTP API listener (unauthenticated — loopback unless asked) ───────────
if [ "$has_http_listen" -eq 0 ]; then
  set -- "$@" --http-listen "${HELIOSDB_HTTP_LISTEN:-127.0.0.1}"
fi

# ── authentication ───────────────────────────────────────────────────────
# The password never goes on the command line, where every local user could
# read it with `ps`: HELIOSDB_PASSWORD stays in the environment (the server
# reads it itself) and a secret file is passed with --password-file.
if "$BIN" start --help 2>/dev/null | grep -q -- '--password-file'; then
  pw_off_argv=1
else
  pw_off_argv=0
fi
pw_file=""
if [ -z "${HELIOSDB_PASSWORD:-}" ] && [ -n "${HELIOSDB_PASSWORD_FILE:-}" ]; then
  [ -r "$HELIOSDB_PASSWORD_FILE" ] && [ -s "$HELIOSDB_PASSWORD_FILE" ] \
    || { echo "error: HELIOSDB_PASSWORD_FILE=$HELIOSDB_PASSWORD_FILE is not a readable, non-empty file" >&2; exit 1; }
  pw_file="$HELIOSDB_PASSWORD_FILE"
fi
if [ "$has_password" -eq 0 ]; then
  if [ -n "${HELIOSDB_PASSWORD:-}" ] || [ -n "$pw_file" ]; then
    [ "$has_auth" -eq 1 ] || set -- "$@" --auth scram-sha-256
    if [ "$pw_off_argv" -eq 1 ]; then
      if [ -n "$pw_file" ]; then
        set -- "$@" --password-file "$pw_file"
      else
        export HELIOSDB_PASSWORD
      fi
    else
      # Older release binary: --password is the only way in.
      echo "entrypoint: warning: this heliosdb-nano build only accepts the password on the command line, where other local users can see it with ps. Use an image of a release newer than 4.41.0." >&2
      if [ -n "$pw_file" ]; then
        password="$(head -n 1 "$pw_file")"
      else
        password="$HELIOSDB_PASSWORD"
      fi
      set -- "$@" --password "$password"
      unset password
    fi
  elif [ "$has_auth" -eq 0 ]; then
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
