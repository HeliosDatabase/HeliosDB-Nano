# Internal Distribution Channel Enablement

This is an internal operator runbook for enabling non-Cargo install channels for
HeliosDB-Nano. Keep public install docs focused on channels that are already
published and smoke-tested.

Current public source of truth is still crates.io:

```bash
cargo install heliosdb-nano --locked
cargo add heliosdb-nano
```

## Release Invariants

All channels must use the same version as `Cargo.toml` and the same git tag.

```bash
VERSION="$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name=="heliosdb-nano") | .version')"
TAG="v${VERSION}"
git rev-parse "${TAG}"
```

Before enabling any channel for a tag:

```bash
cargo test --locked --lib -- --test-threads=2
cargo test --locked --doc -- --test-threads=2
cargo publish --dry-run --locked
```

Recommended release order:

1. Publish crates.io and create the GitHub Release.
2. Build and attach binary archives plus checksums.
3. Publish Docker images from the same tag.
4. Publish the npm wrapper after binary assets exist.
5. Update the Homebrew tap after source or binary assets exist.
6. Run smoke tests for every channel before advertising it publicly.

## Required Accounts And Secrets

GitHub repository secrets:

```text
CARGO_REGISTRY_TOKEN     # existing crates.io publish token
NPM_TOKEN                # npm automation token for the heliosdb package
DOCKERHUB_USERNAME       # if publishing Docker Hub heliosdb/nano
DOCKERHUB_TOKEN          # Docker Hub access token
HOMEBREW_TAP_TOKEN       # PAT with write access to HeliosDatabase/homebrew-tap
```

For GHCR-only Docker publishing, use `GITHUB_TOKEN` with workflow permission
`packages: write`; no Docker Hub secrets are required.

## Direct Binary Download / curl

Goal: GitHub Releases contain tested archives and checksums for each supported
platform. Public curl commands should only be added after this is green.

Recommended asset names:

```text
heliosdb-nano-x86_64-unknown-linux-gnu.tar.gz
heliosdb-nano-aarch64-unknown-linux-gnu.tar.gz
heliosdb-nano-x86_64-apple-darwin.tar.gz
heliosdb-nano-aarch64-apple-darwin.tar.gz
heliosdb-nano-x86_64-pc-windows-msvc.zip
SHA256SUMS
```

Minimum workflow shape:

```yaml
jobs:
  binaries:
    needs: publish
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            archive: tar.gz
          - os: macos-13
            target: x86_64-apple-darwin
            archive: tar.gz
          - os: macos-14
            target: aarch64-apple-darwin
            archive: tar.gz
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            archive: zip
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - run: cargo build --release --locked --target "${{ matrix.target }}"
      - run: ./target/${{ matrix.target }}/release/heliosdb-nano --version
        if: runner.os != 'Windows'
      - run: target/${{ matrix.target }}/release/heliosdb-nano.exe --version
        if: runner.os == 'Windows'
      - name: Package
        shell: bash
        run: |
          set -euo pipefail
          mkdir -p dist
          name="heliosdb-nano-${{ matrix.target }}"
          bin="target/${{ matrix.target }}/release/heliosdb-nano"
          if [ "${{ runner.os }}" = "Windows" ]; then
            bin="${bin}.exe"
            cp "$bin" heliosdb-nano.exe
            7z a "dist/${name}.zip" heliosdb-nano.exe
          else
            cp "$bin" heliosdb-nano
            tar -czf "dist/${name}.tar.gz" heliosdb-nano
          fi
      - uses: actions/upload-artifact@v4
        with:
          name: binary-${{ matrix.target }}
          path: dist/*
```

Add a follow-up job that downloads all artifacts, creates `SHA256SUMS`, and
uploads the files to the existing GitHub Release:

```bash
sha256sum dist/* > dist/SHA256SUMS
gh release upload "$GITHUB_REF_NAME" dist/* --clobber
```

Optional curl installer:

1. Add `scripts/install-nano.sh`.
2. Detect `uname -s` and `uname -m`.
3. Map to the release asset names above.
4. Download the archive and `SHA256SUMS`.
5. Verify the checksum before installing.
6. Install to `${HELIOSDB_INSTALL_DIR:-$HOME/.local/bin}` unless the user sets
   a system path.

Smoke test:

```bash
tmp="$(mktemp -d)"
curl -fsSL "https://github.com/HeliosDatabase/HeliosDB-Nano/releases/download/${TAG}/heliosdb-nano-x86_64-unknown-linux-gnu.tar.gz" \
  | tar -xz -C "$tmp"
"$tmp/heliosdb-nano" --version
```

## npm / npx

Goal: `npx heliosdb start` runs a small JavaScript wrapper that downloads or
executes the matching Nano binary.

Recommended package name:

```text
heliosdb
```

Use `@heliosdatabase/heliosdb` only if the unscoped name is unavailable; public
docs must then use `npx @heliosdatabase/heliosdb`.

Package structure:

```text
npm/heliosdb/
  package.json
  bin/heliosdb.js
  scripts/install.js
```

`package.json` minimum:

```json
{
  "name": "heliosdb",
  "version": "0.0.0",
  "description": "HeliosDB-Nano command-line installer and launcher",
  "bin": {
    "heliosdb": "bin/heliosdb.js"
  },
  "scripts": {
    "postinstall": "node scripts/install.js",
    "test": "node bin/heliosdb.js --version"
  },
  "license": "Apache-2.0"
}
```

