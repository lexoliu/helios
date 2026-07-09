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
