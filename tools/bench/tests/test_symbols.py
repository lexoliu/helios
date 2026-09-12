import hashlib
import json
import struct

import pytest

from helios_bench.cli import main
from helios_bench.symbols import export_kernel_symbols, kernel_symbols


@pytest.fixture
def kernel_elf() -> bytes:
    private = b"test-only-private-signing-bytes!!"
    names = b"\0.shstrtab\0.text\0.data\0.strtab\0.symtab\0"
    strings = b"\0sample_function\0TRUSTED_ROOT_SIGNING_KEY\0"
    text = bytes.fromhex("c3")
    data = bytearray(0x2000)
    data[0x1000 : 0x1000 + len(text)] = text
    data.extend(private)
    names_offset = len(data)
    data.extend(names)
    strings_offset = len(data)
    data.extend(strings)
    data.extend(b"\0" * (-len(data) % 8))
    symbols_offset = len(data)
    symbols = bytes(24)
    symbols += struct.pack("<IBBHQQ", 1, 0x12, 0, 2, 0x401000, len(text))
    symbols += struct.pack("<IBBHQQ", strings.index(b"TRUSTED"), 0x11, 0, 3, 0x402000, len(private))
    data.extend(symbols)
    sections_offset = len(data)
    sections = [
        (0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
        (names.index(b".shstrtab"), 3, 0, 0, names_offset, len(names), 0, 0, 1, 0),
        (names.index(b".text"), 1, 6, 0x401000, 0x1000, len(text), 0, 0, 16, 0),
        (names.index(b".data"), 1, 3, 0x402000, 0x2000, len(private), 0, 0, 8, 0),
        (names.index(b".strtab"), 3, 0, 0, strings_offset, len(strings), 0, 0, 1, 0),
        (names.index(b".symtab"), 2, 0, 0, symbols_offset, len(symbols), 4, 1, 8, 24),
    ]
    for section in sections:
        data.extend(struct.pack("<IIQQQQIIQQ", *section))
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    data[:64] = struct.pack(
        "<16sHHIQQQIHHHHHH",
        ident,
        2,
        62,
        1,
        0x401000,
        64,
        sections_offset,
        0,
        64,
        56,
        2,
        64,
        len(sections),
        1,
    )
    data[64:120] = struct.pack("<IIQQQQQQ", 1, 5, 0x1000, 0x401000, 0x401000, len(text), len(text), 0x1000)
    data[120:176] = struct.pack(
        "<IIQQQQQQ", 1, 6, 0x2000, 0x402000, 0x402000, len(private), len(private), 0x1000
    )
    return bytes(data)


def test_symbols_exclude_private_data_and_non_function_objects(tmp_path, kernel_elf):
    image = tmp_path / "helios"
    image.write_bytes(kernel_elf)
    snapshot = kernel_symbols(image, "target/x86_64-unknown-none/release/helios")
    assert snapshot["functions"] == [{"name": "sample_function", "address": 0x401000, "size": 1}]
    assert snapshot["sha256"] == hashlib.sha256(kernel_elf).hexdigest()
    assert snapshot["entry"] == 0x401000
    assert snapshot["machine"] == "EM_X86_64"
    assert len(snapshot["segments"]) == 2
    encoded = json.dumps(snapshot)
    assert "test-only-private-signing-bytes" not in encoded
    assert "TRUSTED_ROOT_SIGNING_KEY" not in encoded
    assert set(snapshot) == {
        "schema_version",
        "image",
        "sha256",
        "elf_type",
        "machine",
        "entry",
        "segments",
        "functions",
    }


def test_export_keeps_both_image_identities_without_copying_elfs(tmp_path, kernel_elf):
    root = tmp_path / "checkout"
    images = [
        root / "target/x86_64-unknown-none/release/helios",
        root / "target/perf-baselines/worktrees/baseline/target/x86_64-unknown-none/release/helios",
    ]
    for image in images:
        image.parent.mkdir(parents=True)
        image.write_bytes(kernel_elf)
    output = tmp_path / "symbols"
    with pytest.raises(SystemExit) as exited:
        main(["symbols", "--root", str(root), "--out-dir", str(output)])
    assert exited.value.code == 0
    files = sorted(path for path in output.rglob("*") if path.is_file())
    assert len(files) == 2
    assert all(path.name.endswith(".symbols.json") for path in files)
    snapshots = [json.loads(path.read_text()) for path in files]
    assert {snapshot["image"] for snapshot in snapshots} == {
        image.relative_to(root).as_posix() for image in images
    }
    assert all(not path.read_bytes().startswith(b"\x7fELF") for path in files)


def test_export_finds_a_profile_use_release_kernel(tmp_path, kernel_elf):
    """An x86-64 release build reads the fetched profile and lands in
    `profile-use` (docs/pgo.md, #226); the symbols beside a bench run
    have to come from the image that was booted."""
    root = tmp_path / "checkout"
    images = [
        root / "target/x86_64-unknown-none/profile-use/helios",
        root / "target/perf-baselines/worktrees/baseline/target/x86_64-unknown-none/profile-use/helios",
    ]
    for image in images:
        image.parent.mkdir(parents=True)
        image.write_bytes(kernel_elf)
    written = export_kernel_symbols(root, tmp_path / "symbols")
    assert len(written) == 2
    snapshots = [json.loads(path.read_text()) for path in written]
    assert {snapshot["image"] for snapshot in snapshots} == {
        image.relative_to(root).as_posix() for image in images
    }


def test_export_finds_a_kernel_built_against_a_named_profile(tmp_path, kernel_elf):
    """A `--profile-use` kernel is a `profile-use` build in a directory
    keyed to its profile (#327), so the export has to look there too or
    the candidate column of a PGO pairing has no symbols."""
    root = tmp_path / "checkout"
    image = root / "target/pgo-kernels/6f1c9a2b3d4e5f60/x86_64-unknown-none/profile-use/helios"
    image.parent.mkdir(parents=True)
    image.write_bytes(kernel_elf)

    written = export_kernel_symbols(root, tmp_path / "symbols")

    assert len(written) == 1
    assert json.loads(written[0].read_text())["image"] == image.relative_to(root).as_posix()


def test_missing_kernels_are_refused(tmp_path):
    with pytest.raises(SystemExit, match="no release kernel images"):
        export_kernel_symbols(tmp_path, tmp_path / "out")


def test_non_elf_input_is_refused_without_echoing_contents(tmp_path):
    image = tmp_path / "helios"
    image.write_bytes(b"test-only-private-signing-bytes")
    with pytest.raises(SystemExit, match="cannot parse kernel ELF metadata") as error:
        kernel_symbols(image, "helios")
    assert "private-signing-bytes" not in str(error.value)
