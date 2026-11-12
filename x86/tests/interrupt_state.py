import hashlib
import json
import platform
import re
import subprocess
import tempfile
from pathlib import Path


def substitute_asm_operands(text, rust_source):
    """Replace each `{name}` the way the `global_asm!` that includes
    exceptions.S does: from its `name = const CONST` operands and the
    literal value of each `CONST`, so the Rust constants stay the one
    source of the vectors."""
    invocation = re.search(
        r'global_asm!\(\s*include_str!\("exceptions\.S"\),(?P<operands>[^)]*)\)',
        rust_source,
    )
    if invocation is None:
        raise SystemExit("x86/src/exceptions.rs has no global_asm! including exceptions.S")
    for name, constant in re.findall(
        r"(\w+)\s*=\s*const\s+(\w+)", invocation.group("operands")
    ):
        definition = re.search(
            rf"const\s+{constant}\s*:\s*\w+\s*=\s*(0x[0-9a-fA-F_]+|[0-9_]+)\s*;",
            rust_source,
        )
        if definition is None:
            raise SystemExit(
                f"operand {name}'s constant {constant} is not a literal in x86/src/exceptions.rs"
            )
        text = text.replace("{" + name + "}", str(int(definition.group(1).replace("_", ""), 0)))
    leftover = re.search(r"\{\w+\}", text)
    if leftover is not None:
        raise SystemExit(f"exceptions.S operand {leftover.group(0)} has no const operand")
    return text


def host_components():
    if platform.system() == "Linux":
        flags = set()
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("Features") or line.startswith("flags"):
                flags.update(line.split(":", 1)[1].split())
        return flags
    if platform.system() == "Darwin":
        output = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.features"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        return set(output.lower().split())
    return set()


def main():
    root = Path(__file__).resolve().parents[2]
    source = root / "x86/src/exceptions.S"
    assembly = source.read_bytes()
    system = platform.system()
    if system == "Darwin":
        flags = ["-arch", "x86_64"]
        text = assembly.decode().replace(
            '.section .text.helios_x86_exceptions, "ax"', ".text", 1
        )
    elif system == "Linux" and platform.machine() == "x86_64":
        flags = []
        text = assembly.decode()
    else:
        raise SystemExit(
            "IRQ register test requires x86-64 Linux or macOS with x86-64 execution"
        )
    text = substitute_asm_operands(text, (root / "x86/src/exceptions.rs").read_text())
    output = root / "target/x86-interrupt-tests"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output) as directory:
        build = Path(directory)
        (build / "entry.S").write_text(text)
        binary = build / "interrupt-state"
        subprocess.run(
            [
                "clang",
                *flags,
                "-I",
                str(build),
                str(root / "x86/tests/interrupt_state.S"),
                "-o",
                str(binary),
            ],
            check=True,
            timeout=60,
        )
        result = subprocess.run([str(binary)], check=False, timeout=10)
        components = subprocess.run([str(binary), "--components"], check=False, timeout=10)
    categories = {
        0: "preserved",
        1: "x87 control/status/tag corrupted",
        2: "MXCSR corrupted",
        3: "x87 register corrupted",
        4: "XMM register corrupted",
        5: "general-purpose register corrupted",
        6: "YMM upper state corrupted",
        7: "ZMM state corrupted",
        8: "opmask state corrupted",
        9: "interrupt stub entered Rust with DF set",
        10: "interrupt stub did not restore the interrupted DF",
        11: "exception stub entered Rust with DF set",
        12: "exception stub did not restore the interrupted DF",
    }
    component_mask = components.returncode
    verified_components = ["x87", "sse"]
    if component_mask & 1:
        verified_components.append("avx")
    if component_mask & 2:
        verified_components.append("avx512")
    flags = host_components()
    print(
        json.dumps(
            {
                "source_sha256": hashlib.sha256(assembly).hexdigest(),
                "exit_code": result.returncode,
                "result": categories.get(result.returncode, "unexpected termination"),
                "verified_components": verified_components,
                "component_mask": component_mask,
                "host_avx": "avx" in flags,
                "host_avx512f": "avx512f" in flags,
                "host_avx512bw": "avx512bw" in flags,
                "nested_interrupt": True,
            }
        )
    )
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
