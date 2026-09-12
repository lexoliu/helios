"""The noise retry: an inconclusive paired pass is measured once more in
the same job, under ``retry/`` beside the first pass (#375).

A floor past the bound means the host moved by more than any effect a
change could show while the suite ran, so no row of that pass can take a
verdict — but one noisy stretch says nothing about the same machine
minutes later. Every case below drives the real `run_suite`: the fake
driver answers `runner.execute` by writing each invocation's JSONL
wherever the command points, so the wiring — retake, reconfirm, control
reading, report assembly, the retry itself — is the production code's.
"""

from __future__ import annotations

import json
from dataclasses import replace
from pathlib import Path

import pytest
from conftest import THRESHOLDS, iterations

from helios_bench import runner
from helios_bench.baseline import Baseline
from helios_bench.gate import evaluate_paired, gate_report
from helios_bench.manifest import load_manifest
from helios_bench.render import format_percent, render_gate, render_tables
from helios_bench.report import Report, Side, load_report, save_report
from helios_bench.runner import RECONFIRM_OUT, RETAKE_OUT, RETRY_OUT, RunOptions, run_suite
from helios_bench.wasi_apps import gap_bench

CONTROL = "quickjs-loop"
NAMES = ["hostcall-loop", "quickjs-loop", "fs-smallfiles"]


def tight(center: float, seed: int):
    return iterations(center, center * 0.02, seed)


def noisy_control(seed: int):
    """The control pair of run 34381896869: a host that moved 28.7%
    between the boot before the suite and the one after."""
    return iterations(100.0, 1.0, seed), iterations(128.7, 1.0, seed + 1)


def quiet_control(seed: int):
    return iterations(100.0, 1.0, seed), iterations(101.0, 1.0, seed + 1)


def first_pass(control) -> dict:
    """A first pass whose suite cells are tight and flat; the control is
    what makes it unreadable or not."""
    return {
        "helios": {name: tight(20.0, 1) for name in NAMES},
        "baseline": {name: tight(20.0, 2) for name in NAMES},
        "control": {"helios": control, "baseline": control},
        "linux": {name: tight(50.0, 3) for name in NAMES},
        "linux_control": quiet_control(4),
    }


