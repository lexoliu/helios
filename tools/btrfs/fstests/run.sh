#!/usr/bin/env bash
# Replays the pending-log images of log-replay.sh against Linux's own replay,
# runs the unmodified upstream fstests `generic/quick` group against Helios's
# btrfs writer mounted through FUSE by the `fuse-btrfs` example, then checks
# both test devices with `btrfs check` and runs the crash-consistency replay of
# crash.sh. Needs root (losetup, mount, dmsetup) and the build dependencies of
# fstests, btrfs-progs and fuse3.
#
# fstests has no mkfs or fsck for FSTYP=fuse, so this formats the devices with
# mkfs.btrfs and checks them with `btrfs check --readonly` itself.
#
# Every entry of expunged.txt is `<test> # <one-line reason>`.
#
# Environment: HELIOS_FUSE_BTRFS (the fuse-btrfs binary, default
# target/release/examples/fuse-btrfs), HELIOS_FSTESTS_WORK (default target/fstests),
# HELIOS_FSTESTS_DEVICE_SIZE (default 10G), HELIOS_CRASH_SECONDS (see crash.sh).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"
root=$(cd "$here/../../.." && pwd)

[[ $EUID -eq 0 ]] || die "run as root: it sets up loop devices, mounts and device-mapper targets"

HELIOS_FUSE_BTRFS=${HELIOS_FUSE_BTRFS:-$root/target/release/examples/fuse-btrfs}
[[ -x "$HELIOS_FUSE_BTRFS" ]] || die "$HELIOS_FUSE_BTRFS is not an executable; build the fuse-btrfs example with --release first"
work=${HELIOS_FSTESTS_WORK:-$root/target/fstests}
device_size=${HELIOS_FSTESTS_DEVICE_SIZE:-10G}
# The log device records every write the crash workload makes.
log_size=16G

fstests=$work/xfstests-dev
results=$work/results
mounts=/mnt/helios-fstests

export HELIOS_FUSE_BTRFS
export HELIOS_FUSE_BTRFS_LOG=$results/fuse-btrfs.log
export HELIOS_FSTESTS_DIR=$fstests
export HELIOS_FSTESTS_RESULTS=$results/fstests
export HELIOS_FSTESTS_TEST_DIR=$mounts/test
export HELIOS_FSTESTS_SCRATCH_MNT=$mounts/scratch
export HELIOS_CRASH_MNT=$mounts/crash

loops=()

cleanup() {
    # Best-effort teardown: the verdict is already decided.
    set +e
    local mountpoint
    for mountpoint in "$HELIOS_FSTESTS_TEST_DIR" "$HELIOS_FSTESTS_SCRATCH_MNT" "$HELIOS_CRASH_MNT"; do
        while findmnt --mountpoint "$mountpoint" >/dev/null; do
            umount "$mountpoint" || break
        done
    done
    local loop
    for loop in "${loops[@]}"; do
        losetup --detach "$loop"
    done
    rm -f /sbin/mount.fuse.helios-btrfs /sbin/umount.fuse
    chmod -R a+rX "$results"
}

