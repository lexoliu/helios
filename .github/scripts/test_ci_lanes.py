"""Tests for ci_lanes.py: `uv run --with pytest pytest .github/scripts`."""

import tomllib
from pathlib import Path

import pytest

from ci_lanes import LaneTableError, load_table, select

REPO = Path(__file__).resolve().parents[2]
TABLE = REPO / ".github" / "ci-lanes.toml"


def workspace_members():
    """Every package cargo would count as a member: each manifest under the
    repository that carries a `[package]` and no `[workspace]` of its own,
    plus the root package (which carries both)."""
    names = {tomllib.loads((REPO / "Cargo.toml").read_text())["package"]["name"]}
    for manifest in REPO.rglob("Cargo.toml"):
        relative = manifest.relative_to(REPO)
        if relative == Path("Cargo.toml") or {"target", ".git"} & set(relative.parts):
            continue
        document = tomllib.loads(manifest.read_text())
        if "package" in document and "workspace" not in document:
            names.add(document["package"]["name"])
    return names


MEMBERS = workspace_members()


def report(affected=(), owners=None, workspace=False):
    return {
        "workspace": workspace,
        "affected": sorted(affected),
        "members": sorted(MEMBERS),
        "owners": owners or {},
    }


def running(outputs):
    return {name for name, runs in outputs["lanes"].items() if runs}


@pytest.fixture(scope="module")
def table():
    return load_table(TABLE, MEMBERS)


def test_every_lane_runs_when_the_diff_reaches_the_workspace(table):
    outputs, reasons = select(table, report(workspace=True))
    assert running(outputs) == {lane.name for lane in table.lanes}
    assert all(hit == ["workspace"] for hit in reasons.values())
    assert [entry["target"] for entry in outputs["prebuild"]] == [
        "aarch64-unknown-none",
        "riscv64gc-unknown-none-elf",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-none",
    ]


def test_prose_selects_nothing(table):
    outputs, _ = select(table, report(owners={"docs/boot.md": None}))
    assert running(outputs) == set()
    assert outputs["matrix"] == []
    assert outputs["prebuild"] == []


def test_a_netstack_change_reaches_every_kernel_lane_and_no_program_lane(table):
    # What the tool reports for a change to netstack/src/lib.rs.
    affected = {
        "helios",
        "helios-aarch64",
        "helios-hosted",
        "helios-kernel",
        "helios-netstack",
        "helios-riscv",
        "helios-virtio",
        "helios-x86",
    }
    outputs, _ = select(
        table, report(affected, owners={"netstack/src/lib.rs": "helios-netstack"})
    )
    assert running(outputs) == {
        "lint-fmt",
        "check-host",
        "lint-host",
        "lint-aarch64",
        "lint-riscv",
        "lint-x86",
        "test-units",
        "test-embedded-debugger",
        "smoke-x86-64",
        "smoke-riscv64",
        "smoke-aarch64",
        "bench-x86-64-linux",
    }


def test_one_backend_runs_only_its_own_lanes(table):
    # helios-riscv's only reverse dependency is the root package, which the
    # report names without any file of its own changing.
    affected = {"helios-riscv", "helios"}
    outputs, reasons = select(
        table, report(affected, owners={"riscv/src/lib.rs": "helios-riscv"})
    )
    assert running(outputs) == {"lint-fmt", "lint-riscv", "smoke-riscv64"}
    assert reasons["smoke-x86-64"] == []
    assert [entry["target"] for entry in outputs["prebuild"]] == ["riscv64gc-unknown-none-elf"]
    assert outputs["prebuild"][0]["save-cache"] is False


def test_the_root_package_runs_every_smoke_lane_when_its_own_file_changes(table):
    outputs, reasons = select(table, report({"helios"}, owners={"src/main.rs": "helios"}))
    assert running(outputs) == {
        "lint-fmt",
        "smoke-x86-64",
        "smoke-riscv64",
        "smoke-aarch64",
        "bench-x86-64-linux",
    }
    assert reasons["smoke-aarch64"] == ["helios"]


def test_a_boot_program_reaches_the_lanes_that_boot_or_lint_it(table):
    outputs, _ = select(
        table, report({"helios-date"}, owners={"programs/date/src/lib.rs": "helios-date"})
    )
    assert running(outputs) == {
        "lint-fmt",
        "lint-host",
        "test-units",
        "test-embedded-debugger",
        "smoke-x86-64",
        "smoke-riscv64",
        "smoke-aarch64",
        "bench-x86-64-linux",
    }
    assert outputs["prebuild"] == [{"target": "x86_64-unknown-linux-gnu", "save-cache": True}]


def test_matrix_entries_carry_the_settings_the_job_reads(table):
    outputs, _ = select(table, report(workspace=True))
    by_name = {entry["name"]: entry for entry in outputs["matrix"]}
    assert set(by_name) == {
        "lint-fmt",
        "check-host",
        "lint-host",
        "lint-aarch64",
        "lint-riscv",
        "lint-x86",
        "test-units",
        "test-embedded-debugger",
    }
    assert by_name["lint-fmt"]["cache"] == "" and by_name["lint-fmt"]["save-cache"] is False
    assert by_name["test-units"]["tools"] == "nextest" and by_name["test-units"]["uv"] is True
    embedded = by_name["test-embedded-debugger"]
    assert embedded["cache"] == "test-units" and embedded["save-cache"] is False
    assert by_name["check-host"]["apt"] == "libudev-dev"


def write(tmp_path, text):
    path = tmp_path / "lanes.toml"
    path.write_text('[prebuild]\ncache = "prebuild"\ncache-writer = "x86_64-unknown-linux-gnu"\n' + text)
    return path


def test_a_package_the_workspace_lacks_is_refused(tmp_path):
    path = write(tmp_path, '[[lane]]\nname = "x"\npackages = ["helios-gone"]\n')
    with pytest.raises(LaneTableError, match="helios-gone"):
        load_table(path, MEMBERS)


def test_an_undefined_group_is_refused(tmp_path):
    path = write(tmp_path, '[[lane]]\nname = "x"\npackages = ["@missing"]\n')
    with pytest.raises(LaneTableError, match="@missing"):
        load_table(path, MEMBERS)


def test_a_lane_needs_exactly_one_trigger_set(tmp_path):
    path = write(
        tmp_path,
        '[[lane]]\nname = "x"\npackages = ["helios"]\nall-except = ["helios"]\n',
    )
    with pytest.raises(LaneTableError, match="exactly one"):
        load_table(path, MEMBERS)


def test_matrix_settings_on_a_job_lane_are_refused(tmp_path):
    path = write(tmp_path, '[[lane]]\nname = "x"\npackages = ["helios"]\ncache = "x"\n')
    with pytest.raises(LaneTableError, match="never be read"):
        load_table(path, MEMBERS)


def test_an_unknown_key_is_refused(tmp_path):
    path = write(tmp_path, '[[lane]]\nname = "x"\npackages = ["helios"]\ncomand = "just x"\n')
    with pytest.raises(LaneTableError, match="comand"):
        load_table(path, MEMBERS)


def test_a_lane_may_not_write_the_prebuild_key(tmp_path):
    path = write(
        tmp_path,
        '[[lane]]\nname = "x"\npackages = ["helios"]\ncommand = "just x"\ncache = "prebuild"\n',
    )
    with pytest.raises(LaneTableError, match="prebuild"):
        load_table(path, MEMBERS)
