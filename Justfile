set shell := ["bash", "-uc"]

repo_root := justfile_directory()

# Default profile directory used by `kernel-prebuild --profile`.
default_profile := "debug"

# Where `kernel-prebuild` writes each target's outputs.
prebuild_root := repo_root / "target" / "kernel-prebuild"

# This justfile, for a recipe that calls another and reads what it prints.
just := quote(just_executable()) + " --justfile " + quote(justfile())

# Run `helios-cli kernel-prebuild` for `target` and stamp its directory with
# the tree it was built from (`_prebuild-source`), so the directory can be
# handed to a recipe on another machine through `HELIOS_PREBUILT`. CI's
# prebuild job runs this once per target for every lane of the run.
# Usage: just prebuild x86_64-unknown-none
prebuild target:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="{{prebuild_root}}/{{target}}/{{default_profile}}"
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "{{target}}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    {{just}} _prebuild-source "{{target}}" > "${out_dir}/prebuild-source"

# The stamp a prebuild of `target` from this tree carries: the commit,
# marked dirty when a tracked file differs from it, and the target.
_prebuild-source target:
    #!/usr/bin/env bash
    set -euo pipefail
    commit="$(git -C "{{repo_root}}" rev-parse HEAD)"
    if [[ -n "$(git -C "{{repo_root}}" status --porcelain --untracked-files=no)" ]]; then
        commit="${commit}-dirty"
    fi
    printf '%s %s\n' "${commit}" "{{target}}"

# Print the directory holding the kernel-prebuild outputs for `target`
# (`host` names the toolchain's host). Without `HELIOS_PREBUILT` the prebuild
# runs here first. With it, the outputs are the ones under
# `$HELIOS_PREBUILT/<target>` that another machine built, and they are
# refused unless their stamp names this tree's commit and this target and
# this tree has no change to a tracked file: the kernel embeds the root keys
# and the hosted tests boot the payload, so outputs from any other tree
# would be tested in its place.
_prebuilt target:
    #!/usr/bin/env bash
    set -euo pipefail
    target="{{target}}"
    if [[ "${target}" == host ]]; then
        target="$(rustc -vV | sed -n 's/^host: //p')"
        if [[ -z "${target}" ]]; then
            echo "failed to detect rust host target" >&2
            exit 1
        fi
    fi
    if [[ -z "${HELIOS_PREBUILT:-}" ]]; then
        {{just}} prebuild "${target}" >&2
        printf '%s\n' "{{prebuild_root}}/${target}/{{default_profile}}"
        exit 0
    fi
    dir="${HELIOS_PREBUILT}/${target}"
    expected="$({{just}} _prebuild-source "${target}")"
    found="$(cat "${dir}/prebuild-source")"
    if [[ "${expected}" == *-dirty* || "${found}" != "${expected}" ]]; then
        echo "HELIOS_PREBUILT: ${dir} was built from \"${found}\" and this tree is \"${expected}\"" >&2
        git -C "{{repo_root}}" status --short --untracked-files=no >&2
        echo "unset HELIOS_PREBUILT to build the outputs from this tree" >&2
        exit 1
    fi
    printf '%s\n' "${dir}"

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
    out_dir="$({{just}} _prebuilt host)"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    # `helios-venus` generates its protocol module from the pinned
    # venus-protocol inputs when it builds.
    cargo run -p helios-cli --quiet -- venus-protocol-fetch
    cargo clippy \
        --workspace --all-targets \
        --exclude helios \
        --exclude helios-aarch64 --exclude helios-riscv --exclude helios-x86 \
        --exclude helios-audio-test --exclude helios-compositor \
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
    for package in helios-audio-test helios-compositor helios-date helios-debugger helios-display-test helios-gpu-test \
        helios-http-client \
        helios-init helios-input-test helios-oob-load helios-perf helios-ping \
        helios-procbench helios-sched-tasks; do
        cargo clippy -p "${package}" --all-targets -- -D warnings
    done

# Unit and integration tests that need no emulator.
test-units:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="$({{just}} _prebuilt host)"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    cargo nextest run -p helios-hal -p helios-virtio -p helios-netstack -p helios-kernel --lib
    cargo nextest run -p helios-workspace-root --lib
    # What the desktop draws with — damage tracking, focus routing, the
    # terminal grid — is a plain library (`programs/compositor/render`)
    # and is tested here rather than only inside a boot, alongside the
    # capture check that links it.
    cargo nextest run -p helios-compositor-render -p helios-desktop-text-check
    cargo nextest run -p helios-inspector-protocol --lib
    # The host tools: QEMU arguments, network plans, the image audit, the
    # signature trailer, the profile store, the root keypair.
    cargo nextest run -p helios-inspector -p helios-artifact -p helios-profdata -p helios-cli -p helios-compiler-support
    # The Venus protocol is generated from pinned venus-protocol inputs that
    # the CLI fetches and hash-verifies; the parity test then compares the
    # generated encoders with the pinned Mesa generator's output.
    cargo run -p helios-cli --quiet -- venus-protocol-fetch
    cargo nextest run -p helios-venus -p helios-venus-gen
    cargo nextest run -p helios-kernel --test hal_layering
    # The hosted backend boots the same kernel on the host OS; its tests
    # cover the memory-policy density checks and the OOM killer, and
    # `embedded_debugger_is_provisioned` reads the prebuild payload above.
    HELIOS_BOOTFS="${out_dir}/helios-bootfs" cargo nextest run -p helios-hosted
    python3 "{{repo_root}}/x86/tests/interrupt_state.py"

# Run `cargo clippy` for a bare-metal `target` and its backend `package`
# against the kernel-prebuild outputs, denying warnings. It compiles exactly
# what a `cargo check` of the same two invocations would, so it is also the
# target's compile check.
# Usage: just clippy-target riscv64gc-unknown-none-elf helios-riscv
clippy-target target package:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="$({{just}} _prebuilt "{{target}}")"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    # Two invocations, with helios-kernel on the feature set the bare-metal
    # backends select rather than its host defaults: one invocation would
    # unify the two, `wasmtime-host` on a `target_os = "none"` target is a
    # configuration no backend ever builds, and wasmtime's pooling allocator
    # needs the custom-virtual-memory ABI only `wasmtime-bare-metal` turns on.
    cargo clippy \
        -p helios-kernel --no-default-features \
        --features wasmtime-bare-metal,embedded-debugger \
        --target "{{target}}" -- -D warnings
    cargo clippy \
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
    just clippy-target aarch64-unknown-none helios-aarch64
    just clippy-target riscv64gc-unknown-none-elf helios-riscv
    just clippy-target x86_64-unknown-none helios-x86
    just test-embedded-debugger

# Each package is checked on its own, so with its own feature resolution,
# which the workspace-wide `clippy-host` unifies away.
#
# Quick host-only check.
check-host:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="$({{just}} _prebuilt host)"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    cargo check -p helios-kernel
    cargo check -p helios-hosted
    cargo check -p helios-inspector
    cargo check -p helios-compositor-render -p helios-desktop-text-check

# Run hosted embedded-debugger smoke test.
test-embedded-debugger:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="$({{just}} _prebuilt host)"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    export HELIOS_BOOTFS="${out_dir}/helios-bootfs"
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