# Runs the Rust log-replay comparison and `btrfs check` test over $1, dropping
# back to $SUDO_USER when sudo started this script so target/ keeps its owner
# and reuses the FUSE build's artifacts.
run_log_replay_tests() {
    local images=$1 cargo_bin cargo_home rustup_home
    cargo_bin=$(type -P cargo) || {
        echo "fstests: cargo is not on PATH" >&2
        return 1
    }
    local env_args=("PATH=$PATH" "HELIOS_BTRFS_LOG_IMAGES=$images")
    if [[ -n "${CARGO_HOME:-}" ]]; then
        env_args+=("CARGO_HOME=$CARGO_HOME")
    else
        cargo_home=$(cd -P "$(dirname "$cargo_bin")/.." && pwd)
        if [[ "$(basename "$cargo_home")" == .cargo ]]; then
            env_args+=("CARGO_HOME=$cargo_home")
        fi
    fi
    if [[ -n "${RUSTUP_HOME:-}" ]]; then
        env_args+=("RUSTUP_HOME=$RUSTUP_HOME")
    else
        rustup_home=$(cd -P "$(dirname "$cargo_bin")/../.." && pwd)/.rustup
        if [[ -d "$rustup_home" ]]; then
            env_args+=("RUSTUP_HOME=$rustup_home")
        fi
    fi
    local profile_args=()
    if [[ "$HELIOS_FUSE_BTRFS" == */release/* ]]; then
        profile_args=(--release)
    fi
    local cmd=(
        "$cargo_bin" test --locked -p helios-btrfs "${profile_args[@]}"
        --features "write,test-support" --test images log_replay::
        -- --ignored --nocapture
    )
    if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != root ]]; then
        (cd "$root" && sudo -H -u "$SUDO_USER" env "${env_args[@]}" "${cmd[@]}") 2>&1 |
            tee "$results/log-replay.log"
    else
        (cd "$root" && env "${env_args[@]}" "${cmd[@]}") 2>&1 |
            tee "$results/log-replay.log"
    fi
}

# Sets `loop` to a loop device backed by a new sparse file of the given size.
# Not a command substitution: the device must reach `loops` for the cleanup.
make_loop() {
    local file=$1 size=$2
    truncate --size "$size" "$file"
    loop=$(losetup --find --show "$file")
    loops+=("$loop")
}

"$here/fetch.sh" "$fstests"

# umount(8) runs /sbin/umount.fuse for every plain FUSE mount, and the cleanup
# removes the one this installs, so it must not replace one already there.
[[ ! -e /sbin/umount.fuse ]] || die "/sbin/umount.fuse already exists; this installs its own and removes it afterwards"

mkdir -p "$results" "$HELIOS_FSTESTS_RESULTS" \
    "$HELIOS_FSTESTS_TEST_DIR" "$HELIOS_FSTESTS_SCRATCH_MNT" "$HELIOS_CRASH_MNT"
trap cleanup EXIT

make_loop "$work/test.img" "$device_size"
HELIOS_FSTESTS_TEST_DEV=$loop
make_loop "$work/scratch.img" "$device_size"
HELIOS_FSTESTS_SCRATCH_DEV=$loop
make_loop "$work/crash.img" "$device_size"
HELIOS_CRASH_DEV=$loop
make_loop "$work/crash-log.img" "$log_size"
HELIOS_CRASH_LOG_DEV=$loop
export HELIOS_FSTESTS_TEST_DEV HELIOS_FSTESTS_SCRATCH_DEV HELIOS_CRASH_DEV HELIOS_CRASH_LOG_DEV

mkfs.btrfs --force "$HELIOS_FSTESTS_TEST_DEV"
mkfs.btrfs --force "$HELIOS_FSTESTS_SCRATCH_DEV"

install --mode 0755 "$here/mount.fuse.helios-btrfs" /sbin/mount.fuse.helios-btrfs
install --mode 0755 "$here/umount.fuse" /sbin/umount.fuse
cp "$here/local.config" "$fstests/local.config"

# The users and group fstests' README asks for; tests that need them are
# skipped without.
getent group fsgqa >/dev/null || groupadd fsgqa
for user in fsgqa 123456-fsgqa fsgqa2; do
    id "$user" >/dev/null 2>&1 || useradd --create-home --gid fsgqa "$user"
done

failed=()

# Linux's btrfs, not the FUSE mount, makes these: images with a log pending,
# which the unprivileged log replay tests read from $work/log-replay and hold
# to Linux's own replay and `btrfs check`.
echo "== log replay"
if "$here/log-replay.sh" "$work/log-replay"; then
    chmod a+rx "$work"
    run_log_replay_tests "$work/log-replay" || failed+=("log_replay comparison failed")
else
    failed+=("log-replay.sh failed")
fi

echo "== fstests generic/quick"
check_status=0
(cd "$fstests" && ./check -g generic/quick -E "$here/expunged.txt") || check_status=$?
((check_status == 0)) || failed+=("fstests ./check exited $check_status")

echo "== btrfs check"
if findmnt --mountpoint "$HELIOS_FSTESTS_TEST_DIR" >/dev/null; then
    umount "$HELIOS_FSTESTS_TEST_DIR"
fi
if findmnt --mountpoint "$HELIOS_FSTESTS_SCRATCH_MNT" >/dev/null; then
    umount "$HELIOS_FSTESTS_SCRATCH_MNT"
fi
for device in "$HELIOS_FSTESTS_TEST_DEV" "$HELIOS_FSTESTS_SCRATCH_DEV"; do
    btrfs check --readonly "$device" || failed+=("btrfs check found damage on $device")
done

echo "== crash consistency"
"$here/crash.sh" || failed+=("crash.sh failed")

if ((${#failed[@]})); then
    printf 'fstests: %s\n' "${failed[@]}" >&2
    exit 1
fi
