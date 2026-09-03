#!/usr/bin/env bash
# Runs rustfmt over this workspace's own crates.
#
# Deliberately not `cargo fmt --all`: cargo-fmt's `--all` also formats
# every local path-based dependency, which reaches into the vendored
# `../wasmtime` checkout. That tree tracks upstream and is not ours to
# reformat, so the package list is taken from `cargo metadata --no-deps`
# instead. Extra arguments are passed through to `cargo fmt`, so
# `tools/fmt.sh --check` is the gate and `tools/fmt.sh` is the fixer.
set -euo pipefail

cd "$(dirname "$0")/.."

packages=()
while IFS= read -r name; do
    packages+=(-p "${name}")
done < <(cargo metadata --no-deps --format-version 1 | python3 -c '
import json
import sys

for package in json.load(sys.stdin)["packages"]:
    print(package["name"])
')

if [[ ${#packages[@]} -eq 0 ]]; then
    echo "cargo metadata reported no workspace packages" >&2
    exit 1
fi

exec cargo fmt "${packages[@]}" "$@"