def write_jsonl(path: Path, cells: dict[str, list]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [json.dumps({"type": "run", "schema_version": 1})]
    for name, values in cells.items():
        for iteration in values:
            lines.append(
                json.dumps(
                    {
                        "type": "iteration",
                        "workload": name,
                        "iteration": iteration.index,
                        "elapsed_ms": iteration.elapsed_ms,
                        "metrics": dict(iteration.metrics),
                    }
                )
            )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def fake_driver(options: RunOptions, passes: dict[str, dict]):
    """An `execute` that writes each driver invocation's files wherever the
    command points. ``passes`` maps a pass root — "" for the first pass,
    "retry" for the second, "retry/retake"/"retry/reconfirm" for the
    second's follow-ons — to what it records: "helios" and "baseline" cell
    dicts, a "control" dict of (before, after) pairs per image (None where
    the pass is made to lose one), and on "" the "linux" cells and
    "linux_control" pair. A command under a root with no spec fails the
    test: nothing gets invoked that a case did not provide for."""
    executed = []

    def fake(command) -> None:
        executed.append(command)
        if command.argv[0] != "python3":
            # The native-counterparts build records no JSONL.
            return
        args = gap_bench().build_parser().parse_args(command.argv[2:])
        if args.skip_helios:
            spec = passes[""]
            out_dir = Path(args.out_dir)
            write_jsonl(out_dir / "linux-native.jsonl", spec["linux"])
            before, after = spec["linux_control"]
            write_jsonl(out_dir / "linux-native-control-before.jsonl", {CONTROL: before})
            write_jsonl(out_dir / "linux-native-control-after.jsonl", {CONTROL: after})
            return
        helios_out = Path(args.out_dir)
        root = helios_out.parent
        key = "" if root == options.out_dir else root.relative_to(options.out_dir).as_posix()
        spec = passes.get(key)
        if spec is None:
            pytest.fail(f"the driver was invoked under {root}, which no pass of this test provides")
        write_jsonl(
            helios_out / "helios.jsonl",
            {name: spec["helios"][name] for name in args.workloads},
        )
        baseline_out = Path(args.helios_baseline_out_dir) if args.helios_baseline_out_dir else None
        if baseline_out is not None:
            write_jsonl(
                baseline_out / "helios.jsonl",
                {name: spec["baseline"][name] for name in args.workloads},
            )
        if not args.control:
            return
        for image, out_dir in (("helios", helios_out), ("baseline", baseline_out)):
            if out_dir is None:
                continue
            pair = spec.get("control", {}).get(image)
            if pair is None:
                continue
            before, after = pair
            write_jsonl(out_dir / "helios-control-before.jsonl", {CONTROL: before})
            write_jsonl(out_dir / "helios-control-after.jsonl", {CONTROL: after})

    return fake, executed


def out_dirs(executed) -> list[Path]:
    """The ``--out-dir`` every driver invocation was pointed at."""
    dirs = []
    for command in executed:
        if command.argv[0] != "python3":
            continue
        args = gap_bench().build_parser().parse_args(command.argv[2:])
        dirs.append(Path(args.out_dir))
    return dirs


def suite(options: RunOptions, manifest, monkeypatch, report: Report) -> Report:
    """`run_suite` past what only the lane can supply: host probing, the
    baseline worktree, the kernel profile record and the hardware/pins
    collection are replaced by the fixture's; everything from the driver
    down — sides, controls, retake, reconfirm, the retry, the report — is
    the production code's."""
    monkeypatch.setattr(runner, "host_deviations", lambda lane: [])
    monkeypatch.setattr(runner, "prepare_baseline", lambda baseline: None)
    monkeypatch.setattr(runner, "kernel_profiles", lambda options: (None, None))
    monkeypatch.setattr(runner, "kernel_pgo_uncovered", lambda *args: None)
    monkeypatch.setattr(runner, "collect_hardware", lambda lane: report.hardware)
    monkeypatch.setattr(runner, "collect_pins", lambda lane, workloads, kernel_build: report.pins)
    return run_suite(options, manifest)


@pytest.fixture
def options(tmp_path) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE, Side.LINUX_NATIVE}),
        workload_names=NAMES,
        iterations=11,
        baseline=Baseline(ref="merge-base", sha="a" * 40, worktree=tmp_path / "worktree"),
    )


def test_a_noisy_first_pass_with_a_clean_retry_gates_on_the_retry(
    options, baseline_report: Report, monkeypatch, tmp_path
) -> None:
    """Run 34381896869 again: the control drifted 28.7% on the first pass.
    The retry measured a quiet host, so its report is the one the gate
    reads — and the run record carries both floors."""
    options = replace(options, job_timeout_minutes=420)
    fake, executed = fake_driver(
        options,
        {
            "": first_pass(noisy_control(10)),
            "retry": {
                "helios": {name: tight(19.0, 20) for name in NAMES},
                "baseline": {name: tight(20.0, 21) for name in NAMES},
                "control": {"helios": quiet_control(22), "baseline": quiet_control(24)},
            },
        },
    )
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    result = evaluate_paired(report)
    assert result is not None
    assert not result.inconclusive
    assert not result.blocking

    # First pass: native build, the Helios pair, the Linux side. Second:
    # the Helios pair alone, under retry/, control pair included.
    assert len(executed) == 4
    retry_command = executed[-1]
    args = gap_bench().build_parser().parse_args(retry_command.argv[2:])
    assert args.control, "the retry re-measures the control pair, like the suite"
    assert Path(args.out_dir) == options.out_dir / RETRY_OUT / "helios"
    assert Path(args.helios_baseline_out_dir) == options.out_dir / RETRY_OUT / "helios-baseline"
    assert args.workloads == NAMES

    record = report.run.noise_retry
    assert record is not None
    assert not record.skipped_for_budget
    assert record.first_noise_floor > THRESHOLDS.cv_bound
    assert record.second_noise_floor == pytest.approx(report.control.noise_floor)
    assert record.second_noise_floor < THRESHOLDS.cv_bound
    # The floor the gate reads is the retry's alone: the first pass's
    # control stays behind only in the run record.
    assert result.noise_floor == pytest.approx(record.second_noise_floor)
    assert set(report.control.sides) == {Side.HELIOS, Side.HELIOS_BASELINE}

    # The cells the retry did not re-measure are the first pass's, and
    # the Helios columns are the retry's.
    assert report.workload("hostcall-loop").cells[Side.LINUX_NATIVE].warm.median == pytest.approx(
        50.0, rel=0.05
    )
    assert report.workload("hostcall-loop").cells[Side.HELIOS].warm.median == pytest.approx(19.0, rel=0.05)

    # Serialised through the same typed path every other run field uses.
    path = tmp_path / "report.json"
    save_report(report, path)
    assert load_report(path) == report
    assert '"noise_retry"' in path.read_text(encoding="utf-8")

    # The tables and the gate say the run retried and print both floors.
    tables = render_tables(report)
    assert "the suite ran once more" in tables
    assert format_percent(record.first_noise_floor) in tables
    gate_text = render_gate(gate_report(report, None), "x86-64-kvm")
    assert "the suite ran once more" in gate_text
    assert format_percent(record.first_noise_floor) in gate_text
    assert format_percent(record.second_noise_floor) in gate_text


