set shell := ["bash", "-uc"]

repo_root := justfile_directory()

# The `helios-cli` the recipes run: the binary `HELIOS_CLI_BIN` names (CI's
# `helios-cli` action exports the run's prebuilt one), else this checkout's.
cli := env_var_or_default("HELIOS_CLI_BIN", "cargo run -p helios-cli --quiet --")

# Default profile directory used by `kernel-prebuild --profile`.
default_profile := "debug"

# The packages whose unit tests `test-kernel` runs with `--lib`, and the host
# tools `test-host-tools` tests besides the compiler host; each recipe and its
# `warm-` twin read the one list.
kernel_unit_packages := "-p helios-hal -p helios-virtio -p helios-netstack -p helios-artifact -p helios-kernel"
host_tool_packages := "-p helios-inspector -p helios-artifact -p helios-profdata -p helios-cli -p helios-compiler-support"

# The one bare-metal target whose rustflags carry `-Zbranch-protection`, which
# rustc refuses to mix with a precompiled `core`, `alloc` or
# `compiler_builtins` (`.cargo/config.toml`). Its builds rebuild them.
build_std_target := "aarch64-unknown-none"
build_std_flag := "-Zbuild-std=core,alloc"

# Prepare the trusted root for `target` and check the kernel and backend.
# helios-kernel is checked with the feature set the bare-metal backends select,
# not its host defaults: `wasmtime-host` on a `target_os = "none"` target is a
# configuration no backend ever builds, and wasmtime's pooling allocator needs
# the custom-virtual-memory ABI that only `wasmtime-bare-metal` turns on.
# Usage: just check-target riscv64gc-unknown-none-elf helios-riscv
check-target target package:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="{{ repo_root }}/target/kernel-prebuild/{{ target }}/{{ default_profile }}"
    {{ cli }} kernel-keys --out-dir "${out_dir}"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    build_std="{{ if target == build_std_target { build_std_flag } else { "" } }}"
    # Unquoted on purpose: an empty value must expand to no argument.
    cargo check ${build_std} -p helios-kernel \
        --no-default-features --features wasmtime-bare-metal,embedded-debugger \
        --target "{{ target }}"
    cargo check ${build_std} -p "{{ package }}" --target "{{ target }}"

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
    {{ repo_root }}/tools/fmt.sh --check

# Refuse the dependencies deny.toml bans (anyhow, per AGENTS.md §3.2).
deny-check:
    cargo deny --workspace check bans

# Reformat this workspace's own crates.
fmt:
    {{ repo_root }}/tools/fmt.sh

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
    out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
    {{ cli }} kernel-keys --out-dir "${out_dir}"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    # `helios-venus` generates its protocol module from the pinned
    # venus-protocol inputs when it builds.
    {{ cli }} venus-protocol-fetch
    cargo clippy -p helios-sync --all-targets --features runtime -- -D warnings
    cargo clippy -p helios-unix --all-targets --features runtime -- -D warnings
    cargo clippy -p helios-linux --all-targets --features runtime -- -D warnings
    cargo clippy \
        --workspace --all-targets \
        --features helios-webgpu/runtime --features helios-btrfs/write \
        --exclude helios \
        --exclude helios-aarch64 --exclude helios-riscv --exclude helios-x86 \
        --exclude helios-audio-test --exclude helios-compositor --exclude helios-cpu \
        --exclude helios-date --exclude helios-debugger --exclude helios-display-test \
        --exclude helios-gpu-test --exclude helios-http-client \
        --exclude helios-init --exclude helios-input-test \
        --exclude helios-oob-load --exclude helios-perf \
        --exclude helios-ping --exclude helios-pride-flag --exclude helios-procbench \
        --exclude helios-sched-tasks --exclude helios-surface-test \
        --exclude helios-webgpu-provider \
        -- -D warnings

