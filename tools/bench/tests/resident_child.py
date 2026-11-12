"""A stand-in resident counterpart for the guest runner's tests.

It speaks `helios-compiler-host`'s protocol: one `{"type": "ready"}` line,
then one `{"type": "compiled", ...}` line per `compile` request, exit 0 at
the end of stdin. `--die-after N` exits 3 instead of answering request N+1,
the way a host that hits an error answers nothing.
"""

import argparse
import json
import sys


def answer(message: dict) -> None:
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--workers", type=int, required=True)
    parser.add_argument("--die-after", type=int, default=None)
    args = parser.parse_args()
    answer({"type": "ready"})
    answered = 0
    for line in sys.stdin:
        if line != "compile\n":
            sys.exit(f"unexpected request {line!r}")
        if args.die_after is not None and answered == args.die_after:
            print("stand-in host failed on purpose", file=sys.stderr)
            sys.exit(3)
        answered += 1
        answer({"type": "compiled", "output_bytes": 1, "metrics": {"compiler_workers": args.workers}})


if __name__ == "__main__":
    main()
