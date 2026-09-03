set shell := ["bash", "-uc"]

repo_root := justfile_directory()

# Default profile directory used by `kernel-prebuild --profile`.
default_profile := "debug"

# Generate the kernel-prebuild manifest for `target` and run `cargo check` against it.
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
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo check -p helios-kernel --target "{{target}}"
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo check -p "{{package}}" --target "{{target}}"

# Every clippy and rustfmt gate CI enforces.
lint:
    just fmt-check
    just clippy-host
    just clippy-programs
    just clippy-target aarch64-unknown-none helios-aarch64
    just clippy-target riscv64gc-unknown-none-elf helios-riscv
    just clippy-target x86_64-unknown-none helios-x86

fmt-check:
    cargo fmt --all --check

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
        --exclude helios-date --exclude helios-debugger --exclude helios-http-client \
        --exclude helios-init --exclude helios-perf --exclude helios-ping \
        -- -D warnings

# Clippy each guest program on its own, denying warnings.
clippy-programs:
    #!/usr/bin/env bash
    set -euo pipefail
    # One program per invocation: they select mutually exclusive
    # `helios-api` worlds, and a single invocation covering several of
    # them would unify those features and fail to build.
    for package in helios-date helios-debugger helios-http-client \
        helios-init helios-perf helios-ping; do
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
    cargo test -p helios-hal -p helios-virtio -p helios-netstack -p helios-kernel --lib
    cargo test -p helios-kernel --test hal_layering

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
    HELIOS_KERNEL_PREBUILD_MANIFEST="${manifest}" cargo clippy \
        -p helios-kernel -p "{{package}}" --target "{{target}}" -- -D warnings

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
        cargo test -p helios-hosted init_program::tests::embedded_debugger_ -- --nocapture
