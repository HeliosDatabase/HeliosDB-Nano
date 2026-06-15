# Install Channels

HeliosDB-Nano ships through several channels. Prebuilt binaries get you from
zero to first query in under a minute; building from source takes 20–40
minutes of compile time.

Prebuilt targets are attached to
[GitHub Releases](https://github.com/HeliosDatabase/HeliosDB-Nano/releases):

| Target | Archive |
|---|---|
| `x86_64-unknown-linux-gnu` | `heliosdb-nano-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| `aarch64-unknown-linux-gnu` | `heliosdb-nano-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| `aarch64-apple-darwin` | `heliosdb-nano-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| `x86_64-pc-windows-msvc` | `heliosdb-nano-vX.Y.Z-x86_64-pc-windows-msvc.zip` |

Each archive contains the `heliosdb-nano` binary, `LICENSE`, and `README.md`
at the archive root. Every release also carries a single `SHA256SUMS` file
covering all archives.

Not shipped yet: musl (Alpine) builds — the TLS stack currently pulls in
`openssl-sys`, which doesn't cross-build for `*-musl` out of the box — and
Intel macOS. Both fall back to the source channels below. Linux binaries are
built on `ubuntu-24.04` and link glibc 2.39+; older distros (Debian 11,
Ubuntu 20.04) should use the Docker or source channels.

## 1. Install script (Linux / macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/HeliosDatabase/HeliosDB-Nano/main/scripts/install.sh | sh
```

Detects OS/arch, downloads the latest release archive, verifies it against
`SHA256SUMS`, and installs to `~/.local/bin` (or `/usr/local/bin` when run as
root). Options:

```sh
sh install.sh <release-tag>                    # pin a release tag
HELIOSDB_INSTALL_DIR=/opt/bin sh install.sh    # custom install dir
```

## 2. cargo-binstall

If you have Rust tooling but don't want the 20–40 min compile:

```sh
cargo install cargo-binstall   # once
cargo binstall heliosdb-nano
```

`[package.metadata.binstall]` in `Cargo.toml` points binstall at the release
artifacts; it falls back to compiling from source for targets without a
prebuilt archive.

## 3. Manual download + verification

```sh
VERSION=<release-tag>
TARGET=x86_64-unknown-linux-gnu
BASE=https://github.com/HeliosDatabase/HeliosDB-Nano/releases/download/$VERSION

curl -fsSLO "$BASE/heliosdb-nano-$VERSION-$TARGET.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"
grep "heliosdb-nano-$VERSION-$TARGET.tar.gz" SHA256SUMS | sha256sum -c -   # macOS: shasum -a 256 -c -
tar -xzf "heliosdb-nano-$VERSION-$TARGET.tar.gz" heliosdb-nano
./heliosdb-nano --version
```

## 4. Docker (GHCR)

Built from the linux/amd64 release binary (`Dockerfile.binary`), pushed by
release CI:

```sh
docker run -p 5432:5432 -v heliosdb_data:/data \
  ghcr.io/heliosdatabase/heliosdb-nano:latest
```

To build locally from source instead (e.g. for custom feature flags):

```sh
docker build -f deployment/docker/Dockerfile -t heliosdb-nano .
```

## 5. From source (crates.io or git)

Needed for musl/Alpine, Intel macOS, custom feature sets (`code-graph`,
`mcp-endpoint`, `fips`, `ha-full`, …), or older glibc:

```sh
cargo install heliosdb-nano                    # crates.io
# or
git clone https://github.com/HeliosDatabase/HeliosDB-Nano && cd HeliosDB-Nano
cargo build --release --features code-graph,mcp-endpoint
```

Build prerequisites: Rust 1.85+, clang/libclang (rocksdb bindgen), and on
Linux `pkg-config` + OpenSSL headers (`libssl-dev`).

## Verifying any download

1. Fetch `SHA256SUMS` from the same release as the archive.
2. `sha256sum -c` (Linux) or `shasum -a 256 -c` (macOS) the line matching
   your archive — see channel 3 above.
3. The checksum file is generated in CI in the same workflow run that builds
   the binaries (`.github/workflows/release.yml`, `attach-binaries` job).

## First query after install

```sh
heliosdb-nano repl --data-dir ./helios-data
```
