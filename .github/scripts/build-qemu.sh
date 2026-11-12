#!/usr/bin/env bash
#
# Builds the pinned upstream QEMU `smoke-aarch64` boots and installs it under
# a prefix. The lane caches the prefix under a key that hashes this file, so
# a change to the configure invocation below builds a new QEMU instead of
# restoring one configured the old way.
#
# Usage: build-qemu.sh <version> <tarball sha256> <install prefix>
set -euo pipefail

version="$1"
sha256="$2"
prefix="$3"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

tarball="qemu-${version}.tar.xz"
curl -fsSLO "https://download.qemu.org/${tarball}"
echo "${sha256}  ${tarball}" | sha256sum -c -
tar xf "${tarball}"
cd "qemu-${version}"
# aarch64-softmmu is the only target the lane boots, and the inspector's
# default network backend is `user`, which has been external libslirp since
# 8.0.
./configure \
  --prefix="${prefix}" \
  --target-list=aarch64-softmmu \
  --enable-slirp \
  --disable-docs \
  --disable-gtk \
  --disable-sdl \
  --disable-vnc
ninja -C build install
