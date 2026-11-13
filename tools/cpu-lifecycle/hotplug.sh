#!/usr/bin/env bash
# Boots an x86-64 guest with `-smp 2,maxcpus=4`, hot-adds a vCPU through
# QEMU's QMP `device_add` and hot-removes it with `device_del`, and
# asserts the guest followed both through ACPI (SCI, GPE, AML Notify)
# without a reboot.
#
#   hotplug.sh <inspector> <runtime-dir> [vm option...]
#
# The extra options reach `helios-inspector vm` ahead of the session
# command. `HELIOS_ACCEL` (default `kvm`) names the accelerator and
# `HELIOS_VM_PROFILE` (default `--release`) the build profile flag; the
# lane uses both defaults, a machine without KVM can run `tcg`.
#
# Two channels run beside each other. The guest shell is the
# inspector's `repl`, fed `cpu list` through a FIFO at the moments the
# check chooses and answering on stdout as it goes (a `shell` session
# hands its output back only when it ends). QEMU is driven through the
# QMP socket the inspector owns in the kept runtime directory; QMP takes
# one client at a time and the inspector connects to it only to capture a
# hung guest, so this script talks to it through `socat`, one connection
# per command. Run from the repository root.
set -euo pipefail

inspector="${1:?inspector binary}"
runtime="${2:?runtime directory}"
shift 2
vm_options=("$@")

accel="${HELIOS_ACCEL:-kvm}"
read -r -a profile <<<"${HELIOS_VM_PROFILE---release}"
mkdir -p "$runtime"
out="$runtime/repl.out"
serial="$runtime/debug-serial.log"
# The inspector keeps its sockets in a short directory (a socket path
# is limited to ~104 bytes) and links it into the runtime directory;
# `qmp_ready` resolves the link once it exists.
sock=
fifo="$runtime/repl.in"
device_id=hotplug-vcpu
# The most each wait lasts before the check gives up with its name; the
# boot waits also cover the inspector building the kernel.
wait_seconds=120
boot_seconds=1500
# One `cpu list` answer (programs/cpu): the possible and online sets.
base="possible 4 bootstrap 0 online 0,1"

fail() {
    echo "cpu-hotplug: $*" >&2
    exit 1
}

inspector_pid=
cleanup() {
    if [ -n "$inspector_pid" ] && kill -0 "$inspector_pid" 2>/dev/null; then
        kill "$inspector_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# One QMP command on its own connection; prints its `return` value.
#   qmp <command> [arguments-json]
qmp() {
    local command="$1" arguments='{}' reply request
    if [ $# -ge 2 ]; then arguments="$2"; fi
    request="$(jq -cn --arg command "$command" --argjson arguments "$arguments" \
        '{execute: $command, arguments: $arguments, id: "check"}')"
    reply="$(printf '%s\n%s\n' '{"execute":"qmp_capabilities"}' "$request" |
        socat -t1 - "UNIX-CONNECT:$sock")" || fail "no QMP answer to $command"
    reply="$(jq -c 'select(.id == "check")' <<<"$reply")"
    test -n "$reply" || fail "QMP sent no reply to $command"
    if jq -e 'has("error")' <<<"$reply" >/dev/null; then
        fail "QMP refused $command: $(jq -c .error <<<"$reply")"
    fi
    jq -c .return <<<"$reply"
}

# What the guest was doing when a wait on it expired, kept in the
# runtime directory beside the serial capture: QEMU's vCPU list
# (`query-cpus-fast.json`); the inspector's capture over the same QMP
# socket (`vcpu-state.log`: every vCPU's registers, local APIC and
# stacks), which also names on stderr the kernel function each vCPU is
# in and the return addresses on its stacks; and, on a Linux host, the
# debug serial socket's queues (`debug-socket.txt`), since a guest
# writing to a line nobody drains stops with the host's queue full.
#   dump_guest <what>
dump_guest() {
    local what="$1" sockets
    if [ -z "$sock" ]; then
        echo "cpu-hotplug: QMP never answered; no vCPU state to capture" >&2
        return
    fi
    if (qmp query-cpus-fast) >"$runtime/query-cpus-fast.json"; then
        echo "cpu-hotplug: query-cpus-fast: $(cat "$runtime/query-cpus-fast.json")" >&2
    fi
    "$inspector" vm --arch x86-64 "${profile[@]}" --runtime-dir "$runtime" \
        "${vm_options[@]}" vcpu-state --after "the check's wait for $what" ||
        echo "cpu-hotplug: the inspector's vCPU capture failed" >&2
    if command -v ss >/dev/null; then
        sockets="$(dirname "$sock")"
        {
            echo "ss -xpn, the sockets of $sockets and their peers:"
            ss -xpn | grep -e '^Netid' -e "$sockets" -e qemu -e helios-insp || true
        } >"$runtime/debug-socket.txt"
        cat "$runtime/debug-socket.txt" >&2
    fi
}

# Runs `check` once a second until it succeeds or `limit` seconds pass.
#   wait_for <limit> <what> <check command...>
wait_for() {
    local limit="$1" what="$2" waited=0
    shift 2
    until "$@"; do
        kill -0 "$inspector_pid" 2>/dev/null ||
            fail "the inspector exited while waiting for $what"
        waited=$((waited + 1))
        if [ "$waited" -gt "$limit" ]; then
            dump_guest "$what"
            fail "timed out waiting for $what"
        fi
        sleep 1
    done
}

# The kernel's account of a hotplug request it answered
# (kernel/src/exec/hotplug.rs). The serial capture carries the console's
# colour escapes, so it is read as text.
#   hotplug_line <action> <outcome> [processor]
hotplug_line() {
    grep -a 'Processor hotplug request answered' "$serial" 2>/dev/null |
        grep -a "action=$1" | grep -a "outcome=$2" | grep -aq "processor=${3:-[0-9]}"
}

qmp_ready() {
    test -L "$runtime/sockets" || return 1
    sock="$(readlink "$runtime/sockets")/qmp.sock"
    test -S "$sock" && (qmp query-status) >/dev/null 2>&1
}

cpu_indexes() {
    qmp query-cpus-fast | jq -c 'map(."cpu-index") | sort'
}

# The `cpu list` answers the repl has printed, in order.
cpu_answers() {
    grep -ao 'possible [0-9]* bootstrap [0-9]* online [0-9,]*' "$out" 2>/dev/null || true
}

answers_seen=0
answered() {
    test "$(cpu_answers | wc -l)" -gt "$answers_seen"
}

# Asks the guest shell for the sets and leaves its answer in `sets`; the
# first answer waits for the guest to boot.
sets=
guest_sets() {
    local limit="${1:-$wait_seconds}"
    echo "cpu list" >&3
    wait_for "$limit" "the guest's answer to 'cpu list'" answered
    answers_seen=$((answers_seen + 1))
    sets="$(cpu_answers | tail -n 1)"
}

rm -f "$fifo" "$out"
mkfifo "$fifo"
# Read-write, so opening it never blocks on the other end.
exec 3<>"$fifo"

"$inspector" vm --arch x86-64 "${profile[@]}" --memory 2G --accel "$accel" \
    --smp 2 --max-processors 4 \
    --runtime-dir "$runtime" --keep-runtime-dir \
    --boot-program dash --boot-program debugger --boot-program cpu \
    --no-compiler-plugin \
    "${vm_options[@]}" \
    repl <&3 2>"$runtime/inspector.log" >"$out" &
inspector_pid=$!

wait_for "$boot_seconds" "the QMP socket" qmp_ready
sci_online() {
    grep -aq 'ACPI SCI online' "$serial" 2>/dev/null
}
wait_for "$boot_seconds" "the kernel's SCI to come online" sci_online

# The guest as it booted. The repl answers once the debugger is up.
guest_sets "$boot_seconds"
test "$sets" = "$base" || fail "the guest booted with '$sets', not '$base'"
before="$(cpu_indexes)"
test "$before" = "[0,1]" || fail "query-cpus-fast listed $before at boot, not [0,1]"

# A free slot: a hotpluggable entry no device occupies has no qom-path.
slot="$(qmp query-hotpluggable-cpus |
    jq -c '[.[] | select(has("qom-path") | not)] | first // empty')"
