#!/usr/bin/env bash
# One reproducible WordPress run against HeliosDB Nano's MySQL listener.
#
#   tests/conformance/apps/wordpress/run.sh <path-to-heliosdb-nano> [output-dir]
#
# Starts the server in memory with a password (scram-sha-256) and its MySQL
# listener, starts the official WordPress image against it as the account
# 'wordpress' over plain TCP (the RSA public-key exchange of
# caching_sha2_password), then drives WordPress over HTTP: the install, the
# front page, a search, the monthly archive, a page, the feed, wp-login and
# wp-cron. Every statement WordPress sent and every error it was answered with
# are written to the output directory together with the versions and the counts.
#
# Exit status: 0 when the install completed and the front page rendered;
# 1 otherwise. The statement counts are in summary.txt either way.
#
# Needs: docker (the image pinned below), curl. The MySQL listener binds the
# docker bridge address so the container reaches it; nothing listens beyond it.
set -euo pipefail

NANO=${1:?usage: run.sh <path-to-heliosdb-nano> [output-dir]}
OUT=${2:-$(mktemp -d)}
# The official image, pinned by digest (override with WORDPRESS_IMAGE).
IMAGE=${WORDPRESS_IMAGE:-wordpress@sha256:a4447c867c8fd23389f3fd39e5de0d5b0a4536e1f95472fa580fb036a2688a0e}
DOCKER=${DOCKER:-docker}
ADMIN_PASSWORD=nano-admin-$RANDOM$RANDOM
WP_DB_PASSWORD=wp-db-$RANDOM$RANDOM
NAME=nano-wordpress-$$

