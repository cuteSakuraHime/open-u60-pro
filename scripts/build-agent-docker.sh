#!/bin/sh
# Cross-build zte-agent for the router inside Docker (works on Windows/Linux/macOS hosts).
# Usage: scripts/build-agent-docker.sh            -> dist/zte-agent
# ZTE_AGENT_VERSION must equal the version in the device slot's bootstrap.sha256,
# otherwise the boot supervisor's `ota-probe` rejects the binary.
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
docker volume create u60-build-cache >/dev/null
docker run --rm \
  -e ZTE_AGENT_VERSION="${ZTE_AGENT_VERSION:-2.4.0}" \
  -e ZTE_AGENT_COMMIT="$(git rev-parse --short=12 HEAD)" \
  -v u60-build-cache:/cache -v "$PWD":/src:ro -v "$PWD/dist":/out \
  public.ecr.aws/docker/library/rust:1.91 sh -eu -c '
export CARGO_HOME=/cache/cargo RUSTUP_HOME=/cache/rustup PATH=/cache/cargo/bin:/cache/zig:$PATH
[ -x /cache/cargo/bin/rustup ] || { cp -r /usr/local/cargo /cache/cargo; cp -r /usr/local/rustup /cache/rustup; }
if [ ! -x /cache/zig/zig ]; then
  curl -fsSL https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz -o /tmp/zig.txz
  mkdir -p /cache/zig && tar -xJf /tmp/zig.txz -C /cache/zig --strip-components=1
fi
command -v cargo-zigbuild >/dev/null || cargo install --locked cargo-zigbuild
cp -r /src /tmp/src && cd /tmp/src
rustup target add aarch64-unknown-linux-musl >/dev/null
# .cargo/config.toml names a musl-gcc linker; zig provides the cross linker here.
rm -f .cargo/config.toml
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-cpu=cortex-a55" \
  cargo zigbuild --release --target aarch64-unknown-linux-musl -p zte-agent --target-dir /cache/target
cp /cache/target/aarch64-unknown-linux-musl/release/zte-agent /out/zte-agent
sha256sum /out/zte-agent'
