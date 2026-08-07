#!/usr/bin/env python3
"""Synthetic regression tests for the release HIL audio oracle."""

from __future__ import annotations

import math
import tempfile
import unittest
import wave
from array import array
from pathlib import Path

from report_metrics import Pcm, align, read_pcm, source_timing_metrics, validate_stem


THRESHOLDS: dict[str, float | int] = {
    "max_lag_ms": 100,
    "max_duration_error_ms": 20,
    "min_correlation": 0.90,
    "min_isolation_margin": 0.20,
    "max_peak_error": 0.15,
    "max_rms_error": 0.10,
    "max_clipping_samples": 0,
}


def tone(frequency: float, *, channels: int = 1, seconds: float = 0.25) -> Pcm:
    rate = 8000
    values: list[float] = []
    for index in range(round(rate * seconds)):
        sample = 0.4 * math.sin(2 * math.pi * frequency * index / rate)
        values.extend([sample] * channels)
    return Pcm(rate, channels, tuple(values))


class HilOracleTests(unittest.TestCase):
    def test_expected_waveform_passes(self) -> None:
        expected = tone(440)
        other = tone(880)
        metrics = validate_stem("stem", expected, expected, other, THRESHOLDS)
        self.assertGreater(metrics["correlation"], 0.99)

    def test_silence_noise_truncation_swap_and_wrong_signal_fail(self) -> None:
        expected = tone(440)
        other = tone(880)
        noise = Pcm(expected.rate, 1, tuple(0.2 if index % 2 else -0.2 for index in range(expected.frames)))
        cases = [
            Pcm(expected.rate, 1, (0.0,) * expected.frames),
            noise,
            Pcm(expected.rate, 1, expected.samples[: expected.frames // 2]),
            other,
            tone(220),
        ]
        for actual in cases:
            with self.subTest(frames=actual.frames, first=actual.samples[:1]):
                with self.assertRaises(ValueError):
                    validate_stem("stem", actual, expected, other, THRESHOLDS)

    def test_sample_rate_channel_count_and_channel_swap_fail(self) -> None:
        left = tone(440)
        right = tone(880)
        stereo_samples = tuple(
            value
            for pair in zip(left.samples, right.samples)
            for value in pair
        )
        expected = Pcm(left.rate, 2, stereo_samples)
        other = tone(220, channels=2)
        swapped_samples = tuple(
            value
            for pair in zip(right.samples, left.samples)
            for value in pair
        )
        cases = [
            tone(440),
            Pcm(16000, 2, expected.samples),
            Pcm(left.rate, 2, swapped_samples),
        ]
        for actual in cases:
            with self.subTest(rate=actual.rate, channels=actual.channels):
                with self.assertRaises(ValueError):
                    validate_stem("stem", actual, expected, other, THRESHOLDS)

    def test_timing_metrics_measure_drift_and_deadline_misses(self) -> None:
        blocks = []
        for sequence in range(3):
            blocks.append(
                {
                    "source": "microphone",
                    "pcm_start_frame": sequence * 480,
                    "pcm_frame_count": 480,
                    "source_capture_start_ns": sequence * 10_001_000,
                    "callback_arrival_ns": sequence * 10_000_000,
                    "sequence": sequence,
                    "discontinuity_before": False,
                }
            )
        drift, misses = source_timing_metrics(
            {"timeline_blocks": blocks}, "microphone", 48000, 2.0
        )
        self.assertAlmostEqual(drift, -66.7, delta=1.0)
        self.assertEqual(misses, 0)

        blocks[2]["callback_arrival_ns"] = 40_000_000
        _, misses = source_timing_metrics(
            {"timeline_blocks": blocks}, "microphone", 48000, 2.0
        )
        self.assertEqual(misses, 1)

    def test_alignment_reports_bounded_delay(self) -> None:
        state = 1
        values = []
        for _ in range(2000):
            state = (1103515245 * state + 12345) & 0x7FFFFFFF
            values.append((state / 0x7FFFFFFF - 0.5) * 0.5)
        expected = Pcm(8000, 1, tuple(values))
        delayed = Pcm(expected.rate, 1, (0.0,) * 80 + expected.samples[:-80])
        score, lag_ms, _ = align(delayed, expected, 100)
        self.assertGreater(score, 0.9)
        self.assertAlmostEqual(lag_ms, 10, delta=2)

    def test_pcm_reader_rejects_segment_format_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths = []
            for name, channels in (("one.wav", 1), ("two.wav", 2)):
                path = root / name
                values = array("h", [100] * (100 * channels))
                with wave.open(str(path), "wb") as output:
                    output.setnchannels(channels)
                    output.setsampwidth(2)
                    output.setframerate(8000)
                    output.writeframes(values.tobytes())
                paths.append(path)
            with self.assertRaises(ValueError):
                read_pcm(paths)

    def test_pcm_reader_bounds_long_soak_waveform_memory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "long.wav"
            values = array("h", range(1000))
            with wave.open(str(path), "wb") as output:
                output.setnchannels(1)
                output.setsampwidth(2)
                output.setframerate(8000)
                output.writeframes(values.tobytes())
            pcm = read_pcm([path], max_frames=125)
            self.assertEqual(pcm.frames, 125)


if __name__ == "__main__":
    unittest.main()
