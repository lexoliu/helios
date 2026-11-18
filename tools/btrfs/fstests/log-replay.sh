#!/usr/bin/env bash
# Images Linux's own btrfs leaves with a log tree pending, for Helios's log
# replay to recover, made the way fstests' log-writes tests make crash points
# (common/dmlogwrites, tests/btrfs/192): each case formats a device behind a
# dm-log-writes target, commits a base tree, changes it, fsyncs, and marks
# the log; replay-log then rebuilds the device as it stood at the mark,
# after the fsync and before any commit, so its superblock points at the log
# tree the fsync wrote. A copy that Linux mounts, which replays the log, and
# unmounts is what Helios's replay of the same image must match.
#
# Writes <directory>/<case>/pending.img (the log pending) and
# <directory>/<case>/linux.img (Linux's replay of it, which `btrfs check`
# has passed). `tests/images/log_replay.rs` reads them through
# HELIOS_BTRFS_LOG_IMAGES.
#
# Called by run.sh as root, with HELIOS_FSTESTS_DIR the built fstests tree.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"

out=${1:?usage: log-replay.sh <directory>}
: "${HELIOS_FSTESTS_DIR:?}"
replay_log=$HELIOS_FSTESTS_DIR/src/log-writes/replay-log
[[ -x "$replay_log" ]] || die "$replay_log is missing; build fstests first"

# mkfs.btrfs's minimum for a single device with its default profiles is
# well below this; the log device holds every write a case makes.
device_size=256M
log_size=1G
target=logwrites-helios-replay
mnt=$(mktemp -d)
work=$(mktemp -d)
loops=()

cleanup() {
    set +e
    mountpoint -q "$mnt" && umount "$mnt"
    [[ -e /dev/mapper/$target ]] && dmsetup remove --retry "$target"
    local loop
    for loop in "${loops[@]}"; do
        losetup --detach "$loop"
    done
    rm -rf "$work"
    rmdir "$mnt"
}
trap cleanup EXIT

modprobe dm-log-writes || die "cannot load the dm-log-writes kernel module"

# Sets `loop` to a loop device over the file $1, which is created $2 long.
attach() {
    truncate --size "$2" "$1"
    loop=$(losetup --find --show "$1")
    loops+=("$loop")
}

detach() {
    losetup --detach "$1"
    local kept=() entry
    for entry in "${loops[@]}"; do
        [[ "$entry" == "$1" ]] || kept+=("$entry")
    done
    loops=("${kept[@]}")
}

fsync() {
    xfs_io -c fsync "$@"
}

# Each case has a `base_<case>` that builds what the first commit holds and
# a `change_<case>` that makes the change the fsync logs, both run in $mnt.

base_data() {
    mkdir 文档
    xfs_io -f -c "pwrite -S 0x11 0 256K" 文档/既存.bin >/dev/null
    xfs_io -f -c "pwrite -S 0x12 0 192K" 截断.bin >/dev/null
}
change_data() {
    # Inline data, a new regular extent with its checksums, an overwrite
    # splitting a committed extent, a hole, preallocation past i_size, and
    # a truncation.
    xfs_io -f -c "pwrite -S 0x21 0 1000" -c fsync 文档/日本語ファイル.txt >/dev/null
    xfs_io -f -c "pwrite -S 0x22 0 300K" -c "pwrite -S 0x23 1M 4K" \
        -c "falloc -k 2M 64K" -c fsync 한국어.bin >/dev/null
    xfs_io -c "pwrite -S 0x24 64K 8K" -c fsync 文档/既存.bin >/dev/null
    xfs_io -c "truncate 70K" -c fsync 截断.bin >/dev/null
}

base_rename() {
    mkdir 源 目标
    xfs_io -f -c "pwrite -S 0x31 0 20K" 源/旧名.txt >/dev/null
    xfs_io -f -c "pwrite -S 0x32 0 3K" 源/同じ.txt >/dev/null
}
change_rename() {
    # The old directory is logged first, so the rename records the old
    # name's removal in its log.
    touch 源/触发
    fsync 源
    mv 源/旧名.txt 目标/新しい名前.txt
    mv 源/同じ.txt 源/새이름.txt
    fsync 目标/新しい名前.txt 源/새이름.txt
}