mkdir -p "$OUT"
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'; }
BRIDGE=$($DOCKER network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')
MYSQL_PORT=$(free_port); PG_PORT=$(free_port); HTTP_PORT=$(free_port); WP_PORT=$(free_port)

cleanup() {
  $DOCKER rm -f "$NAME" >/dev/null 2>&1 || true
  [ -n "${NANO_PID:-}" ] && kill "$NANO_PID" 2>/dev/null || true
}
trap cleanup EXIT

# 1. The server, with the statement log on.
RUST_LOG=warn,mysql_statements=debug "$NANO" start --memory \
  --auth scram-sha-256 --password "$ADMIN_PASSWORD" \
  --port "$PG_PORT" --http-port "$HTTP_PORT" \
  --mysql --mysql-listen "$BRIDGE:$MYSQL_PORT" > "$OUT/nano.log" 2>&1 &
NANO_PID=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/$BRIDGE/$MYSQL_PORT") 2>/dev/null && break
  kill -0 "$NANO_PID" 2>/dev/null || { echo "the server exited at start; see $OUT/nano.log"; exit 1; }
  sleep 0.2
done

# 2. WordPress's own account, made with the MySQL statement for it. The image's
#    PHP is the client: the same driver WordPress uses.
$DOCKER run --rm "$IMAGE" php -r '
  mysqli_report(MYSQLI_REPORT_ERROR | MYSQLI_REPORT_STRICT);
  $db = new mysqli($argv[1], "helios", $argv[3], "", (int)$argv[2]);
  $db->query("CREATE USER \x27wordpress\x27@\x27%\x27 IDENTIFIED BY \x27" . $argv[4] . "\x27");
  echo "client: ", $db->client_info, "\nserver: ", $db->server_info, "\n";
' "$BRIDGE" "$MYSQL_PORT" "$ADMIN_PASSWORD" "$WP_DB_PASSWORD" > "$OUT/account.txt" 2>&1 \
  || { echo "could not create the WordPress account; see $OUT/account.txt"; exit 1; }

# 3. WordPress.
$DOCKER run --rm -d --name "$NAME" -p "127.0.0.1:$WP_PORT:80" \
  -e WORDPRESS_DB_HOST="$BRIDGE:$MYSQL_PORT" -e WORDPRESS_DB_USER=wordpress \
  -e WORDPRESS_DB_PASSWORD="$WP_DB_PASSWORD" -e WORDPRESS_DB_NAME=wordpress \
  -e WORDPRESS_DEBUG=1 "$IMAGE" > /dev/null
BASE="http://127.0.0.1:$WP_PORT"
for _ in $(seq 1 150); do
  curl -fsS -o /dev/null "$BASE/wp-admin/install.php" 2>/dev/null && break
  sleep 0.4
done

fetch() { # name, url, [curl args...]
  local name=$1 url=$2; shift 2
  curl -sS -L -o "$OUT/$name.html" -w "$name %{http_code}\n" "$@" "$url" >> "$OUT/http.txt" || echo "$name curl-failed" >> "$OUT/http.txt"
}
: > "$OUT/http.txt"
fetch install-form "$BASE/wp-admin/install.php"
fetch install "$BASE/wp-admin/install.php?step=2" \
  --data-urlencode "weblog_title=Nano conformance" --data-urlencode "user_name=admin" \
  --data-urlencode "admin_password=$ADMIN_PASSWORD-wp" --data-urlencode "admin_password2=$ADMIN_PASSWORD-wp" \
  --data-urlencode "pw_weak=1" --data-urlencode "admin_email=admin@example.com" --data-urlencode "blog_public=0"
fetch front "$BASE/"
fetch post "$BASE/?p=1"
fetch page "$BASE/?page_id=2"
fetch search "$BASE/?s=hello"
fetch archive "$BASE/?m=$(date -u +%Y%m)"
fetch category "$BASE/?cat=1"
fetch feed "$BASE/?feed=rss2"
fetch login "$BASE/wp-login.php"
fetch cron "$BASE/wp-cron.php"
fetch rest-posts "$BASE/?rest_route=/wp/v2/posts"

# 4. Evidence.
{
  echo "date: $(date -u +%FT%TZ)"
  echo "image: $IMAGE ($($DOCKER image inspect "$IMAGE" --format '{{index .RepoDigests 0}}' 2>/dev/null || echo 'no digest'))"
  $DOCKER exec "$NAME" php -r 'echo "php: ", PHP_VERSION, "\n";' 2>/dev/null || true
  $DOCKER exec "$NAME" sh -c 'grep "^\$wp_version" /var/www/html/wp-includes/version.php' 2>/dev/null || true
  echo "nano: $("$NANO" --version 2>/dev/null | head -1)"
  cat "$OUT/account.txt"
} > "$OUT/versions.txt"
$DOCKER logs "$NAME" > "$OUT/wordpress.log" 2>&1 || true
grep -a 'mysql_statements' "$OUT/nano.log" | grep -a ' query: ' | sed 's/^.* query: //' > "$OUT/statements.sql" || true
grep -a 'mysql_statements' "$OUT/nano.log" | grep -a ' error [0-9]*: ' | sed 's/^.* error \([0-9]*\): /\1 /' > "$OUT/errors.txt" || true
sort "$OUT/errors.txt" | uniq -c | sort -rn > "$OUT/errors-by-message.txt" || true

STATEMENTS=$(wc -l < "$OUT/statements.sql"); ERRORS=$(wc -l < "$OUT/errors.txt")
INSTALLED=no; grep -qi 'success' "$OUT/install.html" 2>/dev/null && INSTALLED=yes
FRONT=no; grep -q 'Nano conformance' "$OUT/front.html" 2>/dev/null && FRONT=yes
{
  echo "statements: $STATEMENTS"
  echo "errors: $ERRORS"
  echo "distinct error messages: $(wc -l < "$OUT/errors-by-message.txt")"
  echo "install completed: $INSTALLED"
  echo "front page rendered: $FRONT"
  cat "$OUT/http.txt"
} > "$OUT/summary.txt"
cat "$OUT/summary.txt"
echo "output: $OUT"
[ "$INSTALLED" = yes ] && [ "$FRONT" = yes ]
