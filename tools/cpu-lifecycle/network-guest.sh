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

# An offline is refused while the interface is still bringing itself up
# (its queue pair cannot be handed off yet), which is also before the
# transfer can connect, so the first offline waits for the network.
attempt=1
until cpu offline "$target" 2>&1; do
    attempt=$((attempt + 1))
    if [ "$attempt" -gt 200 ]; then
        echo "status:first-offline-refused"
        break
    fi
done

cycle=1
while [ "$cycle" -le "$cycles" ]; do
    echo "sets:offline $(cpu list)"
    cpu online "$target" || echo "status:cycle-online $?"
    echo "sets:online $(cpu list)"
    cycle=$((cycle + 1))
    if [ "$cycle" -le "$cycles" ]; then
        cpu offline "$target" || echo "status:cycle-offline $?"
    fi
done
echo "cycles:done"

wait
echo "sets:after $(cpu list)"
echo "after:ok"
