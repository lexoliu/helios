#!/usr/bin/env python3
"""Check the tone a Helios guest played into a QEMU `wav` audiodev.

`--audiodev wav:<path>` records what the guest's virtio-sound device
played, so the file is the other end of `programs/audio-test`: it should
hold one partial at 440 Hz, lasting as long as the program said it
played for. A recording that came back the right length but silent, or
loud but at the wrong pitch, is the failure this checks for — a valid
WAV header proves only that QEMU opened the file.

Two assertions:

* the sounding part of the recording lasts as long as the guest played
  for, within one of the kernel's own periods; and
* the strongest partial in it is the one the guest generated, within one
  bin of the transform used to find it.

The transform is a Goertzel filter over a frequency grid rather than an
FFT, because the runner has the Python standard library and nothing
else, and one bin at a time over a few thousand candidate frequencies is
cheap at this size.
"""

from __future__ import annotations

import argparse
import array
import math
import pathlib
import sys
import wave

# The tone `audio-test` generates, and the length it plays for.
DEFAULT_HZ = 440.0
DEFAULT_SECONDS = 2.0

# One of the kernel's playback periods, in milliseconds. The recording's
# sounding span is a whole number of periods plus whatever the device
# still held, so this is the unit its length is allowed to differ by.
PERIOD_MS = 10.0

# Frames the transform is taken over. A power of two purely so the bin
# width is easy to state: at 44.1 kHz it is about eleven hertz, which is
# the resolution every frequency below is quoted at.
WINDOW_FRAMES = 4096

# Where the search for the strongest partial runs, in hertz. Below the
# first is rumble no device reproduces; above the last is beyond
# anything this check is asked about.
SEARCH_LOW_HZ = 20.0
SEARCH_HIGH_HZ = 5000.0

# How far below full scale a frame has to be to count as silence.
#
# `audio-test` plays at half of full scale, and a host backend's own
# resampling leaves the quiet parts of a sine well above this, so the
# span this finds is the tone and not the noise floor.
SILENCE_FLOOR = 0.02

# How much louder the strongest partial has to be than anything outside
# its own main lobe for the recording to be a tone rather than noise.
DOMINANCE = 4.0

# Bins either side of the peak that belong to its own main lobe.
LOBE_BINS = 3


class CheckFailed(Exception):
    """A recording that is not the sound the guest said it played."""


def read_unfinalized(path: str) -> tuple[int, int, int, bytes]:
    """Channels, sample width, rate and samples of a half-written WAV.

    QEMU patches a recording's RIFF and `data` lengths as it closes the
    file, so a run that was killed leaves both at zero and the standard
    library refuses the result. The samples are all there; only the two
    lengths are missing, and the file's own size supplies them. Reading
    it anyway is what lets this check run against the recording a
    retained runtime directory kept from an interrupted session.
    """
    raw = pathlib.Path(path).read_bytes()
    if len(raw) < 44 or raw[:4] != b"RIFF" or raw[8:12] != b"WAVE":
        raise CheckFailed(f"{path} is not a WAV file at all")
    offset = 12
    fmt = None
    while offset + 8 <= len(raw):
        name = raw[offset : offset + 4]
        size = int.from_bytes(raw[offset + 4 : offset + 8], "little")
        body = offset + 8
        if name == b"fmt " and size >= 16:
            fmt = raw[body : body + 16]
        elif name == b"data":
            if fmt is None:
                raise CheckFailed(f"{path} carries samples it never described")
            channels = int.from_bytes(fmt[2:4], "little")
            rate = int.from_bytes(fmt[4:8], "little")
            width = int.from_bytes(fmt[14:16], "little") // 8
            # Zero is the length QEMU never got round to writing; the
            # rest of the file is the recording.
            end = body + size if size else len(raw)
            return channels, width, rate, raw[body:end]
        if size == 0:
            break
        offset = body + size + (size & 1)
    raise CheckFailed(f"{path} carries no samples")


def read_mono(path: str) -> tuple[list[float], int]:
    """The first channel of `path`, as samples in -1.0..1.0, and its rate."""
    try:
        with wave.open(path, "rb") as recording:
            channels = recording.getnchannels()
            width = recording.getsampwidth()
            rate = recording.getframerate()
            raw = recording.readframes(recording.getnframes())
    except wave.Error:
        channels, width, rate, raw = read_unfinalized(path)
        print(f"check-tone: {path} was never closed; reading it by its length")

    if channels < 1 or rate < 1:
        raise CheckFailed(f"{path} describes {channels} channels at {rate} Hz")
    frames = len(raw) // max(channels * width, 1)
    if frames == 0:
        raise CheckFailed(f"{path} holds no frames at all")

    # Eight-bit WAV samples are unsigned — silence is the byte 128 —
    # where every wider width is two's complement. Reading the first as
    # signed folds everything above silence onto the floor.
    typecode = {1: "B", 2: "h", 4: "i"}.get(width)
    if typecode is None:
        raise CheckFailed(
            f"{path} carries {width}-byte samples, which this check does not read"
        )
    samples = array.array(typecode, raw[: frames * channels * width])
    if sys.byteorder == "big" and width > 1:
        # WAV is little-endian; the check has to read it the same way
        # wherever it runs. A one-byte sample has no order to swap.
        samples.byteswap()

    full_scale = float(1 << (8 * width - 1))
    if width == 1:
        return ([sample / 128.0 - 1.0 for sample in samples[::channels]], rate)
    return ([sample / full_scale for sample in samples[::channels]], rate)