`scripts/install.js` responsibilities:

1. Read `process.platform` and `process.arch`.
2. Map to a GitHub Release asset.
3. Download the archive for the package version.
4. Download `SHA256SUMS` and verify the archive.
5. Extract the binary into a package-local `vendor/` directory.
6. Mark it executable on Unix.

`bin/heliosdb.js` responsibilities:

1. Locate the package-local binary.
2. Forward all arguments and stdio to it.
3. Exit with the child process exit code.

Release automation:

```bash
cd npm/heliosdb
npm version "$VERSION" --no-git-tag-version
npm pack
npm publish --access public
```

Workflow requirements:

1. Run after GitHub Release binary assets are uploaded.
2. Use `NPM_TOKEN`.
3. Refuse to publish if `package.json` version does not match `Cargo.toml`.

Smoke tests:

```bash
npm view heliosdb version
npx --yes heliosdb --version
npx --yes heliosdb start --memory --http-port 18090
```

## Homebrew

Goal: `brew install HeliosDatabase/tap/heliosdb-nano` installs the tagged
release.

Create or use this tap repository:

```text
HeliosDatabase/homebrew-tap
```

Formula path:

```text
Formula/heliosdb-nano.rb
```

Source-build formula template; replace `vX.Y.Z` and the checksum during each
release:

```ruby
class HeliosdbNano < Formula
  desc "Single-binary embedded database with PostgreSQL and MySQL wire compatibility"
  homepage "https://github.com/HeliosDatabase/HeliosDB-Nano"
  url "https://github.com/HeliosDatabase/HeliosDB-Nano/archive/refs/tags/vX.Y.Z.tar.gz"
  sha256 "REPLACE_WITH_SOURCE_TARBALL_SHA256"
  license "Apache-2.0"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/heliosdb-nano --version")
  end
end
```

Manual update flow:

```bash
VERSION="$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name=="heliosdb-nano") | .version')"
TAG="v${VERSION}"
URL="https://github.com/HeliosDatabase/HeliosDB-Nano/archive/refs/tags/${TAG}.tar.gz"
curl -fsSL "$URL" -o "/tmp/heliosdb-nano-${TAG}.tar.gz"
shasum -a 256 "/tmp/heliosdb-nano-${TAG}.tar.gz"
```

Update `url`, `sha256`, and `version` as needed, then validate inside the tap:

```bash
brew audit --strict --online heliosdb-nano
brew install --build-from-source ./Formula/heliosdb-nano.rb
brew test heliosdb-nano
```

Automation options:

1. Push formula updates from the Nano release workflow using
   `HOMEBREW_TAP_TOKEN`.
2. Use a dedicated workflow in `HeliosDatabase/homebrew-tap` triggered by
   `workflow_dispatch` or `repository_dispatch`.
3. Add bottles later with `brew test-bot` once source builds are stable.

## Docker

Goal:

```bash
docker run -p 5432:5432 -p 8080:8080 heliosdb/nano:latest
```

Registry options:

```text
Docker Hub: heliosdb/nano
GHCR:       ghcr.io/heliosdatabase/heliosdb-nano
```

The current `deployment/docker/Dockerfile` must be reviewed before release
automation. It references `COPY crates ./crates`, but this repo currently has no
top-level `crates/` directory. Either remove that copy or use a Dockerfile that
matches the current source tree.

Recommended build workflow:

```yaml
jobs:
  docker:
    needs: publish
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/login-action@v3
        with:
          username: ${{ secrets.DOCKERHUB_USERNAME }}
          password: ${{ secrets.DOCKERHUB_TOKEN }}
      - uses: docker/metadata-action@v5
        id: meta
        with:
          images: |
            ghcr.io/heliosdatabase/heliosdb-nano
            heliosdb/nano
          tags: |
            type=semver,pattern={{version}}
            type=semver,pattern={{major}}.{{minor}}
            type=raw,value=latest
      - uses: docker/build-push-action@v6
        with:
          context: .
          file: deployment/docker/Dockerfile
          platforms: linux/amd64,linux/arm64
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
```

Local validation before enabling the workflow:

```bash
docker build -f deployment/docker/Dockerfile -t heliosdb-nano:test .
docker run --rm heliosdb-nano:test --version
cid="$(docker run -d -p 15432:5432 -p 18080:8080 heliosdb-nano:test)"
sleep 3
curl -fsS http://127.0.0.1:18080/health
docker rm -f "$cid"
```

Public smoke tests after release:

```bash
docker pull ghcr.io/heliosdatabase/heliosdb-nano:${TAG}
docker run --rm ghcr.io/heliosdatabase/heliosdb-nano:${TAG} --version
docker pull heliosdb/nano:${TAG}
docker run --rm heliosdb/nano:${TAG} --version
```

## Public README Promotion Checklist

Only move a channel from "not active" to public install instructions when all of
these are true:

```text
[ ] Artifact/package/image exists for the current tag.
[ ] Version command works from a clean host or clean container.
[ ] Start command works with a temporary data directory or memory mode.
[ ] Checksums are published and verified where applicable.
[ ] The command shown in README exactly matches the tested command.
[ ] The channel has a repeatable release workflow, not a one-off manual upload.
```