def test_a_second_pass_still_past_the_bound_stays_inconclusive_and_names_both_floors(
    options, baseline_report: Report, monkeypatch
) -> None:
    """A host that could not produce a clean control twice fails the check
    the way it did without the retry — and says so with both floors."""
    fake, executed = fake_driver(
        options,
        {
            "": first_pass(noisy_control(10)),
            "retry": {
                "helios": {name: tight(20.0, 20) for name in NAMES},
                "baseline": {name: tight(20.0, 21) for name in NAMES},
                "control": {"helios": noisy_control(22), "baseline": noisy_control(24)},
            },
        },
    )
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    result = evaluate_paired(report)
    assert result.inconclusive
    assert result.blocking
    record = report.run.noise_retry
    assert record.first_noise_floor > THRESHOLDS.cv_bound
    assert record.second_noise_floor > THRESHOLDS.cv_bound

    text = render_gate(gate_report(report, None), "x86-64-kvm")
    assert "**Blocking: inconclusive**" in text
    assert "could not produce a clean control" in text
    assert format_percent(record.first_noise_floor) in text
    assert format_percent(record.second_noise_floor) in text


def test_an_unpaired_run_is_never_retried(tmp_path, baseline_report: Report, monkeypatch) -> None:
    """An unpaired run has no paired verdict to retry, whatever its
    control's floor says."""
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS}),
        workload_names=NAMES,
        iterations=11,
    )
    fake, executed = fake_driver(
        options,
        {
            "": {
                "helios": {name: tight(20.0, 1) for name in NAMES},
                "control": {"helios": noisy_control(10)},
            },
        },
    )
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    assert report.run.noise_retry is None
    # One driver invocation — the unpaired Helios side — and no retry/.
    assert len(executed) == 1
    assert not (options.out_dir / RETRY_OUT).exists()


def test_a_first_pass_under_the_bound_is_never_retried(options, baseline_report: Report, monkeypatch) -> None:
    fake, executed = fake_driver(options, {"": first_pass(quiet_control(10))})
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    assert report.run.noise_retry is None
    # The three first-pass commands and nothing under retry/.
    assert len(executed) == 3
    assert not (options.out_dir / RETRY_OUT).exists()


def test_a_dispersed_second_pass_is_retaken_under_retry(
    options, baseline_report: Report, monkeypatch
) -> None:
    """Retake applies to the retry pass as to the first: a headline cell
    too dispersed to gate on is measured again under ``retry/retake/``."""
    fake, executed = fake_driver(
        options,
        {
            "": first_pass(noisy_control(10)),
            "retry": {
                "helios": {
                    "hostcall-loop": iterations(20.0, 8.0, 20),
                    "quickjs-loop": tight(20.0, 21),
                    "fs-smallfiles": tight(20.0, 22),
                },
                "baseline": {name: tight(20.0, 23) for name in NAMES},
                "control": {"helios": quiet_control(24), "baseline": quiet_control(26)},
            },
            "retry/retake": {
                "helios": {"hostcall-loop": tight(20.0, 30)},
                "baseline": {"hostcall-loop": tight(20.0, 31)},
            },
        },
    )
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    assert report.run.retaken == ["hostcall-loop"]
    retake_out = options.out_dir / RETRY_OUT / RETAKE_OUT
    assert retake_out / "helios" in out_dirs(executed)
    assert (retake_out / "helios" / "helios.jsonl").is_file()
    assert (retake_out / "helios-baseline" / "helios.jsonl").is_file()
    result = evaluate_paired(report)
    assert not result.inconclusive
    assert not result.blocking