# Clippy each guest program on its own, denying warnings.
clippy-programs:
    #!/usr/bin/env bash
    set -euo pipefail
    # One program per invocation: they select mutually exclusive
    # `helios-api` worlds, and a single invocation covering several of
    # them would unify those features and fail to build.
    for package in helios-audio-test helios-compositor helios-cpu helios-date helios-debugger helios-display-test helios-gpu-test \
        helios-http-client \
        helios-init helios-input-test helios-oob-load helios-perf helios-ping helios-pride-flag \
        helios-procbench helios-sched-tasks helios-surface-test helios-webgpu-provider; do
        cargo clippy -p "${package}" --all-targets -- -D warnings
    done

# Unit and integration tests that need no emulator.
test-units: test-kernel test-libraries test-host-tools

# `all` also mounts intact and corrupted mkfs.erofs images and needs erofs-utils.
test-kernel run_ignored="default":
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's/^host: //p')"
    out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
    {{ cli }} kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "${target}" \
        --profile "{{ default_profile }}" \
        --cargo cargo
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    cargo nextest run {{ kernel_unit_packages }} --lib --run-ignored "{{ run_ignored }}"
    # The Linux process host is behind a feature the run above leaves off.
    cargo nextest run -p helios-kernel --lib --features linux --run-ignored "{{ run_ignored }}"
    cargo nextest run -p helios-kernel --test hal_layering
    # The x86 user address space's page-table code, the code the x86
    # kernel runs, over host memory and real threads.
    cargo nextest run -p helios-x86-vmm
    # The hosted backend boots the same kernel on the host OS; its tests
    # cover the memory-policy density checks and the OOM killer, and
    # `embedded_debugger_is_provisioned` reads the prebuild payload above.
    HELIOS_BOOTFS="${out_dir}/helios-bootfs" cargo nextest run -p helios-hosted

# Compile every test binary `test-kernel` runs and run none of them. The
# binaries depend on the trusted-root keys and on nothing the prebuild's
# guest wasm holds, so CI runs this while the guest wasm is still building
# and `test-kernel` then compiles nothing. The package lists are the
# recipe's own, so the two build the same units.
warm-test-kernel:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's/^host: //p')"
    out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
    {{ cli }} kernel-keys --out-dir "${out_dir}"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_KERNEL_ROOT_SECRET_KEY="${out_dir}/helios-root-secret.key"
    cargo nextest run --no-run {{ kernel_unit_packages }} --lib
    cargo nextest run --no-run -p helios-kernel --lib --features linux
    cargo nextest run --no-run -p helios-kernel --test hal_layering
    cargo nextest run --no-run -p helios-x86-vmm
    HELIOS_BOOTFS="${out_dir}/helios-bootfs" cargo nextest run --no-run -p helios-hosted

test-libraries:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo nextest run -p helios-sync --features runtime
    cargo nextest run -p helios-unix --features runtime
    cargo nextest run -p helios-linux --features runtime
    cargo nextest run -p helios-firmware
    cargo nextest run -p helios-vmm
    cargo nextest run -p helios-workspace-root --lib
    # The TPM driver: both interfaces against register models that
    # enforce their state machines, and the sealing scenario replayed
    # from its committed swtpm recording.
    cargo nextest run -p helios-tpm
    # What the desktop draws with — damage tracking, focus routing, the
    # terminal grid — is a plain library (`programs/compositor/render`)
    # and is tested here rather than only inside a boot, alongside the
    # capture check that links it.
    cargo nextest run -p helios-compositor-render -p helios-desktop-text-check
    cargo nextest run -p helios-inspector-protocol --lib
    # Btrfs reader and transaction tests use images two pure-Rust writers build.
    # Images from `mkfs.btrfs` need btrfs-progs and run in
    # `test-btrfs-mkfs`.
    cargo nextest run -p helios-btrfs --features write
    # The Venus protocol is generated from pinned venus-protocol inputs that
    # the CLI fetches and hash-verifies; the parity test then compares the
    # generated encoders with the pinned Mesa generator's output. The
    # driver's entry points and handle-type translation are behind its
    # `driver` feature.
    {{ cli }} venus-protocol-fetch
    cargo nextest run -p helios-venus -p helios-venus-gen --features helios-venus/driver
    # The ACPI library runs QEMU's own CPU hotplug AML, from the pinned
    # tables the fetch below hash-verifies.
    just acpi-tables-fetch
    cargo nextest run -p helios-acpi
    python3 "{{ repo_root }}/x86/tests/interrupt_state.py"

