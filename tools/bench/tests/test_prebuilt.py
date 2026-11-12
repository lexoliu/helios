"""`helios-bench run --prebuilt`: a run on a host that compiles nothing."""

from __future__ import annotations

from pathlib import Path

import pytest

from helios_bench import runner
from helios_bench.manifest import load_manifest
from helios_bench.report import Side
from helios_bench.runner import GAP_BENCH, RunOptions, missing_prebuilt_files, plan, require_prebuilt
from helios_bench.wasi_apps import gap_bench
from helios_bench.workloads import load_workloads

ALL_SIDES = frozenset({Side.HELIOS, Side.LINUX_NATIVE, Side.LINUX_WASMTIME})


def options_for(tmp_path: Path, prebuilt: bool, **overrides) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("aarch64-hvf"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=overrides.pop("sides", ALL_SIDES),
        prebuilt=prebuilt,
        **overrides,
    )


def helios_argv(options: RunOptions) -> list[str]:
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    (command,) = [c for c in commands if "--skip-linux" in c.argv]
    return command.argv


def test_a_prebuilt_plan_builds_nothing_and_skips_the_driver_build(tmp_path) -> None:
    options = options_for(tmp_path, prebuilt=True)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    assert not any(str(runner.NATIVE_BUILD) in command.argv for command in commands)
    assert not any("build" in command.description for command in commands)
    argv = helios_argv(options)
    assert "--helios-skip-build" in argv
    # The driver that receives the flag parses it.
    parsed = gap_bench().build_parser().parse_args(argv[2:])
    assert parsed.helios_skip_build


def test_the_plan_without_the_flag_is_unchanged(tmp_path) -> None:
    options = options_for(tmp_path, prebuilt=False)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    assert commands[0].argv == [str(runner.NATIVE_BUILD), "aarch64"]
    assert "--helios-skip-build" not in helios_argv(options)
    assert [c.argv[1] for c in commands[1:]] == [str(GAP_BENCH)] * 2


def stub_inspector(path: Path, kernel: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\necho {kernel}\n", encoding="utf-8")
    path.chmod(0o755)


@pytest.fixture
def host(tmp_path, monkeypatch) -> dict[str, Path]:
    """Where a prebuilt run would find everything, none of it present yet."""
    target = tmp_path / "target"
    prebuild = target / "kernel-prebuild" / "aarch64-unknown-none" / "kernel-release"
    paths = {
        "inspector": tmp_path / "bin" / "helios-inspector",
        "cli": tmp_path / "bin" / "helios-cli",
        "kernel": target / "aarch64-unknown-none" / "kernel-release" / "helios",
        "bootfs": prebuild / "helios-bootfs",
        "plugin": prebuild / "compiler_plugin.wasm",
        "native": tmp_path / "native",
    }
    monkeypatch.setenv("HELIOS_INSPECTOR_BIN", str(paths["inspector"]))
    monkeypatch.setenv("HELIOS_CLI_BIN", str(paths["cli"]))
    monkeypatch.setattr(runner, "NATIVE_ARTIFACTS", paths["native"])
    monkeypatch.setattr(runner, "compiler_plugin_wasm", lambda lane, kernel_build: paths["plugin"])
    return paths


def test_the_error_lists_every_missing_path(tmp_path, host) -> None:
    stub_inspector(host["inspector"], host["kernel"])
    options = options_for(tmp_path, prebuilt=True)
    workloads = load_workloads()["workloads"]
    natives = runner.native_counterparts(options, workloads)
    assert "hello" in natives and "procbench" in natives
    assert "helios-compiler-host" in natives

    with pytest.raises(SystemExit) as error:
        require_prebuilt(options, workloads)
    message = str(error.value)
    for path in (host["cli"], host["kernel"], host["bootfs"], host["plugin"]):
        assert str(path) in message
    for name in natives:
        assert str(host["native"] / "aarch64" / name) in message
    assert str(host["inspector"]) not in message


def test_nothing_is_missing_once_everything_is_in_place(tmp_path, host) -> None:
    stub_inspector(host["inspector"], host["kernel"])
    host["cli"].write_text("#!/bin/sh\n", encoding="utf-8")
    host["cli"].chmod(0o755)
    options = options_for(tmp_path, prebuilt=True)
    workloads = load_workloads()["workloads"]
    for path in (host["kernel"], host["bootfs"], host["plugin"]):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("", encoding="utf-8")
    (host["native"] / "aarch64").mkdir(parents=True)
    for name in runner.native_counterparts(options, workloads):
        (host["native"] / "aarch64" / name).write_text("", encoding="utf-8")
    assert missing_prebuilt_files(options, workloads) == []


def test_a_missing_inspector_is_listed_with_the_paths_it_would_have_answered(tmp_path, host) -> None:
    options = options_for(tmp_path, prebuilt=True, sides=frozenset({Side.HELIOS}))
    missing = missing_prebuilt_files(options, load_workloads()["workloads"])
    assert any(str(host["inspector"]) in line for line in missing)
    assert any(str(host["cli"]) in line for line in missing)
    assert any("kernel image and helios-bootfs" in line for line in missing)
    assert not any("native" in line for line in missing)
    assert not any("compiler plugin" in line for line in missing)


def test_a_run_that_skips_the_plugin_host_does_not_need_the_plugin(tmp_path, host) -> None:
    options = options_for(tmp_path, prebuilt=True, sides=frozenset({Side.LINUX_NATIVE}))
    missing = missing_prebuilt_files(options, load_workloads()["workloads"])
    assert not any("compiler plugin" in line for line in missing)
    assert not any("helios-compiler-host" in line for line in missing)


def test_the_linux_plan_asks_for_the_helios_columns_compile(tmp_path) -> None:
    options = options_for(tmp_path, prebuilt=True)
    commands = plan(options, load_manifest(), load_workloads()["workloads"])
    (linux,) = [c for c in commands if "--skip-helios" in c.argv]
    parsed = gap_bench().build_parser().parse_args(linux.argv[2:])
    assert parsed.compiler_target == "aarch64-unknown-none"
    assert parsed.compiler_workers == options.lane.compiler_workers
    assert parsed.compiler_plugin_wasm == runner.compiler_plugin_wasm(options.lane, options.kernel_build)
    assert parsed.compiler_plugin_wasm.name == "compiler_plugin.wasm"
