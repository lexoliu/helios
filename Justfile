set shell := ["bash", "-uc"]

repo_root := justfile_directory()

# Default profile directory used by `kernel-prebuild --profile`.
default_profile := "debug"

# Generate the kernel-prebuild manifest for `target` and run `cargo check` against it.
# helios-kernel is checked with the feature set the bare-metal backends select,
# not its host defaults: `wasmtime-host` on a `target_os = "none"` target is a
# configuration no backend ever builds, and wasmtime's pooling allocator needs
# the custom-virtual-memory ABI that only `wasmtime-bare-metal` turns on.
# Usage: just check-target riscv64gc-unknown-none-elf helios-riscv
check-target target package:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="{{repo_root}}/target/kernel-prebuild/{{target}}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "{{target}}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    manifest="${out_dir}/kernel-prebuild.json"
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo check -p helios-kernel \
        --no-default-features --features wasmtime-bare-metal,embedded-debugger \
        --target "{{target}}"
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo check -p "{{package}}" --target "{{target}}"

# Every clippy and rustfmt gate CI enforces.
lint:
    just fmt-check
    just deny-check
    just clippy-host
    just clippy-programs
    just clippy-target aarch64-unknown-none helios-aarch64
    just clippy-target riscv64gc-unknown-none-elf helios-riscv
    just clippy-target x86_64-unknown-none helios-x86

# Check formatting of this workspace's own crates.
fmt-check:
    {{repo_root}}/tools/fmt.sh --check

# Refuse the dependencies deny.toml bans (anyhow, per AGENTS.md §3.2).
deny-check:
    cargo deny --workspace check bans

# Reformat this workspace's own crates.
fmt:
    {{repo_root}}/tools/fmt.sh

# Clippy every crate that builds for the host, denying warnings.
clippy-host:
    #!/usr/bin/env bash
    set -euo pipefail
    # `--workspace` cannot be used here: cargo unifies features across
    # the selected packages, and the bare-metal backends pin
    # `critical-section/restore-state-usize` while the host crates pull
    # in its `std` backend, which is a hard conflict. Those backends are
    # covered by `clippy-target` and the guest programs by
    # `clippy-programs`.
    target="$(rustc -vV | sed -n 's/^host: //p')"
    out_dir="{{repo_root}}/target/kernel-prebuild/${target}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "${target}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    HELIOS_KERNEL_PREBUILD_MANIFEST="${out_dir}/kernel-prebuild.json" cargo clippy \
        --workspace --all-targets \
        --exclude helios \
        --exclude helios-aarch64 --exclude helios-riscv --exclude helios-x86 \
        --exclude helios-compositor \
        --exclude helios-date --exclude helios-debugger --exclude helios-display-test \
        --exclude helios-gpu-test --exclude helios-http-client \
        --exclude helios-init --exclude helios-input-test \
        --exclude helios-oob-load --exclude helios-perf \
        --exclude helios-ping --exclude helios-procbench \
        --exclude helios-sched-tasks \
        -- -D warnings

# Clippy each guest program on its own, denying warnings.
clippy-programs:
    #!/usr/bin/env bash
    set -euo pipefail
    # One program per invocation: they select mutually exclusive
    # `helios-api` worlds, and a single invocation covering several of
    # them would unify those features and fail to build.
    for package in helios-compositor helios-date helios-debugger helios-display-test helios-gpu-test helios-http-client \
        helios-init helios-input-test helios-oob-load helios-perf helios-ping \
        helios-procbench helios-sched-tasks; do
        cargo clippy -p "${package}" --all-targets -- -D warnings
    done

# Unit and integration tests that need no emulator.
test-units:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's/^host: //p')"
    out_dir="{{repo_root}}/target/kernel-prebuild/${target}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "${target}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    export HELIOS_KERNEL_PREBUILD_MANIFEST="${out_dir}/kernel-prebuild.json"
    cargo nextest run -p helios-hal -p helios-virtio -p helios-netstack -p helios-kernel --lib
    cargo nextest run -p helios-workspace-root --lib
    # What the desktop draws with — damage tracking, focus routing, the
    # terminal grid — is a plain library (`programs/compositor/render`)
    # and is tested here rather than only inside a boot, alongside the
    # capture check that links it.
    cargo nextest run -p helios-compositor-render -p helios-desktop-text-check
    cargo nextest run -p helios-inspector-protocol --lib
    cargo nextest run -p helios-kernel --test hal_layering
    python3 "{{repo_root}}/x86/tests/interrupt_state.py"

