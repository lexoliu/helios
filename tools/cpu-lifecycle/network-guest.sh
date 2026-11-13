# shellcheck shell=sh
# Run by the guest's dash for tools/cpu-lifecycle/network.sh, behind a
# prelude that sets `host`, `port`, `bytes` and `cycles`; the host reads
# what it prints.
#
# The target is the highest possible processor, as in guest.sh. The
# network service drains one queue pair per processor, so taking the
# target offline hands its pair off and the device's live count falls by
# one, and bringing it back takes the pair up again.
# shellcheck disable=SC2046 # the split into the line's words is the point
# shellcheck disable=SC2154 # the prelude sets host, port, bytes and cycles
set -- $(cpu list)
echo "sets:before $*"
possible=$2
target=$((possible - 1))
echo "target $target"

# The transfer: the guest streams `bytes` from the host's download
# server over the multi-queue tap, long enough that the cycles below
# run under it.
/host/tcp-throughput.wasm "$host" "$port" "$bytes" &

# The cycles start once the transfer is demonstrably moving: the host
# peer writes `transfer-begun` into the share after it has sent more
# than the guest's receive window and its own send buffer hold, so the
# guest has read bytes by then. A transfer that ends first, whatever
# ended it, leaves `transfer-ended` instead, and no cycle runs.
until [ -e /host/transfer-begun ]; do
    if [ -e /host/transfer-ended ]; then
        echo "status:transfer-ended-before-it-began"
        cycles=0
        break
    fi
done
echo "transfer:begun"

cycle=1
while [ "$cycle" -le "$cycles" ]; do
    cpu offline "$target" || echo "status:cycle-offline $?"
    echo "sets:offline $(cpu list)"
    cpu online "$target" || echo "status:cycle-online $?"
    echo "sets:online $(cpu list)"
    cycle=$((cycle + 1))
done
echo "cycles:done"

wait
echo "sets:after $(cpu list)"
echo "after:ok"
