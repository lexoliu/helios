from __future__ import annotations

import hashlib
import json
from pathlib import Path

from elftools.common.exceptions import ELFError
from elftools.elf.elffile import ELFFile

# A release kernel lives in the `release` directory, or in `profile-use`
# on the target whose release builds read the fetched profile (docs/pgo.md,
# #226): the inspector keeps the two apart so a plain build and a
# profile-guided one never share an artifact. A kernel built against a
# profile named on the command line lives under `target/pgo-kernels/`, in
# a directory keyed to that profile, because it is a `profile-use` build
# too and would otherwise be the release kernel's own artifact (#327);
# the inspector names that directory (`NAMED_PROFILE_KERNELS`).
KERNEL_PATTERNS = (
    "target/*-unknown-none*/release/helios",
    "target/*-unknown-none*/profile-use/helios",
    "target/pgo-kernels/*/*-unknown-none*/profile-use/helios",
    "target/perf-baselines/worktrees/*/target/*-unknown-none*/release/helios",
    "target/perf-baselines/worktrees/*/target/*-unknown-none*/profile-use/helios",
)


def kernel_symbols(image: Path, name: str) -> dict:
    with image.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
        source.seek(0)
        try:
            elf = ELFFile(source)
            table = elf.get_section_by_name(".symtab")
            if table is None:
                raise SystemExit(f"kernel has no symbol table: {image}")
            symbols = [
                {"name": symbol.name, "address": int(symbol["st_value"]), "size": int(symbol["st_size"])}
                for symbol in table.iter_symbols()
                if symbol["st_info"]["type"] == "STT_FUNC" and symbol["st_shndx"] != "SHN_UNDEF"
            ]
            if not symbols:
                raise SystemExit(f"kernel has no defined function symbols: {image}")
            return {
                "schema_version": 1,
                "image": name,
                "sha256": digest,
                "elf_type": elf.header["e_type"],
                "machine": elf.header["e_machine"],
                "entry": int(elf.header["e_entry"]),
                "segments": [
                    {
                        "type": segment["p_type"],
                        "virtual_address": int(segment["p_vaddr"]),
                        "memory_size": int(segment["p_memsz"]),
                        "flags": int(segment["p_flags"]),
                    }
                    for segment in elf.iter_segments()
                ],
                "functions": sorted(symbols, key=lambda symbol: (symbol["address"], symbol["name"])),
            }
        except (ELFError, UnicodeError, ValueError):
            raise SystemExit(f"cannot parse kernel ELF metadata: {image}") from None


def export_kernel_symbols(root: Path, out_dir: Path) -> list[Path]:
    images = sorted({image for pattern in KERNEL_PATTERNS for image in root.glob(pattern)})
    if not images:
        raise SystemExit(f"no release kernel images (release or profile-use builds) found under {root}")
    written = []
    for image in images:
        relative = image.relative_to(root)
        snapshot = kernel_symbols(image, relative.as_posix())
        destination = out_dir / relative.with_suffix(".symbols.json")
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("w", encoding="utf-8") as output:
            json.dump(snapshot, output, indent=2)
            output.write("\n")
        written.append(destination)
    return written