test-host-tools:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "${HELIOS_PREBUILT_WASM:-}" ]]; then
        plugin="${HELIOS_PREBUILT_WASM}/{{ default_profile }}/helios_compiler_plugin.wasm"
    else
        target="$(rustc -vV | sed -n 's/^host: //p')"
        out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
        {{ cli }} kernel-prebuild \
            --out-dir "${out_dir}" \
            --target "${target}" \
            --profile "{{ default_profile }}" \
            --cargo cargo
        plugin="${out_dir}/compiler_plugin.wasm"
    fi
    # These tools read no bootfs. The compiler-host test needs only the
    # original wasm, which CI's shared producer already built.
    if [[ ! -f "$plugin" ]]; then
        echo "compiler plugin wasm is missing: $plugin" >&2
        exit 1
    fi
    cargo nextest run {{ host_tool_packages }}
    HELIOS_COMPILER_PLUGIN_WASM="$plugin" cargo nextest run -p helios-compiler-host

# Compile every test binary `test-host-tools` runs and run none of them; CI
# runs it while the compiler plugin is still building, which only the
# compiler-host test reads, when it runs.
warm-test-host-tools:
    cargo nextest run --no-run {{ host_tool_packages }}
    cargo nextest run --no-run -p helios-compiler-host

# Mount the images `mkfs.btrfs --rootdir` builds, one per checksum type and
# node size. Needs btrfs-progs on PATH; CI's test-libraries lane installs it.
# The log replay of images Linux leaves pending runs in
# `test-btrfs-log-replay`, which needs those images.
test-btrfs-mkfs:
    cargo nextest run -p helios-btrfs --features write --run-ignored ignored-only -E 'not test(/^log_replay::/)'

# Replay the images `tools/btrfs/fstests/log-replay.sh` makes from Linux's
# own btrfs into `images` (run.sh calls it in test-btrfs-fstests), and hold
# each replay to Linux's. Needs btrfs-progs on PATH.
test-btrfs-log-replay images:
    HELIOS_BTRFS_LOG_IMAGES="{{ images }}" cargo nextest run -p helios-btrfs --features write --test images --run-ignored ignored-only -E 'test(/^log_replay::/)'
# Fetch the ACPI tables `acpi/tests/qemu-tables.sha256` pins — QEMU's own
# expected tables for its q35 machine, from its `tests/data/acpi` at the
# v9.2.0 commit — into `target/acpi-tables` (or `$HELIOS_ACPI_TABLES_DIR`).
# A file already there at its pinned hash is kept; a fetched one replaces
# nothing unless it matches.
acpi-tables-fetch:
    #!/usr/bin/env bash
    set -euo pipefail
    # QEMU v9.2.0.
    commit="ae35f033b874c627d81d51070187fbf55f0bf1a7"
    dir="${HELIOS_ACPI_TABLES_DIR:-{{ repo_root }}/target/acpi-tables}"
    while read -r hash path; do
        file="${dir}/${path}"
        if [[ -f "${file}" ]] && echo "${hash}  ${file}" | shasum -a 256 -c --status; then
            continue
        fi
        mkdir -p "$(dirname "${file}")"
        curl --fail --silent --show-error --location --retry 3 --output "${file}.part" \
            "https://gitlab.com/qemu-project/qemu/-/raw/${commit}/tests/data/acpi/${path}"
        if ! echo "${hash}  ${file}.part" | shasum -a 256 -c --status; then
            echo "QEMU ${path} at ${commit} does not hash to the pinned ${hash}; nothing was stored" >&2
            rm -f "${file}.part"
            exit 1
        fi
        mv "${file}.part" "${file}"
    done < "{{ repo_root }}/acpi/tests/qemu-tables.sha256"

