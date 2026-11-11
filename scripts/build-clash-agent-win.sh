#!/bin/sh
set -eu
export MSYS_NO_PATHCONV=1
ROOT=$(pwd -W 2>/dev/null || pwd)
case "$ROOT" in
  ?:/*) ;;
  *) echo "Run this script from the repository root in Git Bash" >&2; exit 1 ;;
esac
docker volume create u60-build-cache >/dev/null
docker run --rm \
  -e ZTE_AGENT_VERSION=2.4.0-clash -e ZTE_AGENT_COMMIT=dev-u60-duo \
  -v u60-build-cache:/cache -v "${ROOT}:/src:ro" -v "${ROOT}/dist:/out" \
  public.ecr.aws/docker/library/rust:1.91 sh -eu -c '
export CARGO_HOME=/cache/cargo
export RUSTUP_HOME=/cache/rustup
export PATH=/cache/cargo/bin:/cache/zig:$PATH
rm -rf /tmp/open-u60-src
cp -r /src /tmp/open-u60-src
cd /tmp/open-u60-src
rustup target add aarch64-unknown-linux-musl >/dev/null
rm -f .cargo/config.toml
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-cpu=cortex-a55" cargo zigbuild \
  --release --target aarch64-unknown-linux-musl -p zte-agent --target-dir /cache/target
install -m 755 /cache/target/aarch64-unknown-linux-musl/release/zte-agent /out/zte-agent-clash
test -s /out/zte-agent-clash
sha256sum /out/zte-agent-clash
'
test -s dist/zte-agent-clash
