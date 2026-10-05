#!/usr/bin/env bash
# Cross-compile a release build for Raspberry Pi OS 64-bit (Debian 13, aarch64).
# Extra arguments go to `cargo build`. Output: target/aarch64-unknown-linux-gnu/release/
set -euo pipefail
cd "$(dirname "$0")/.."

export AARCH64_SYSROOT="${AARCH64_SYSROOT:-$PWD/target/sysroot-arm64}"
[ -d "$AARCH64_SYSROOT/usr/lib/aarch64-linux-gnu" ] || scripts/make-sysroot.sh "$AARCH64_SYSROOT"

# Resolve -sys crates (libdbus, libpipewire, libspa) in the sysroot, not on the host.
unset PKG_CONFIG_PATH
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$AARCH64_SYSROOT"
export PKG_CONFIG_LIBDIR="$AARCH64_SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig:$AARCH64_SYSROOT/usr/share/pkgconfig"
# bindgen (libspa-sys, pipewire-sys) must see target headers only: drop the devShell's host args.
unset BINDGEN_EXTRA_CLANG_ARGS
# bindgen also asks `clang` on PATH for builtin include dirs; the Nix wrapper would answer with host glibc.
export CLANG_PATH="$AARCH64_CLANG"
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="--target=aarch64-linux-gnu --sysroot=$AARCH64_SYSROOT -I$AARCH64_SYSROOT/usr/include/aarch64-linux-gnu $(pkg-config --cflags libpipewire-0.3 libspa-0.2)"

# C code in -sys crates (cc-rs) must use the same unwrapped clang as the linker.
export CC_aarch64_unknown_linux_gnu="$PWD/scripts/aarch64-clang.sh"

exec cargo build --release --target aarch64-unknown-linux-gnu "$@"