def sounding_span(samples: list[float]) -> tuple[int, int]:
    """The first and last frame above the silence floor."""
    first = None
    last = None
    for index, sample in enumerate(samples):
        if abs(sample) >= SILENCE_FLOOR:
            if first is None:
                first = index
            last = index
    if first is None or last is None:
        raise CheckFailed(
            "the recording never rises above the silence floor: the guest played nothing"
        )
    return first, last


def goertzel(window: list[float], rate: int, hz: float) -> float:
    """The magnitude of `hz` in `window`, by Goertzel's recurrence."""
    omega = 2.0 * math.pi * hz / rate
    coefficient = 2.0 * math.cos(omega)
    first = 0.0
    second = 0.0
    for sample in window:
        current = sample + coefficient * first - second
        second = first
        first = current
    real = first - second * math.cos(omega)
    imaginary = second * math.sin(omega)
    return abs(complex(real, imaginary))


def hann(window: list[float]) -> list[float]:
    """`window` tapered, so a partial between two bins does not smear."""
    count = len(window)
    return [
        sample * 0.5 * (1.0 - math.cos(2.0 * math.pi * index / (count - 1)))
        for index, sample in enumerate(window)
    ]


def spectrum(window: list[float], rate: int, step: float) -> list[tuple[float, float]]:
    """The magnitude at every grid frequency in the search range."""
    tapered = hann(window)
    limit = min(SEARCH_HIGH_HZ, rate / 2.0)
    magnitudes = []
    hz = SEARCH_LOW_HZ
    while hz <= limit:
        magnitudes.append((hz, goertzel(tapered, rate, hz)))
        hz += step
    if not magnitudes:
        raise CheckFailed(f"a {rate} Hz recording has no band to search")
    return magnitudes


def dominant(magnitudes: list[tuple[float, float]], step: float) -> tuple[float, float]:
    """The strongest grid frequency, and how much it beats the rest by."""
    peak_hz, peak = max(magnitudes, key=lambda entry: entry[1])
    if peak == 0.0:
        raise CheckFailed("the recording holds no energy in the audible band")
    lobe = LOBE_BINS * step
    outside = [
        magnitude
        for hz, magnitude in magnitudes
        if abs(hz - peak_hz) > lobe
    ]
    runner_up = max(outside) if outside else 0.0
    margin = float("inf") if runner_up == 0.0 else peak / runner_up
    return peak_hz, margin


def check(path: str, expect_hz: float, expect_seconds: float) -> None:
    samples, rate = read_mono(path)
    duration = len(samples) / rate
    first, last = sounding_span(samples)
    sounding = (last - first + 1) / rate
    print(
        f"check-tone: {path} rate={rate} frames={len(samples)} "
        f"duration={duration:.3f}s sounding={sounding:.3f}s"
    )

    tolerance = PERIOD_MS / 1000.0
    if abs(sounding - expect_seconds) > tolerance:
        raise CheckFailed(
            f"the tone sounds for {sounding:.3f}s, not {expect_seconds:.3f}s "
            f"(±{tolerance:.3f}s)"
        )

    if last - first + 1 < WINDOW_FRAMES:
        raise CheckFailed(
            f"the tone is {last - first + 1} frames long, too short to measure over "
            f"{WINDOW_FRAMES}"
        )
    # From the middle of the tone, so neither the attack nor whatever the
    # device still held at the end is in the transform.
    middle = first + (last - first + 1 - WINDOW_FRAMES) // 2
    window = samples[middle : middle + WINDOW_FRAMES]

    # The tone swings both ways around silence. A decode that mistook
    # the samples' sign convention folds it onto one side — which is
    # precisely what an unsigned eight-bit recording read as signed
    # does — and no transform can tell the difference after that.
    if min(window) >= -SILENCE_FLOOR or max(window) <= SILENCE_FLOOR:
        raise CheckFailed(
            "the tone never crosses silence, which is a decode artifact, "
            "not the sine the guest generated"
        )

    bin_hz = rate / WINDOW_FRAMES
    step = bin_hz / 2.0
    magnitudes = spectrum(window, rate, step)
    peak_hz, margin = dominant(magnitudes, step)
    print(
        f"check-tone: dominant={peak_hz:.1f}Hz expected={expect_hz:.1f}Hz "
        f"bin={bin_hz:.1f}Hz margin={margin:.1f}x"
    )

    if abs(peak_hz - expect_hz) > bin_hz:
        raise CheckFailed(
            f"the strongest partial is {peak_hz:.1f}Hz, not {expect_hz:.1f}Hz "
            f"(±{bin_hz:.1f}Hz)"
        )
    if margin < DOMINANCE:
        raise CheckFailed(
            f"the {peak_hz:.1f}Hz partial is only {margin:.1f}x the rest of the band; "
            f"the recording is noise rather than a tone"
        )
    print(f"check-tone: {path} holds the tone the guest played")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wav", help="the recording QEMU's wav audiodev wrote")
    parser.add_argument(
        "--hz",
        type=float,
        default=DEFAULT_HZ,
        help="the frequency the guest played (default: %(default)s)",
    )
    parser.add_argument(
        "--seconds",
        type=float,
        default=DEFAULT_SECONDS,
        help="how long it played for (default: %(default)s)",
    )
    arguments = parser.parse_args()

    try:
        check(arguments.wav, arguments.hz, arguments.seconds)
    except (CheckFailed, wave.Error, OSError) as failure:
        print(f"check-tone: {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
