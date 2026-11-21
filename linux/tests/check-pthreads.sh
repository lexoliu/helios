#!/bin/sh
# Runs the musl pthread and futex fixture (#1032) in each of its four endings
# and compares its output and exit status with what Linux gives. Runs under
# Linux (in build-fixtures.sh) and under Helios's dash (with optional `smp`
# requiring 4 online CPUs in sched_getaffinity).
# Usage: check-pthreads.sh <pthreads fixture> [smp]
fixture="${1:?pthreads fixture required}"
smp_flag="${2:-}"
failed=0

check() {
    mode="$1"
    status="$2"
    expected="$3"
    if [ -n "$smp_flag" ]; then
        output="$("$fixture" "$mode" "$smp_flag")"
    else
        output="$("$fixture" "$mode")"
    fi
    actual=$?
    if [ "$output" = "$expected" ] && [ "$actual" -eq "$status" ]; then
        echo "pthreads $mode: ok"
    else
        echo "pthreads $mode: expected status $status, got $actual; output:"
        printf '%s\n' "$output"
        failed=1
    fi
}

check suite 0 'pthreads suite: ok'
check main-pthread-exit 0 'worker completed after main pthread_exit'
check group-exit 42 'non-main exit_group'
check shootdown-fault 139 'end: sibling write after mprotect ro'
exit "$failed"
