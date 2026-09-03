import numpy as np
import pytest

from helios_bench.report import SeriesStats
from helios_bench.stats import (
    StatsConfig,
    intervals_overlap,
    noise_floor,
    relative_shift,
    series_stats,
    split_cold_warm,
)

CONFIG = StatsConfig(bootstrap_resamples=5000, confidence=0.95, bootstrap_seed=42, cv_bound=0.15)


def test_series_stats_median_iqr_and_cv() -> None:
    stats = series_stats([10.0, 12.0, 11.0, 13.0, 14.0, 12.0, 11.0, 13.0, 12.0, 12.0], CONFIG)
    assert stats.count == 10
    assert stats.median == 12.0
    assert stats.q1 == 11.25
    assert stats.q3 == 12.75
    assert stats.iqr == pytest.approx(1.5)
    assert stats.min == 10.0 and stats.max == 14.0
    assert 0.0 < stats.cv < 0.15


def test_bootstrap_interval_brackets_the_median_and_is_deterministic() -> None:
    generator = np.random.default_rng(3)
    values = list(generator.normal(100.0, 5.0, size=10))
    first = series_stats(values, CONFIG)
    second = series_stats(values, CONFIG)
    assert first.ci_low <= first.median <= first.ci_high
    assert (first.ci_low, first.ci_high) == (second.ci_low, second.ci_high)
    assert first.ci_high - first.ci_low < 20.0


def test_bootstrap_interval_narrows_with_less_spread() -> None:
    generator = np.random.default_rng(4)
    tight = series_stats(list(generator.normal(100.0, 0.5, size=10)), CONFIG)
    loose = series_stats(list(generator.normal(100.0, 8.0, size=10)), CONFIG)
    assert (tight.ci_high - tight.ci_low) < (loose.ci_high - loose.ci_low)


def test_single_sample_interval_is_degenerate() -> None:
    stats = series_stats([7.5], CONFIG)
    assert stats.ci_low == stats.ci_high == 7.5
    assert stats.stdev == 0.0


def test_split_cold_warm_requires_a_warm_series() -> None:
    cold, warm = split_cold_warm([5.0, 1.0, 1.1], 1)
    assert cold == [5.0]
    assert warm == [1.0, 1.1]
    with pytest.raises(ValueError):
        split_cold_warm([5.0], 1)


def stats_with_interval(low: float, high: float, cv: float = 0.01) -> SeriesStats:
    median = (low + high) / 2
    return SeriesStats(
        count=10,
        median=median,
        q1=low,
        q3=high,
        iqr=high - low,
        mean=median,
        stdev=cv * median,
        cv=cv,
        ci_low=low,
        ci_high=high,
        min=low,
        max=high,
    )


def test_interval_overlap() -> None:
    assert intervals_overlap(stats_with_interval(1.0, 2.0), stats_with_interval(1.5, 3.0))
    assert not intervals_overlap(stats_with_interval(1.0, 2.0), stats_with_interval(2.1, 3.0))


def test_noise_floor_is_the_worst_of_drift_and_control_cv() -> None:
    before = stats_with_interval(99.0, 101.0, cv=0.01)
    after = stats_with_interval(102.0, 104.0, cv=0.02)
    assert noise_floor(before, after) == pytest.approx(0.03)
    jittery = stats_with_interval(99.0, 101.0, cv=0.08)
    assert noise_floor(jittery, after) == pytest.approx(0.08)


def test_relative_shift_sign() -> None:
    assert relative_shift(100.0, 110.0) == pytest.approx(0.10)
    assert relative_shift(100.0, 90.0) == pytest.approx(-0.10)
    with pytest.raises(ValueError):
        relative_shift(0.0, 1.0)