base_unlink() {
    mkdir 目录
    xfs_io -f -c "pwrite -S 0x41 0 64K" 目录/删除.txt >/dev/null
    xfs_io -f -c "pwrite -S 0x42 0 2K" 目录/保留.txt >/dev/null
}
change_unlink() {
    rm 目录/删除.txt
    xfs_io -f -c "pwrite -S 0x43 0 9K" 目录/新規.txt >/dev/null
    fsync 目录/新規.txt 目录
}

base_hardlink() {
    mkdir 子
    xfs_io -f -c "pwrite -S 0x51 0 12K" 原本.txt >/dev/null
}
change_hardlink() {
    ln 原本.txt 링크.txt
    ln 原本.txt 子/リンク.txt
    fsync 原本.txt
}

base_rmdir() {
    mkdir -p 父/子目录 父/空
    xfs_io -f -c "pwrite -S 0x61 0 40K" 父/子目录/孫.txt >/dev/null
}
change_rmdir() {
    rm -rf 父/子目录
    rmdir 父/空
    fsync 父
}

base_xattr() {
    xfs_io -f -c "pwrite -S 0x71 0 1K" 属性.txt >/dev/null
    setfattr -n user.删除 -v gone 属性.txt
    setfattr -n user.保持 -v kept 属性.txt
}
change_xattr() {
    setfattr -x user.删除 属性.txt
    setfattr -n user.新しい -v 새 属性.txt
    setfattr -n security.helios -v system_u:object_r:helios_t:s0 属性.txt
    chmod 04755 属性.txt
    ln -s 属性.txt 符号链接
    mknod 设备 c 1 3
    fsync 属性.txt .
    mkdir 新目录
    setfattr -n user.目录 -v 보존 新目录
    xfs_io -f -c "pwrite -S 0x72 0 512" -c fsync 新目录/하위.txt >/dev/null
}

base_subvolume() {
    btrfs subvolume create 卷 >/dev/null
    xfs_io -f -c "pwrite -S 0x81 0 8K" 卷/既存.txt >/dev/null
}
change_subvolume() {
    xfs_io -f -c "pwrite -S 0x82 0 24K" -c fsync 卷/새파일.txt >/dev/null
    xfs_io -c "pwrite -S 0x83 4K 4K" -c fsync 卷/既存.txt >/dev/null
}

run_case() {
    local case=$1 dev log pending
    local dir=$out/$case
    echo "== log replay: $case"
    rm -rf "$dir"
    mkdir -p "$dir"
    attach "$work/device.img" "$device_size"
    dev=$loop
    attach "$work/log.img" "$log_size"
    log=$loop
    dmsetup create "$target" --table "0 $(blockdev --getsz "$dev") log-writes $dev $log"
    mkfs.btrfs --force --nodiscard "/dev/mapper/$target" >"$dir/mkfs.log"
    dmsetup message "$target" 0 mark mkfs
    # A commit interval no case reaches, so only the explicit sync commits.
    mount -t btrfs -o commit=3600 "/dev/mapper/$target" "$mnt"
    (cd "$mnt" && "base_$case")
    sync -f "$mnt"
    (cd "$mnt" && "change_$case")
    dmsetup message "$target" 0 mark fsynced
    umount "$mnt"
    dmsetup remove --retry "$target"

    attach "$dir/pending.img" "$device_size"
    pending=$loop
    "$replay_log" --log "$log" --find --end-mark fsynced >/dev/null ||
        die "case $case: the log holds no fsynced mark"
    "$replay_log" --log "$log" --replay "$pending" --end-mark fsynced
    detach "$pending"
    detach "$dev"
    detach "$log"
    rm -f "$work/device.img" "$work/log.img"
    # The device's last state has the same fsid; Linux must not remember it.
    btrfs device scan --forget

    local log_root
    log_root=$(btrfs inspect-internal dump-super "$dir/pending.img" | awk '$1 == "log_root" { print $2 }')
    [[ -n "$log_root" && "$log_root" != 0 ]] ||
        die "case $case: Linux committed the fsync instead of logging it"

    cp --sparse=always "$dir/pending.img" "$dir/linux.img"
    attach "$dir/linux.img" "$device_size"
    mount -t btrfs "$loop" "$mnt"
    umount "$mnt"
    detach "$loop"
    btrfs device scan --forget
    btrfs check --readonly "$dir/linux.img" >"$dir/linux-check.log" 2>&1 ||
        die "case $case: btrfs check failed on Linux's own replay; see $dir/linux-check.log"
}

mkdir -p "$out"
for case in data rename unlink hardlink rmdir xattr subvolume; do
    run_case "$case"
done
chmod -R a+rX "$out"
