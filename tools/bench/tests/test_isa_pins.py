"""The guest ISA level and the Linux column's inferred flags, read from the run records."""

from __future__ import annotations

import pytest

from helios_bench.report import GuestIsa, Side
from helios_bench.runner import guest_isa_of, linux_wasmtime_isa_flags


def test_the_helios_run_record_names_the_level_and_its_extras() -> None:
    run = {"type": "run", "guest_isa": {"level": "x86-64-v4", "extras": ["vbmi", "bitalg"]}}
    isa = guest_isa_of(run, Side.HELIOS, required=True)
    assert isa == GuestIsa(level="x86-64-v4", extras=["vbmi", "bitalg"])
    assert isa.describe() == "x86-64-v4+vbmi+bitalg"


def test_a_level_without_extras_reads_as_its_name() -> None:
    run = {"type": "run", "guest_isa": {"level": "x86-64-v3", "extras": []}}
    isa = guest_isa_of(run, Side.HELIOS, required=True)
    assert isa is not None and isa.describe() == "x86-64-v3"


def test_the_candidate_without_a_level_is_a_broken_run() -> None:
    with pytest.raises(SystemExit, match="no `guest_isa`"):
        guest_isa_of({"type": "run"}, Side.HELIOS, required=True)


def test_a_baseline_from_before_the_field_reads_as_unrecorded() -> None:
    assert guest_isa_of({"type": "run"}, Side.HELIOS_BASELINE, required=False) is None


def test_the_linux_run_record_names_the_inferred_flags() -> None:
    assert linux_wasmtime_isa_flags({"inferred_isa_flags": ["has_avx"]}) == ["has_avx"]
    with pytest.raises(SystemExit, match="no `inferred_isa_flags`"):
        linux_wasmtime_isa_flags({"type": "run"})
