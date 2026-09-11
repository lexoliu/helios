"""A stand-in for `workload-bench.sh`, and the checkouts that carry it.

The Helios side of the driver is a scheduler around one shell script:
which guests it boots, in what order, under what budget, and what it does
when one never answers. That is what these tests are about, so the script
is replaced by one that answers instantly, or never, on demand — and no
test in this directory boots a guest.
"""

from __future__ import annotations

import json
from pathlib import Path

WEDGED = "net"

# Stands in for `workload-bench.sh`: writes a summary line per workload
# and exits, unless it is serving the class that wedges, in which case it
# never returns and leaves a child behind to prove the whole process
# group is torn down rather than just the script.
FAKE_BENCH = """#!/bin/sh
set -eu
if [ -n "${HELIOS_WORKLOAD_BENCH_BUILD_ONLY:-}" ]; then
    sleep "$HELIOS_TEST_BUILD_SECONDS"
    echo built >> "$HELIOS_TEST_BUILD_LOG"
    # The guest artifact this image would boot, standing in for the
    # kernel: its content is the workspace root, so two images are two
    # builds unless a test says otherwise.
    printf '%s\n' "${HELIOS_TEST_KERNEL_CONTENT:-$HELIOS_WORKSPACE_ROOT}" \
        > "$HELIOS_WORKSPACE_ROOT/kernel"
    # And the tooling the image's own checkout would compile, where the
    # driver pinned it — HELIOS_TEST_TOOLS_ABSENT names one a broken
    # build leaves behind, so the refusal that checks it can be seen.
    for tool in "${HELIOS_INSPECTOR_BIN:-}" "${HELIOS_CLI_BIN:-}"; do
        if [ -n "$tool" ] && [ "$tool" != "${HELIOS_TEST_TOOLS_ABSENT:-}" ]; then
            mkdir -p "$(dirname "$tool")"
            printf '#!/bin/sh\nexit 0\n' > "$tool"
            chmod +x "$tool"
        fi
    done
    exit 0
fi
log="$HELIOS_WORKLOAD_BENCH_LOG"
mkdir -p "$(dirname "$log")"
if [ -n "${HELIOS_TEST_ORDER:-}" ]; then
    # The harness that ran, the guest it was pointed at, the inspector
    # it was pinned to, and what it ran.
    printf '%s %s %s %s\n' "$(basename "$PWD")" "$(basename "$HELIOS_WORKSPACE_ROOT")" \
        "${HELIOS_INSPECTOR_BIN:--}" "$HELIOS_WORKLOAD_BENCH_WORKLOADS" >> "$HELIOS_TEST_ORDER"
fi
if [ "$HELIOS_WORKLOAD_BENCH_CLASSES" = "@WEDGED@" ]; then
    sleep 600 &
    echo $! > "$HELIOS_TEST_WEDGE_PID_FILE"
    wait
fi
for name in $(echo "$HELIOS_WORKLOAD_BENCH_WORKLOADS" | tr ',' ' '); do
    printf '{"type":"summary","workload":"%s","class":"%s","headline":false,\
"runner":"program","median_elapsed_ms":1.125,"iterations":1,"elapsed_ms":[1.125],\
"validation":{"ok":true}}\\n' "$name" "$HELIOS_WORKLOAD_BENCH_CLASSES" >> "$log"
done
""".replace("@WEDGED@", WEDGED)


# Stands in for `helios-inspector vm … kernel-path`: the question decides
# the answer. The real one resolves the image from the workspace root it
# is pointed at and from the build the flags describe — a profile named
# with `--profile-use` keys a directory of its own (#327) — so the
# stand-in keys its answer by exactly those two, and leaves an image
# there because a query in a real run follows the build that produced
# one.
FAKE_INSPECTOR = """#!/usr/bin/env python3
import hashlib
import os
import sys
from pathlib import Path

argv = sys.argv[1:]
if argv[-1] != "kernel-path":
    raise SystemExit(f"the stand-in answers kernel-path and nothing else: {argv}")
root = Path(os.environ["HELIOS_WORKSPACE_ROOT"])
build = hashlib.sha256(" ".join([str(root), *argv]).encode()).hexdigest()[:16]
image = root / "target" / build / "helios"
image.parent.mkdir(parents=True, exist_ok=True)
image.write_text(build, encoding="utf-8")
print(image)
"""


def fake_inspector(root: Path) -> Path:
    """An inspector whose `kernel-path` answers without a build."""
    root.mkdir(parents=True, exist_ok=True)
    inspector = root / "helios-inspector"
    inspector.write_text(FAKE_INSPECTOR, encoding="utf-8")
    inspector.chmod(0o755)
    return inspector


def workload(name: str, workload_class: str) -> dict:
    return {"name": name, "class": workload_class, "runner": "program", "headline": False}


WORKLOADS = [
    workload("quickjs-loop", "compute"),
    workload("wasi-tcp-throughput", WEDGED),
    workload("hostcall-loop", "hostcall"),
]


def fake_checkout(root: Path) -> Path:
    """A checkout whose `workload-bench.sh` we control."""
    script = root / "tools/wasi-apps/workload-bench.sh"
    script.parent.mkdir(parents=True)
    script.write_text(FAKE_BENCH, encoding="utf-8")
    script.chmod(0o755)
    return root


def records(log: Path) -> dict[str, dict]:
    return {
        json.loads(line)["workload"]: json.loads(line)
        for line in log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    }
