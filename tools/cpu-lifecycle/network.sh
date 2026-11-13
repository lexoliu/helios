#!/usr/bin/env bash
# Boots an x86-64 guest under KVM on the multi-queue tap backend with one
# queue pair per processor, starts the `tcp-throughput` download the
# benchmark lane runs against its host server, takes the guest's highest
# processor offline and back repeatedly while the bytes flow
# (tools/cpu-lifecycle/network-guest.sh), and asserts that the transfer
# completed and that the `virtio-net online` line reported the live pair
# count after each change (#495).
#
#   network.sh <inspector> <runtime-dir> [vm option...]
#
# The tap and its bridge are provisioned beforehand with `vm net-setup`;
# the extra options name them (`--net-ifname`, `--net-bridge`). The host
# peer is reached on the bridge's address, `HELIOS_NET_HOST` (default
# `10.77.0.1`, the address `net-setup` gives the bridge). Run from the
# repository root: the host share is `artifacts/wasi-tools`.
set -euo pipefail

inspector="${1:?inspector binary}"
runtime="${2:?runtime directory}"
shift 2

here="$(dirname "${BASH_SOURCE[0]}")"
host="${HELIOS_NET_HOST:-10.77.0.1}"
# One pair per processor: the guest brings up no more pairs than it has
# processors, and a processor with no pair of its own would let its
# departure be absorbed without the live count changing.
processors=4
# 16 GiB: the benchmark's 64 MiB download takes a fraction of a second,
# which the offline would not land inside. This keeps the stream busy
# for as long as the cycles take.
bytes=17179869184
cycles=40
download_port=45871
upload_port=45872

mkdir -p "$runtime"
out="$runtime/shell.out"
script="$runtime/network-guest.sh"
{
    printf 'host=%s\nport=%s\nbytes=%s\ncycles=%s\n' \
        "$host" "$download_port" "$bytes" "$cycles"
    cat "$here/network-guest.sh"
} >"$script"

# The host peer, the same server `workload-bench` runs `tcp-throughput`
# against.
python3 tools/wasi-apps/tcp_throughput_server.py --host 0.0.0.0 \
    --download-port "$download_port" --upload-port "$upload_port" \
    --payload-bytes "$bytes" >"$runtime/peer.log" 2>&1 &
peer=$!
trap 'kill "$peer" 2>/dev/null || true' EXIT

"$inspector" vm --arch x86-64 --release --memory 2G --accel kvm \
    --runtime-dir "$runtime" --keep-runtime-dir \
    --smp "$processors" --net-backend tap --net-queues "$processors" \
    --boot-program dash --boot-program debugger --boot-program cpu \
    --shared-dir artifacts/wasi-tools \
    "$@" \
    shell "$script" 2>&1 >"$out" | tee "$runtime/inspector.log"
cat "$out"

fail() {
    echo "cpu-lifecycle-network: $*" >&2
    exit 1
}
kill -0 "$peer" 2>/dev/null || fail "the host peer exited: $(cat "$runtime/peer.log")"

expect_line() {
    grep -Fqx -- "$1" "$out" || fail "missing: $1"
}

read -r _ _ possible _ bootstrap _ online_before \
    <<<"$(grep -m1 '^sets:before ' "$out")" || fail "no sets:before line"
test "$possible" -eq "$processors" ||
    fail "$possible possible processors, not the $processors asked for"
target=$((possible - 1))
all="$(seq -s, 0 $((possible - 1)))"
without_target="$(seq -s, 0 $((possible - 2)))"
test "$online_before" = "$all" || fail "online at boot was $online_before, not $all"
expect_line "target $target"
if grep -q '^status:' "$out"; then
    fail "a request failed: $(grep '^status:' "$out")"
fi

# The transfer completed in full, and the cycles all ran before it did,
# so the transitions were taken under it.
expect_line "tcp-throughput:$bytes"
expect_line "cycles:done"
cycles_done="$(grep -nFx -m1 'cycles:done' "$out" | cut -d: -f1)"
transferred="$(grep -nFx -m1 "tcp-throughput:$bytes" "$out" | cut -d: -f1)"
test "$cycles_done" -lt "$transferred" ||
    fail "the transfer finished before the cycles did, so not every one ran under it"

# Every cycle was seen in the sets, and the machine ended where it began.
offlines="$(grep -Fcx "offline $target" "$out" || true)"
onlines="$(grep -Fcx "online $target" "$out" || true)"
test "$offlines" -eq "$cycles" || fail "$offlines departures of $target, not $cycles"
test "$onlines" -eq "$cycles" || fail "$onlines returns of $target, not $cycles"
count="$(grep -Fcx "sets:offline possible $possible bootstrap $bootstrap online $without_target" "$out" || true)"
test "$count" -eq "$cycles" || fail "$count offline sets read, not $cycles"
expect_line "sets:after possible $possible bootstrap $bootstrap online $all"
expect_line "after:ok"

# The pair count the `virtio-net online` line reported after each change
# (virtio/src/net.rs, `log_online`). The serial capture carries the
# console's colour escapes, which are stripped before the field is read.
# Bring-up reports every pair live; each departure shrinks the count by
# one and each return grows it back, once per cycle.
serial="$runtime/debug-serial.log"
mapfile -t counts < <(
    grep -a 'virtio-net online' "$serial" |
        sed -E 's/\x1b\[[0-9;]*m//g' |
        grep -ao 'queue_pairs=[0-9]*' | cut -d= -f2
)
expected=("$processors")
for ((cycle = 0; cycle < cycles; cycle++)); do
    expected+=("$((processors - 1))" "$processors")
done
test "${counts[*]}" = "${expected[*]}" ||
    fail "the live pair counts were '${counts[*]}', not '${expected[*]}'"
echo "cpu-lifecycle-network: $bytes bytes arrived while processor $target went offline and back $cycles times; live pairs $processors, then $((processors - 1)) and $processors each cycle"
