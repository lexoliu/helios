"""The noise retry: an inconclusive paired pass is measured once more in
the same job, under ``retry/`` beside the first pass (#375).

A floor past the bound means the host moved by more than any effect a
change could show while the suite ran, so no row of that pass can take a
verdict — but one noisy stretch says nothing about the same machine
minutes later. The second pass's report is what the gate reads, and the
run record keeps both floors.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import THRESHOLDS, iterations, raw_side
from conftest import WORKLOADS as REPORT_WORKLOADS

from helios_bench import runner
from helios_bench.assemble import assemble_report
from helios_bench.baseline import Baseline
from helios_bench.gate import evaluate_paired, gate_report
from helios_bench.manifest import load_manifest
from helios_bench.render import format_percent, render_gate, render_tables
from helios_bench.report import Report, Side, load_report, save_report
from helios_bench.runner import RETRY_OUT, RunOptions, retry
from helios_bench.wasi_apps import gap_bench

CONTROL = "quickjs-loop"
NAMES = [workload["name"] for workload in REPORT_WORKLOADS]


def tight(center: float, seed: int):
    return iterations(center, center * 0.02, seed)


@pytest.fixture
def options(tmp_path) -> RunOptions:
    return RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS, Side.HELIOS_BASELINE, Side.LINUX_NATIVE}),
        baseline=Baseline(ref="merge-base", sha="a" * 40, worktree=tmp_path / "worktree"),
    )


def first_pass_sides():
    """What the first pass's `read_sides` produced: both Helios images'
    cells, which the retry's replace, and the Linux side's, which stand."""
    return {
        Side.HELIOS: raw_side({name: tight(20.0, 1) for name in NAMES}),
        Side.HELIOS_BASELINE: raw_side({name: tight(20.0, 2) for name in NAMES}),
        Side.LINUX_NATIVE: raw_side({name: tight(50.0, 3) for name in NAMES}),
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


def fake_pass(helios: dict[str, list], baseline: dict[str, list], control: tuple[list, list]):
    """An `execute` that answers the retry's driver invocation with both
    images' files wherever it points: `helios.jsonl` and, since the suite
    passes `--control`, the before/after control pair."""
    executed = []

    def fake(command):
        executed.append(command)
        args = gap_bench().build_parser().parse_args(command.argv[2:])
        write_jsonl(Path(args.out_dir) / "helios.jsonl", {name: helios[name] for name in args.workloads})
        write_jsonl(
            Path(args.helios_baseline_out_dir) / "helios.jsonl",
            {name: baseline[name] for name in args.workloads},
        )
        if args.control:
            before, after = control
            for out_dir in (Path(args.out_dir), Path(args.helios_baseline_out_dir)):
                write_jsonl(out_dir / "helios-control-before.jsonl", {CONTROL: before})
                write_jsonl(out_dir / "helios-control-after.jsonl", {CONTROL: after})

    return fake, executed


def second_pass_build(template: Report):
    """The assemble step `run_suite`'s `build` performs, over the
    template's run record."""

    def build(sides, control, retaken, reconfirmed, noise_retry):
        run = template.run.model_copy(
            update={
                "retaken": retaken,
                "reconfirmed": reconfirmed,
                "noise_retry": noise_retry,
            }
        )
        return assemble_report(
            REPORT_WORKLOADS, sides, control, run, template.hardware, template.pins, THRESHOLDS
        )

    return build