test -n "$slot" || fail "query-hotpluggable-cpus listed no free slot"
arguments="$(jq -c --arg id "$device_id" '.props + {driver: .type, id: $id}' <<<"$slot")"
echo "cpu-hotplug: device_add $arguments"

qmp device_add "$arguments" >/dev/null
added_by_qemu() {
    test "$(qmp query-cpus-fast | jq 'length')" -eq 3
}
wait_for "$wait_seconds" "query-cpus-fast to list the added vCPU" added_by_qemu
after_add="$(cpu_indexes)"
new_index="$(jq -n --argjson before "$before" --argjson after "$after_add" \
    '$after - $before | if length == 1 then first else empty end')"
test -n "$new_index" || fail "device_add changed the vCPUs from $before to $after_add"
echo "cpu-hotplug: QEMU lists vCPU $new_index"

# The kernel's arrival line, and then the guest shell's own view.
new_hotplug_line() {
    hotplug_line Insert Completed "$new_index"
}
wait_for "$wait_seconds" "the kernel to answer the Insert request for processor $new_index" \
    new_hotplug_line
guest_sets
test "$sets" = "$base,$new_index" ||
    fail "after device_add the guest's 'cpu list' said '$sets', not '$base,$new_index'"

# Release it.
qmp device_del "$(jq -cn --arg id "$device_id" '{id: $id}')" >/dev/null
echo "cpu-hotplug: device_del $device_id"
ejected() {
    hotplug_line Eject Completed "$new_index"
}
wait_for "$wait_seconds" "the kernel to answer the Eject request for processor $new_index" ejected
released() {
    grep -a 'Processor released to firmware' "$serial" | grep -aq "processor=$new_index"
}
wait_for "$wait_seconds" "the kernel to release processor $new_index to firmware" released
removed_by_qemu() {
    test "$(cpu_indexes)" = "$before"
}
wait_for "$wait_seconds" "query-cpus-fast to drop the vCPU" removed_by_qemu
guest_sets
test "$sets" = "$base" ||
    fail "after device_del the guest's 'cpu list' said '$sets', not '$base'"

# The repl's own `exit` ends the session and QEMU (its input is read-write
# in the inspector too, so closing this end would not end it).
echo exit >&3
exec 3>&-
wait "$inspector_pid" || fail "the inspector exited with a failure; see $runtime/inspector.log"
inspector_pid=
if grep -a 'Kernel panic' "$out" "$runtime/inspector.log" "$serial" >/dev/null 2>&1; then
    fail "the guest panicked"
fi
echo "cpu-hotplug: processor $new_index came online on device_add and was released on device_del"
