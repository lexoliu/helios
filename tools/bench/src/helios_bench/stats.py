"""Statistics over the iterations of one cell.

Every cell (workload x side) is a list of wall-clock samples. The first
``warmup_discard`` iterations are the cold series; the rest are the warm
series that every headline number and the gate use. For a series we
report the median, the interquartile range, the coefficient of variation,
and a percentile-bootstrap confidence interval of the median.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from helios_bench.report import SeriesStats


@dataclass(frozen=True)
class StatsConfig:
    bootstrap_resamples: int
    confidence: float
    bootstrap_seed: int
    cv_bound: float


def series_stats(values: list[float], config: StatsConfig) -> SeriesStats:
    if not values:
        raise ValueError("a series needs at least one sample")
    samples = np.asarray(values, dtype=np.float64)
    q1, median, q3 = (float(value) for value in np.percentile(samples, [25, 50, 75]))
    mean = float(samples.mean())
    stdev = float(samples.std(ddof=1)) if samples.size > 1 else 0.0
    cv = stdev / mean if mean > 0 else 0.0
    ci_low, ci_high = bootstrap_median_ci(samples, config)
    return SeriesStats(
        count=int(samples.size),
        median=median,
        q1=q1,
        q3=q3,
        iqr=q3 - q1,
        mean=mean,
        stdev=stdev,
        cv=cv,
        ci_low=ci_low,
        ci_high=ci_high,
        min=float(samples.min()),
        max=float(samples.max()),
    )


def bootstrap_median_ci(samples: np.ndarray, config: StatsConfig) -> tuple[float, float]:
    """Percentile bootstrap interval of the median with a fixed seed."""
    if samples.size == 1:
        value = float(samples[0])
        return value, value
    generator = np.random.default_rng(config.bootstrap_seed)
    indices = generator.integers(0, samples.size, size=(config.bootstrap_resamples, samples.size))
    medians = np.median(samples[indices], axis=1)
    tail = (1.0 - config.confidence) / 2.0
    low, high = np.percentile(medians, [tail * 100.0, (1.0 - tail) * 100.0])
    return float(low), float(high)


def split_cold_warm(values: list[float], warmup_discard: int) -> tuple[list[float], list[float]]:
    if len(values) <= warmup_discard:
        raise ValueError(f"{len(values)} iterations leave no warm series after discarding {warmup_discard}")
    return values[:warmup_discard], values[warmup_discard:]


def intervals_overlap(a: SeriesStats, b: SeriesStats) -> bool:
    return a.ci_low <= b.ci_high and b.ci_low <= a.ci_high


def relative_shift(before: float, after: float) -> float:
    """``after`` relative to ``before``; positive means slower."""
    if before <= 0:
        raise ValueError("relative shift needs a positive reference")
    return (after - before) / before


def noise_floor(before: SeriesStats, after: SeriesStats) -> float:
    """Machine drift bound from the control workload run before and after.

    The floor is the larger of the control's median drift and its warm
    coefficient of variation, so a quiet machine with a jittery control
    still reports a floor the gate has to clear.
    """
    return max(abs(relative_shift(before.median, after.median)), before.cv, after.cv)