def test_a_regressed_second_pass_is_reconfirmed_under_retry(
    options, baseline_report: Report, monkeypatch
) -> None:
    """Reconfirm applies to the retry pass as to the first: a headline
    regression is timed a second time under ``retry/reconfirm/`` and has
    to show twice."""
    fake, executed = fake_driver(
        options,
        {
            "": first_pass(noisy_control(10)),
            "retry": {
                "helios": {
                    "hostcall-loop": tight(26.0, 20),
                    "quickjs-loop": tight(20.0, 21),
                    "fs-smallfiles": tight(20.0, 22),
                },
                "baseline": {name: tight(20.0, 23) for name in NAMES},
                "control": {"helios": quiet_control(24), "baseline": quiet_control(26)},
            },
            "retry/reconfirm": {
                "helios": {"hostcall-loop": tight(26.0, 40)},
                "baseline": {"hostcall-loop": tight(20.0, 41)},
            },
        },
    )
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    assert report.run.reconfirmed == ["hostcall-loop"]
    reconfirm_out = options.out_dir / RETRY_OUT / RECONFIRM_OUT
    assert (reconfirm_out / "helios" / "helios.jsonl").is_file()
    assert (reconfirm_out / "helios-baseline" / "helios.jsonl").is_file()
    result = evaluate_paired(report)
    # The regression showed on the second pair of boots too: it blocks.
    assert not result.inconclusive
    assert result.blocking


@pytest.mark.parametrize("missing", ["helios", "baseline"])
def test_a_retry_pass_without_its_control_pair_fails_fast(
    options, baseline_report: Report, monkeypatch, missing
) -> None:
    """A retry pass that lost a control pair is a failed pass, not a clean
    one: the run stops naming the pass, the side and the files it expected
    rather than reporting a floor of zero."""
    control = {"helios": quiet_control(22), "baseline": quiet_control(24)}
    control[missing] = None
    fake, executed = fake_driver(
        options,
        {
            "": first_pass(noisy_control(10)),
            "retry": {
                "helios": {name: tight(20.0, 20) for name in NAMES},
                "baseline": {name: tight(20.0, 21) for name in NAMES},
                "control": control,
            },
        },
    )
    monkeypatch.setattr(runner, "execute", fake)

    with pytest.raises(SystemExit) as error:
        suite(options, load_manifest(), monkeypatch, baseline_report)

    side = Side.HELIOS_BASELINE if missing == "baseline" else Side.HELIOS
    message = str(error.value)
    assert "the retry pass" in message
    assert f"the {side} side" in message
    assert "helios-control-before.jsonl" in message
    assert "helios-control-after.jsonl" in message


def test_a_retry_that_would_outlast_the_job_budget_is_skipped(
    options, baseline_report: Report, monkeypatch, capsys
) -> None:
    """`--job-timeout-minutes` is what the job gives the whole run; a second
    pass estimated at the first pass's Helios wall time that cannot fit
    what remains is not started — the first pass stands, inconclusive, and
    the record says why."""
    options = replace(options, job_timeout_minutes=0)
    fake, executed = fake_driver(options, {"": first_pass(noisy_control(10))})
    monkeypatch.setattr(runner, "execute", fake)

    report = suite(options, load_manifest(), monkeypatch, baseline_report)

    record = report.run.noise_retry
    assert record is not None
    assert record.skipped_for_budget
    assert record.first_noise_floor > THRESHOLDS.cv_bound
    assert record.second_noise_floor is None
    assert record.needed_seconds is not None
    assert record.remaining_seconds is not None

    # No second pass ran: the three first-pass commands only.
    assert len(executed) == 3
    assert not (options.out_dir / RETRY_OUT).exists()

    result = evaluate_paired(report)
    assert result.inconclusive
    assert result.blocking

    log = capsys.readouterr().out
    assert "the second pass would need" in log
    assert "remain" in log
    text = render_gate(gate_report(report, None), "x86-64-kvm")
    assert "**Blocking: inconclusive**" in text
    assert "the second pass would need" in text
    assert "of the job's budget" in text
