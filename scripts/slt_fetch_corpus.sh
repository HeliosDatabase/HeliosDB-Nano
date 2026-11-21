#!/usr/bin/env bash
# Fetch the public SQL Logic Test corpus at a pinned revision and verify it.
#
# The corpus is never stored in this repository. It is downloaded into a cache directory
# (SLT_CORPUS_HOME, default ~/.cache/heliosdb-nano/slt-corpus), checked against a pinned
# digest of its test scripts, and its `test` directory is printed on the last line:
#
#     export SLT_CORPUS_DIR="$(scripts/slt_fetch_corpus.sh | tail -1)"
#
# Needs: curl, tar, sha256sum. Network: one download of about 110 MB (1.1 GB unpacked).
set -euo pipefail

# A git mirror of the upstream repository (https://www.sqlite.org/sqllogictest/).
REVISION="${SLT_CORPUS_REVISION:-c93ac416f05635ff5cc490009add936d9753d01d}"
URL="${SLT_CORPUS_URL:-https://github.com/gregrahn/sqllogictest/archive/${REVISION}.tar.gz}"
# sha256 over `sha256sum` of every test/**/*.test, paths sorted bytewise. It pins the
# scripts themselves, so it does not change if the archive is repacked.
CONTENT_SHA256="${SLT_CORPUS_SHA256:-57203c15e5180ef203fa1970983cbac4681963d7fefd9dbc693aaa6a6952564b}"
FILES=622

HOME_DIR="${SLT_CORPUS_HOME:-$HOME/.cache/heliosdb-nano/slt-corpus}"
DEST="$HOME_DIR/$REVISION"

digest() {
    (cd "$1" && find . -name '*.test' -type f | LC_ALL=C sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
}

if [ ! -f "$DEST/.verified" ]; then
    mkdir -p "$HOME_DIR"
    TMP="$(mktemp -d "$HOME_DIR/fetch.XXXXXX")"
    trap 'rm -rf "$TMP"' EXIT
    echo "fetching $URL" >&2
    curl --fail --location --silent --show-error --output "$TMP/corpus.tar.gz" "$URL"
    tar -xzf "$TMP/corpus.tar.gz" -C "$TMP" --strip-components=1 --wildcards '*/test'
    GOT="$(digest "$TMP/test")"
    COUNT="$(find "$TMP/test" -name '*.test' -type f | wc -l)"
    if [ "$GOT" != "$CONTENT_SHA256" ] || [ "$COUNT" != "$FILES" ]; then
        echo "corpus does not match the pinned digest:" >&2
        echo "  expected $CONTENT_SHA256 over $FILES files" >&2
        echo "  got      $GOT over $COUNT files" >&2
        exit 1
    fi
    rm -rf "$DEST"
    mkdir -p "$DEST"
    mv "$TMP/test" "$DEST/test"
    echo "$CONTENT_SHA256" > "$DEST/.verified"
fi
echo "corpus revision $REVISION, $FILES files, verified" >&2
echo "$DEST/test"
