#!/usr/bin/env bash
# Boots an x86-64 guest under KVM, takes its highest processor offline
# and back eight times while the in-kernel compiler and a fetch over
# virtio-net run (tools/cpu-lifecycle/guest.sh), and asserts the typed
# answers `cpu` printed and that the guest kept working.
#
#   check.sh <inspector> <runtime-dir> [vm option...]
#
# The extra options reach `helios-inspector vm` ahead of the session
# command, so a lane can name a different network backend or processor
# count. Run from the repository root: the host share is
# `artifacts/wasi-tools`.
set -euo pipefail

inspector="${1:?inspector binary}"
runtime="${2:?runtime directory}"
shift 2

here="$(dirname "${BASH_SOURCE[0]}")"
mkdir -p "$runtime"
out="$runtime/shell.out"

"$inspector" vm --arch x86-64 --release --memory 2G --accel kvm \
    --runtime-dir "$runtime" --keep-runtime-dir \
    --boot-program dash --boot-program debugger --boot-program cpu \
    --boot-program http-client \
    --shared-dir artifacts/wasi-tools \
    "$@" \
    shell "$here/guest.sh" 2>&1 >"$out" | tee "$runtime/inspector.log"
cat "$out"

fail() {
    echo "cpu-lifecycle: $*" >&2
    exit 1
}

# The sets as the guest read them first: `possible <n> bootstrap <b>
# online <list>`, every possible processor online.
read -r _ _ possible _ bootstrap _ online_before \
    <<<"$(grep -m1 '^sets:before ' "$out")" || fail "no sets:before line"
test "$possible" -ge 3 ||
    fail "$possible possible processors leave no secondary beside the debugger's"
target=$((possible - 1))
all="$(seq -s, 0 $((possible - 1)))"
without_target="$(seq -s, 0 $((possible - 2)))"
test "$online_before" = "$all" || fail "online at boot was $online_before, not $all"
grep -Fqx "target $target" "$out" || fail "the guest chose another target"

expect_line() {
    grep -Fqx -- "$1" "$out" || fail "missing: $1"
}
expect_count() {
    local count
    count="$(grep -Fcx -- "$2" "$out" || true)"
    test "$count" -eq "$1" || fail "$count of $1 lines read: $2"
}

# Requests answered without a transition, each with its own case and a
# failing exit.
expect_line "processor $bootstrap did not go offline: it is the bootstrap processor, which stays online"
expect_line "status:offline-bootstrap 1"
expect_line "processor $possible did not go offline: it is not one of the $possible possible processors"
expect_line "status:offline-not-possible 1"
expect_line "processor $target did not come online: it is not offline"
expect_line "status:online-not-offline 1"

# Eight cycles, each settled, each seen in the sets, and the second
# offline of each refused because the first had completed.
expect_count 8 "offline $target"
expect_count 8 "sets:offline possible $possible bootstrap $bootstrap online $without_target"
expect_count 8 "processor $target did not go offline: it is not online"
expect_count 8 "status:offline-not-online 1"
expect_count 8 "online $target"
if grep -q '^status:cycle-' "$out"; then
    fail "a cycle's transition failed: $(grep '^status:cycle-' "$out")"
fi

# The load finished correctly, and the first offline settled before
# the fetch did, so the transitions ran under it.
expect_line "success"
expect_line "hostcall-loop:2000000"
first_offline="$(grep -nFx -m1 "offline $target" "$out" | cut -d: -f1)"
fetched="$(grep -nFx -m1 success "$out" | cut -d: -f1)"
test "$first_offline" -lt "$fetched" ||
    fail "the fetch finished before the first offline, so nothing ran under load"

# Back where it started, and still answering.
expect_line "sets:after possible $possible bootstrap $bootstrap online $all"
expect_line "after:ok"

# The kernel's own account (kernel/src/exec/lifecycle.rs). The serial
# capture carries the console's colour escapes, so it is read as text.
serial="$runtime/debug-serial.log"
departures="$(grep -a 'Processor offline' "$serial" | grep -ac "processor=$target" || true)"
returns="$(grep -a 'Processor back online' "$serial" | grep -ac "processor=$target" || true)"
test "$departures" -eq 8 || fail "the kernel logged $departures departures of $target, not 8"
test "$returns" -eq 8 || fail "the kernel logged $returns returns of $target, not 8"
echo "cpu-lifecycle: processor $target went offline and back 8 times under load"
