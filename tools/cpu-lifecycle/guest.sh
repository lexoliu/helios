# Run by the guest's dash for tools/cpu-lifecycle/check.sh, which reads
# what it prints.
#
# The target is the highest possible processor: neither the bootstrap
# processor nor the one the embedded debugger runs on, so nothing the
# session itself runs is pinned to it.
# shellcheck disable=SC2046 # the split into the line's words is the point
set -- $(cpu list)
echo "sets:before $*"
possible=$2
bootstrap=$4
target=$((possible - 1))
echo "target $target"

# Requests the lifecycle answers without a transition, each with its
# own case.
cpu offline "$bootstrap" 2>&1 || echo "status:offline-bootstrap $?"
cpu offline "$possible" 2>&1 || echo "status:offline-not-possible $?"
cpu online "$target" 2>&1 || echo "status:online-not-offline $?"

# The load: the in-kernel compiler builds both programs from the host
# share, its worker threads running as global tasks on every kernel
# processor, the target among them; then curl fetches over virtio-net
# and hostcall-loop spins on host calls.
/host/curl.wasm http://detectportal.firefox.com/success.txt &
/host/hostcall-loop.wasm 2000000 &

cycle=1
while [ "$cycle" -le 8 ]; do
    cpu offline "$target" || echo "status:cycle-offline $?"
    echo "sets:offline $(cpu list)"
    cpu offline "$target" 2>&1 || echo "status:offline-not-online $?"
    cpu online "$target" || echo "status:cycle-online $?"
    cycle=$((cycle + 1))
done

wait
echo "sets:after $(cpu list)"
echo "after:ok"
