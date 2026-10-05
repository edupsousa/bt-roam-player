#!/usr/bin/env bash
# Build a Debian 13 (trixie) arm64 sysroot with the -dev packages this project links
# against, for cross-compiling to a Raspberry Pi (see docs/CROSS.md). Needs docker.
set -euo pipefail

out="${1:-$(dirname "$0")/../target/sysroot-arm64}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

docker run --rm -v "$out:/out" -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" debian:trixie bash -euc '
  mkdir -p /dl/partial
  dpkg --add-architecture arm64
  apt-get update -qq
  apt-get install -y -qq --download-only --no-install-recommends \
    -o Dir::Cache::archives=/dl -o APT::Sandbox::User=root \
    libc6-dev:arm64 libgcc-14-dev:arm64 libstdc++-14-dev:arm64 \
    libdbus-1-dev:arm64 libpipewire-0.3-dev:arm64 libspa-0.2-dev:arm64
  for deb in /dl/*_arm64.deb /dl/*_all.deb; do [ -e "$deb" ] && dpkg -x "$deb" /out; done
  # merged-usr layout: linker scripts such as libm.so refer to /lib/aarch64-linux-gnu/...
  [ -e /out/lib ] || ln -s usr/lib /out/lib
  chown -R "$HOST_UID:$HOST_GID" /out
'
echo "sysroot ready: $out"
