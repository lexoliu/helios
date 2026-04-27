set shell := ["bash", "-uc"]

repo_root := justfile_directory()

# Default profile directory used by `kernel-prebuild --profile`.
default_profile := "debug"

# Generate the kernel-prebuild manifest for `target` and run `cargo check` against it.
# Usage: just check-target riscv64gc-unknown-none-elf
check-target target:
    cargo run -p helios-cli --quiet -- kernel-prebuild \
        --out-dir "{{repo_root}}/target/kernel-prebuild/{{target}}/{{default_profile}}" \
        --target "{{target}}" \
        --profile "{{default_profile}}" \
        --cargo cargo
    HELIOS_KERNEL_PREBUILD_MANIFEST="{{repo_root}}/target/kernel-prebuild/{{target}}/{{default_profile}}/kernel-prebuild.json" \
        cargo check -p helios-kernel --target "{{target}}"

# Equivalent of AGENTS §7 required checks. Run before declaring a change complete.
check-all:
    cargo check -p helios-kernel
    cargo check -p helios-hosted
    cargo check -p helios-inspector
    just check-target riscv64gc-unknown-none-elf
    just check-target x86_64-unknown-none
    cargo test -p helios-hosted init_program::tests::embedded_debugger_ -- --nocapture

# Quick host-only check (no prebuild manifest, no none targets).
check-host:
    cargo check -p helios-kernel
    cargo check -p helios-hosted
    cargo check -p helios-inspector

# Run hosted embedded-debugger smoke test.
test-embedded-debugger:
    cargo test -p helios-hosted init_program::tests::embedded_debugger_ -- --nocapture