# Generate the kernel-prebuild manifest for `target` and run `cargo clippy`
# against it, denying warnings.
# Usage: just clippy-target riscv64gc-unknown-none-elf helios-riscv
clippy-target target package:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="{{repo_root}}/target/kernel-prebuild/{{target}}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "{{target}}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    manifest="${out_dir}/kernel-prebuild.json"
    # Two invocations, for the same reason `check-target` uses explicit
    # features: cargo would otherwise unify helios-kernel's host defaults with
    # the backend's `wasmtime-bare-metal` selection.
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo clippy \
        -p helios-kernel --no-default-features \
        --features wasmtime-bare-metal,embedded-debugger \
        --target "{{target}}" -- -D warnings
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo clippy \
        -p "{{package}}" --target "{{target}}" -- -D warnings

# Build the instrumented kernel image of docs/pgo.md for `arch` (aarch64,
# riscv64, x86-64) and stop there.
#
# The instrumented rustflags have exactly one definition, in the inspector,
# so what this builds and what a `--profile-generate` boot builds cannot
# drift. `vm` always wants an accelerator named; this recipe boots nothing,
# so the one named here means only "not a boot".
# Usage: just build-instrumented aarch64
build-instrumented arch:
    cargo run -p helios-inspector --quiet -- \
        vm --arch {{arch}} --profile-generate build

# Build the PGO kernel image of docs/pgo.md for `arch` (aarch64, riscv64,
# x86-64) against `profile`, a merged `.profdata`, and stop there.
#
# The mirror of `build-instrumented`: the PGO rustflags have one
# definition, in the inspector, so what this builds and what a
# `--profile-use` boot builds cannot drift. The profile is an argument
# because a PGO kernel is only as good as the profile behind it, and the
# inspector refuses one this toolchain cannot read before cargo starts.
# `bench-suite.yml` uploads one as `helios-kernel-profdata`.
# Usage: just kernel-pgo-use x86-64 target/pgo/helios-kernel.profdata
kernel-pgo-use arch profile:
    cargo run -p helios-inspector --quiet -- \
        vm --arch {{arch}} --profile-use {{profile}} build

# Equivalent of AGENTS §7 required checks. Run before declaring a change complete.
check-all:
    just check-host
    just check-target aarch64-unknown-none helios-aarch64
    just check-target riscv64gc-unknown-none-elf helios-riscv
    just check-target x86_64-unknown-none helios-x86
    just test-embedded-debugger

# Quick host-only check.
check-host:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's/^host: //p')"
    if [[ -z "${target}" ]]; then
        echo "failed to detect rust host target" >&2
        exit 1
    fi
    out_dir="{{repo_root}}/target/kernel-prebuild/${target}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "${target}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    manifest="${out_dir}/kernel-prebuild.json"
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo check -p helios-kernel
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo check -p helios-hosted
    cargo check -p helios-inspector
    cargo check -p helios-compositor-render -p helios-desktop-text-check

# Run hosted embedded-debugger smoke test.
test-embedded-debugger:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's/^host: //p')"
    if [[ -z "${target}" ]]; then
        echo "failed to detect rust host target" >&2
        exit 1
    fi
    out_dir="{{repo_root}}/target/kernel-prebuild/${target}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "${target}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    HELIOS_KERNEL_PREBUILD_MANIFEST="${out_dir}/kernel-prebuild.json" \
        cargo nextest run -p helios-hosted --no-capture init_program::tests::embedded_debugger_

# Unit tests and lint of the benchmark-suite tooling (tools/bench); no benchmark runs.
check-bench-tools:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{repo_root}}/tools/bench"
    uv sync --quiet
    uv run ruff check .
    uv run ruff format --check .
    uv run pytest -q
