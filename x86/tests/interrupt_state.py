import hashlib
import json
import platform
import subprocess
import tempfile
from pathlib import Path


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
    categories = {
        0: "preserved",
        1: "x87 control/status/tag corrupted",
        2: "MXCSR corrupted",
        3: "x87 register corrupted",
        4: "XMM register corrupted",
        5: "general-purpose register corrupted",
    }
    print(
        json.dumps(
            {
                "source_sha256": hashlib.sha256(assembly).hexdigest(),
                "exit_code": result.returncode,
                "result": categories.get(result.returncode, "unexpected termination"),
                "nested_interrupt": True,
            }
        )
    )
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