def test_a_noisy_first_pass_with_a_clean_retry_gates_on_the_retry(
    paired_noisy_host_report: Report, options, monkeypatch, tmp_path
) -> None:
    """Run 34381896869 again: the control drifted 28.7% on the first pass.
    The retry measured a quiet host, so its report is the one the gate
    reads — and the run record carries both floors."""
    fake, executed = fake_pass(
        helios={name: tight(19.0, 10) for name in NAMES},
        baseline={name: tight(20.0, 11) for name in NAMES},
        control=(iterations(100.0, 1.0, 12), iterations(101.0, 1.0, 13)),
    )
    monkeypatch.setattr(runner, "execute", fake)

    second = retry(
        options,
        11,
        REPORT_WORKLOADS,
        paired_noisy_host_report,
        first_pass_sides(),
        THRESHOLDS,
        second_pass_build(paired_noisy_host_report),
    )

    assert second is not None
    result = evaluate_paired(second)
    assert not result.inconclusive
    assert not result.blocking

    # Exactly one pass ran: the cells are tight enough that no retake
    # fired, and nothing regressed so no reconfirm did either.
    assert len(executed) == 1
    args = gap_bench().build_parser().parse_args(executed[0].argv[2:])
    assert args.control, "the retry re-measures the control pair, like the suite"
    assert Path(args.out_dir) == options.out_dir / RETRY_OUT / "helios"
    assert Path(args.helios_baseline_out_dir) == options.out_dir / RETRY_OUT / "helios-baseline"
    assert args.workloads == NAMES

    record = second.run.noise_retry
    assert record is not None
    first_floor = evaluate_paired(paired_noisy_host_report).noise_floor
    assert record.first_noise_floor == pytest.approx(first_floor)
    assert record.first_noise_floor > THRESHOLDS.cv_bound
    assert record.second_noise_floor == pytest.approx(second.control.noise_floor)
    assert record.second_noise_floor < THRESHOLDS.cv_bound
    # The floor the gate reads is the retry's alone: the first pass's
    # control stays behind only in the run record.
    assert result.noise_floor == pytest.approx(record.second_noise_floor)
    assert set(second.control.sides) == {Side.HELIOS, Side.HELIOS_BASELINE}

    # The cells the retry did not re-measure are the first pass's, and
    # the Helios columns are the retry's.
    assert second.workload("hostcall-loop").cells[Side.LINUX_NATIVE].warm.median == pytest.approx(
        50.0, rel=0.05
    )
    assert second.workload("hostcall-loop").cells[Side.HELIOS].warm.median == pytest.approx(19.0, rel=0.05)

    # Serialised through the same typed path every other run field uses.
    path = tmp_path / "report.json"
    save_report(second, path)
    assert load_report(path) == second
    assert '"noise_retry"' in path.read_text(encoding="utf-8")

    # The tables and the gate say the run retried and print both floors.
    tables = render_tables(second)
    assert "the suite ran once more" in tables
    assert format_percent(record.first_noise_floor) in tables
    gate_text = render_gate(gate_report(second, None), "x86-64-kvm")
    assert "the suite ran once more" in gate_text
    assert format_percent(record.first_noise_floor) in gate_text
    assert format_percent(record.second_noise_floor) in gate_text


def test_a_second_pass_still_past_the_bound_stays_inconclusive_and_names_both_floors(
    paired_noisy_host_report: Report, options, monkeypatch
) -> None:
    """A host that could not produce a clean control twice fails the check
    the way it did without the retry — and says so with both floors."""
    fake, executed = fake_pass(
        helios={name: tight(20.0, 10) for name in NAMES},
        baseline={name: tight(20.0, 11) for name in NAMES},
        control=(iterations(100.0, 1.0, 12), iterations(121.0, 1.0, 13)),
    )
    monkeypatch.setattr(runner, "execute", fake)

    second = retry(
        options,
        11,
        REPORT_WORKLOADS,
        paired_noisy_host_report,
        first_pass_sides(),
        THRESHOLDS,
        second_pass_build(paired_noisy_host_report),
    )

    assert second is not None
    result = evaluate_paired(second)
    assert result.inconclusive
    assert result.blocking
    record = second.run.noise_retry
    assert record.first_noise_floor > THRESHOLDS.cv_bound
    assert record.second_noise_floor > THRESHOLDS.cv_bound

    text = render_gate(gate_report(second, None), "x86-64-kvm")
    assert "**Blocking: inconclusive**" in text
    assert "could not produce a clean control" in text
    assert format_percent(record.first_noise_floor) in text
    assert format_percent(record.second_noise_floor) in text


def test_an_unpaired_run_is_never_retried(baseline_report: Report, tmp_path, monkeypatch) -> None:
    options = RunOptions(
        lane=load_manifest().lane("x86-64-kvm"),
        out_dir=tmp_path / "out",
        advisory=True,
        sides=frozenset({Side.HELIOS}),
    )
    monkeypatch.setattr(
        runner,
        "execute",
        lambda command: pytest.fail("an unpaired run has no paired verdict to retry"),
    )
    assert (
        retry(
            options,
            11,
            REPORT_WORKLOADS,
            baseline_report,
            {},
            THRESHOLDS,
            second_pass_build(baseline_report),
        )
        is None
    )


def test_a_first_pass_under_the_bound_is_never_retried(
    paired_flat_report: Report, options, monkeypatch
) -> None:
    monkeypatch.setattr(
        runner,
        "execute",
        lambda command: pytest.fail("the first pass was readable; nothing to retry"),
    )
    assert (
        retry(
            options,
            11,
            REPORT_WORKLOADS,
            paired_flat_report,
            {},
            THRESHOLDS,
            second_pass_build(paired_flat_report),
        )
        is None
    )