# Prepare the trusted root for `target` and lint the kernel and backend.
# Usage: just clippy-target riscv64gc-unknown-none-elf helios-riscv
clippy-target target package:
    #!/usr/bin/env bash
    set -euo pipefail
    out_dir="{{ repo_root }}/target/kernel-prebuild/{{ target }}/{{ default_profile }}"
    {{ cli }} kernel-keys --out-dir "${out_dir}"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    # Two invocations, for the same reason `check-target` uses explicit
    # features: cargo would otherwise unify helios-kernel's host defaults with
    # the backend's `wasmtime-bare-metal` selection.
    build_std="{{ if target == build_std_target { build_std_flag } else { "" } }}"
    # Unquoted on purpose: an empty value must expand to no argument.
    cargo clippy ${build_std} \
        -p helios-kernel --no-default-features \
        --features wasmtime-bare-metal,embedded-debugger \
        --target "{{ target }}" -- -D warnings
    cargo clippy ${build_std} \
        -p "{{ package }}" --target "{{ target }}" -- -D warnings
    if [[ "{{ package }}" == helios-x86 ]]; then
        cargo clippy -p helios --features linux --target "{{ target }}" -- -D warnings
    fi

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
        vm --arch {{ arch }} --profile-generate build

# Build the PGO kernel image of docs/pgo.md for `arch` (aarch64, riscv64,
# x86-64) against `profile`, a merged `.profdata`, and stop there.
#
# The mirror of `build-instrumented`: the PGO rustflags have one
# definition, in the inspector, so what this builds and what a
# `--profile-use` boot builds cannot drift. The profile is an argument
# because a PGO kernel is only as good as the profile behind it, and the
# inspector refuses one this toolchain cannot read before cargo starts.
# `bench-suite.yml` uploads one as `helios-kernel-profdata`.
# Further arguments are `vm` options: the release workflow passes
# `--root-secret-key <file>` so the kernel it ships trusts the release key
# (docs/boot.md).
# Usage: just kernel-pgo-use x86-64 target/pgo/helios-kernel.profdata
kernel-pgo-use arch profile *vm_options:
    cargo run -p helios-inspector --quiet -- \
        vm --arch {{ arch }} --profile-use {{ profile }} {{ vm_options }} build

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
    out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
    {{ cli }} kernel-keys --out-dir "${out_dir}"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    cargo check -p helios-kernel
    cargo check -p helios-hosted
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
    out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
    {{ cli }} kernel-prebuild \
        --out-dir "${out_dir}" \
        --target "${target}" \
        --profile "{{ default_profile }}" \
        --cargo cargo
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_BOOTFS="${out_dir}/helios-bootfs"
    cargo nextest run -p helios-hosted --no-capture init_program::tests::embedded_debugger_

# Compile the test binary `test-embedded-debugger` runs and run nothing; CI
# runs it while the guest wasm is still building, which the test reads when
# it runs and the compile does not.
warm-test-embedded-debugger:
    #!/usr/bin/env bash
    set -euo pipefail
    target="$(rustc -vV | sed -n 's/^host: //p')"
    out_dir="{{ repo_root }}/target/kernel-prebuild/${target}/{{ default_profile }}"
    {{ cli }} kernel-keys --out-dir "${out_dir}"
    export HELIOS_KERNEL_ROOT_PUBLIC_KEY="${out_dir}/helios-root-public.key"
    export HELIOS_BOOTFS="${out_dir}/helios-bootfs"
    cargo nextest run --no-run -p helios-hosted

# Unit tests and lint of the benchmark-suite tooling (tools/bench); no benchmark runs.
check-bench-tools:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ repo_root }}/tools/bench"
    uv sync --quiet
    uv run ruff check .
    uv run ruff format --check .
    uv run pytest -q
