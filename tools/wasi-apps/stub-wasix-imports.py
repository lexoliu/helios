#!/usr/bin/env python3
"""Rewrites the Wasmer WASIX coreutils module so a plain WASI runtime loads it.

The extracted webc atom imports `wasix_32v1` functions plus a shared
`env.memory`; upstream Wasmtime refuses both, so the Linux + Wasmtime
side of the benchmark has no coreutils it can run. This script converts
the module's text form (`wasm-tools print`/`parse`): the memory import
becomes a shared memory definition — the module uses atomic ops, so
shared is required — and every `wasix_32v1` import becomes a defined
stub. `getcwd` gets a real implementation of the WASIX ABI (the required
length is written through the capacity pointer); every other import
becomes `unreachable`, because a loud trap beats a silently wrong answer
if an applet ever calls one.

Only the import set this artifact declares is accepted: an import the
stub map does not know fails the rewrite rather than emitting a quietly
different module.

  tools/wasi-apps/stub-wasix-imports.py <in.wasm> <out.wasm>
"""

from __future__ import annotations

import argparse
import re
import subprocess
import tempfile
from pathlib import Path
from typing import NoReturn

IMPORT_LINE = re.compile(r'^\s*\(import "([^"]+)" "([^"]+)"')
WASIX_FUNC = re.compile(
    r'^\s*\(import "wasix_32v1" "([a-z0-9_]+)" \(func (\$[^ ]+) \(;\d+;\) \(type (\d+)\)\)\)$'
)
ENV_MEMORY = re.compile(r'^\s*\(import "env" "memory" \(memory \(;\d+;\) (\d+) (\d+) shared\)\)$')
TYPE_FUNC = re.compile(r"^\s*\(type \(;(\d+);\) \(func (.*)\)\)$")

GETCWD = "getcwd"
GETCWD_SIGNATURE = "(param i32 i32) (result i32)"

# The WASIX getcwd ABI: arg0 is the output buffer, arg1 a pointer whose
# loaded value is the buffer's capacity and which receives the required
# path length on return. The process root is "/", so the answer is fixed.
GETCWD_BODY = """(local i32)
    local.get 1
    i32.load
    local.set 2
    local.get 1
    i32.const 1
    i32.store
    local.get 2
    i32.const 1
    i32.lt_u
    if (result i32)
      i32.const 68
    else
      local.get 0
      i32.const 47
      i32.store8
      local.get 2
      i32.const 1
      i32.gt_u
      if
        local.get 0
        i32.const 1
        i32.add
        i32.const 0
        i32.store8
      end
      i32.const 0
    end"""

# Every wasix_32v1 import the coreutils atom declares. Anything not in
# this map is refused: the pin is the whole point of staging by hand.
KNOWN_WASIX = {
    GETCWD,
    "callback_signal",
    "chdir",
    "futex_wait",
    "futex_wake",
    "futex_wake_all",
    "proc_join",
    "proc_spawn",
    "thread_exit",
    "thread_signal",
}


def die(message: str) -> NoReturn:
    raise SystemExit(f"stub-wasix-imports: {message}")


def wasm_tools(*arguments: str, text: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(["wasm-tools", *arguments], check=True, capture_output=True, text=text)


def stubbed_wat(source: str) -> str:
    types: dict[int, str] = {}
    output: list[str] = []
    stubs: list[tuple[str, int, str]] = []
    memory: tuple[str, str] | None = None
    last_import = -1
    for line in source.splitlines():
        declared = TYPE_FUNC.match(line)
        if declared:
            types[int(declared.group(1))] = declared.group(2)
        imported = IMPORT_LINE.match(line)
        if imported:
            function = WASIX_FUNC.match(line)
            if function:
                name, symbol, type_index = function.group(1), function.group(2), int(function.group(3))
                if name not in KNOWN_WASIX:
                    die(f"unknown wasix_32v1 import {name}")
                if name == GETCWD and types.get(type_index) != GETCWD_SIGNATURE:
                    die(f"getcwd has an unexpected signature {types.get(type_index)}")
                stubs.append((symbol, type_index, name))
                continue
            memory_import = ENV_MEMORY.match(line)
            if memory_import:
                memory = (memory_import.group(1), memory_import.group(2))
                continue
            module, name = imported.group(1), imported.group(2)
            if module != "wasi_snapshot_preview1":
                die(f"unexpected import {module}::{name}")
            last_import = len(output)
        output.append(line)

    if memory is None:
        die("no shared env.memory import found")
    if not stubs:
        die("no wasix_32v1 imports found")
    if last_import < 0:
        die("no wasi_snapshot_preview1 imports found")

    minimum, maximum = memory
    emitted = [f'  (memory (export "memory") {minimum} {maximum} shared)']
    for symbol, type_index, name in stubs:
        if name == GETCWD:
            emitted.append(f"  (func {symbol} (type {type_index}) {GETCWD_BODY})")
        else:
            emitted.append(f"  (func {symbol} (type {type_index}) unreachable)")
    output[last_import + 1 : last_import + 1] = emitted
    return "\n".join(output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    printed = wasm_tools("print", str(args.source), text=True).stdout
    rewritten = stubbed_wat(printed)
    with tempfile.NamedTemporaryFile(suffix=".wat", mode="w", delete=False) as handle:
        handle.write(rewritten)
        temporary = handle.name
    try:
        wasm_tools("parse", temporary, "-o", str(args.output))
        wasm_tools("validate", str(args.output))
    finally:
        Path(temporary).unlink()


if __name__ == "__main__":
    main()
