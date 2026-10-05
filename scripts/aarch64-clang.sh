#!/usr/bin/env bash
# Cargo linker and C compiler (CC) for aarch64-unknown-linux-gnu: unwrapped clang + lld against the Debian
# sysroot made by scripts/make-sysroot.sh. AARCH64_CLANG / AARCH64_LLD come from the
# Nix devShell; AARCH64_SYSROOT is set by scripts/build-arm64.sh.
exec "${AARCH64_CLANG:?run inside the Nix devShell}" --target=aarch64-linux-gnu \
  --sysroot="${AARCH64_SYSROOT:?use scripts/build-arm64.sh}" \
  -fuse-ld="${AARCH64_LLD:?}" "$@"
