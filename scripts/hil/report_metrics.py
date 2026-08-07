#!/usr/bin/env python3
"""Validate deterministic microphone/system HIL captures against PCM fixtures."""

from __future__ import annotations

import argparse
from array import array
from dataclasses import dataclass
import json
import math
import os
import sys
import wave
from pathlib import Path


@dataclass(frozen=True)
class Pcm:
    rate: int
    channels: int
    samples: tuple[float, ...]

    @property
    def frames(self) -> int:
        return len(self.samples) // self.channels


def read_pcm(paths: list[Path], max_frames: int | None = None) -> Pcm:
    """Read PCM segments, optionally bounding waveform analysis memory.

    One-hour release soaks are assessed from the complete timeline manifest;
    waveform identity needs only a fixture-sized capture window. The bound
    prevents loading multi-gigabyte stems into Python tuples.
    """
    rate = 0
    channels = 0
    combined: list[float] = []
    frames_read = 0
    for path in paths:
        with wave.open(str(path), "rb") as wav:
            if wav.getsampwidth() != 2 or wav.getcomptype() != "NONE":
                raise ValueError(f"expected uncompressed PCM16 WAV: {path.name}")
            if rate and (wav.getframerate(), wav.getnchannels()) != (rate, channels):
                raise ValueError("stem segments changed sample rate or channel mapping")
            rate, channels = wav.getframerate(), wav.getnchannels()
            requested = wav.getnframes()
            if max_frames is not None:
                requested = min(requested, max_frames - frames_read)
            if requested <= 0:
                break
            values = array("h")
            values.frombytes(wav.readframes(requested))
            if sys.byteorder != "little":
                values.byteswap()
            combined.extend(float(value) / 32768.0 for value in values)
            frames_read += requested
            if max_frames is not None and frames_read >= max_frames:
                break
    if rate <= 0 or channels <= 0 or not combined:
        raise ValueError("audio contains no samples")
    return Pcm(rate, channels, tuple(combined))


def channel(pcm: Pcm, channel_index: int) -> Pcm:
    if not 0 <= channel_index < pcm.channels:
        raise ValueError("channel index is outside the PCM mapping")
    return Pcm(
        pcm.rate,
        1,
        tuple(pcm.samples[channel_index::pcm.channels]),
    )


def mono(pcm: Pcm) -> list[float]:
    return [
        sum(pcm.samples[index : index + pcm.channels]) / pcm.channels
        for index in range(0, len(pcm.samples), pcm.channels)
    ]


def resample(values: list[float], source_rate: int, target_rate: int) -> list[float]:
    if source_rate == target_rate:
        return values
    output_length = max(1, round(len(values) * target_rate / source_rate))
    scale = source_rate / target_rate
    output: list[float] = []
    for output_index in range(output_length):
        position = output_index * scale
        left = min(int(position), len(values) - 1)
        right = min(left + 1, len(values) - 1)
        fraction = position - left
        output.append(values[left] * (1.0 - fraction) + values[right] * fraction)
    return output


def correlation(left: list[float], right: list[float]) -> float:
    if len(left) != len(right) or len(left) < 2:
        return 0.0
    left_mean = sum(left) / len(left)
    right_mean = sum(right) / len(right)
    numerator = 0.0
    left_energy = 0.0
    right_energy = 0.0
    for left_value, right_value in zip(left, right):
        centered_left = left_value - left_mean
        centered_right = right_value - right_mean
        numerator += centered_left * centered_right
        left_energy += centered_left * centered_left
        right_energy += centered_right * centered_right
    denominator = math.sqrt(left_energy * right_energy)
    return numerator / denominator if denominator else 0.0


def align(actual: Pcm, expected: Pcm, max_lag_ms: int) -> tuple[float, int, int]:
    target_rate = min(actual.rate, expected.rate, 1000)
    actual_low = resample(mono(actual), actual.rate, target_rate)
    expected_low = resample(mono(expected), expected.rate, target_rate)
    max_lag = max_lag_ms * target_rate // 1000
    minimum_overlap = max(2, int(min(len(actual_low), len(expected_low)) * 0.8))
    best = (-1.0, 0, 0)
    for lag in range(-max_lag, max_lag + 1):
        actual_start = max(lag, 0)
        expected_start = max(-lag, 0)
        overlap = min(
            len(actual_low) - actual_start,
            len(expected_low) - expected_start,
        )
        if overlap < minimum_overlap:
            continue
        score = correlation(
            actual_low[actual_start : actual_start + overlap],
            expected_low[expected_start : expected_start + overlap],
        )
        if score > best[0]:
            best = (score, lag * 1000 // target_rate, overlap * 1000 // target_rate)
    return best


def signal_metrics(pcm: Pcm) -> tuple[float, float, int]:
    peak = max(abs(value) for value in pcm.samples)
    rms = math.sqrt(sum(value * value for value in pcm.samples) / len(pcm.samples))
    clipping = sum(abs(value) >= 32767 / 32768 for value in pcm.samples)
    return peak, rms, clipping


def validate_stem(
    name: str,
    actual: Pcm,
    expected: Pcm,
    other_expected: Pcm,
    thresholds: dict[str, float | int],
) -> dict[str, object]:
    if actual.rate != expected.rate:
        raise ValueError(f"{name} sample rate {actual.rate} != expected {expected.rate}")
    if actual.channels != expected.channels:
        raise ValueError(
            f"{name} channel mapping {actual.channels} != expected {expected.channels}"
        )
    channel_scores: list[float] = []
    for index in range(expected.channels):
        actual_channel = channel(actual, index)
        expected_channel = channel(expected, index)
        channel_score, _, _ = align(
            actual_channel, expected_channel, int(thresholds["max_lag_ms"])
        )
        if channel_score < thresholds["min_correlation"]:
            raise ValueError(
                f"{name} channel {index} correlation {channel_score:.4f} is too low"
            )
        for other_index in range(expected.channels):
            if other_index == index:
                continue
            cross_score, _, _ = align(
                actual_channel,
                channel(expected, other_index),
                int(thresholds["max_lag_ms"]),
            )
            if channel_score - cross_score < thresholds["min_isolation_margin"]:
                raise ValueError(f"{name} channel {index} mapping is swapped or ambiguous")
        channel_scores.append(channel_score)
    duration_error_ms = abs(actual.frames / actual.rate - expected.frames / expected.rate) * 1000
    score, lag_ms, overlap_ms = align(actual, expected, int(thresholds["max_lag_ms"]))
    wrong_score, _, _ = align(actual, other_expected, int(thresholds["max_lag_ms"]))
    peak, rms, clipping = signal_metrics(actual)
    expected_peak, expected_rms, _ = signal_metrics(expected)
    if duration_error_ms > thresholds["max_duration_error_ms"]:
        raise ValueError(f"{name} duration error {duration_error_ms:.1f} ms")
    if score < thresholds["min_correlation"]:
        raise ValueError(f"{name} waveform correlation {score:.4f} is too low")
    if score - wrong_score < thresholds["min_isolation_margin"]:
        raise ValueError(
            f"{name} isolation margin {score - wrong_score:.4f} indicates swap/crosstalk"
        )
    if abs(lag_ms) > thresholds["max_lag_ms"]:
        raise ValueError(f"{name} alignment lag {lag_ms} ms")
    if abs(peak - expected_peak) > thresholds["max_peak_error"]:
        raise ValueError(f"{name} peak error {abs(peak - expected_peak):.4f}")
    if abs(rms - expected_rms) > thresholds["max_rms_error"]:
        raise ValueError(f"{name} RMS error {abs(rms - expected_rms):.4f}")
    if clipping > thresholds["max_clipping_samples"]:
        raise ValueError(f"{name} has {clipping} clipped samples")
    return {
        "sample_rate": actual.rate,
        "channels": actual.channels,
        "frames": actual.frames,
        "duration_error_ms": duration_error_ms,
        "waveform_duration_drift_ppm": (
            actual.frames / actual.rate / (expected.frames / expected.rate) - 1.0
        )
        * 1_000_000,
        "alignment_lag_ms": lag_ms,
        "aligned_overlap_ms": overlap_ms,
        "correlation": score,
        "other_fixture_correlation": wrong_score,
        "isolation_margin": score - wrong_score,
        "peak_normalized": peak,
        "rms_normalized": rms,
        "peak_error_normalized": abs(peak - expected_peak),
        "rms_error_normalized": abs(rms - expected_rms),
        "clipping_samples": clipping,
        "channel_correlations": channel_scores,
    }


def source_timing_metrics(
    manifest: dict, source: str, sample_rate: int, deadline_ratio: float
) -> tuple[float, int]:
    blocks = sorted(
        (
            item
            for item in manifest.get("timeline_blocks", [])
            if item.get("source") == source and int(item.get("pcm_frame_count", 0)) > 0
        ),
        key=lambda item: int(item["sequence"]),
    )
    if len(blocks) < 2:
        raise ValueError(f"{source} has insufficient timeline blocks for drift measurement")
    first = blocks[0]
    last = blocks[-1]
    frame_span = (
        int(last["pcm_start_frame"])
        + int(last["pcm_frame_count"])
        - int(first["pcm_start_frame"])
    )
    source_elapsed_ns = (
        int(last["source_capture_start_ns"])
        - int(first["source_capture_start_ns"])
        + round(int(last["pcm_frame_count"]) * 1_000_000_000 / sample_rate)
    )
    if frame_span <= 0 or source_elapsed_ns <= 0:
        raise ValueError(f"{source} timeline is not monotonic")
    measured_rate = frame_span * 1_000_000_000 / source_elapsed_ns
    drift_ppm = (measured_rate / sample_rate - 1.0) * 1_000_000

    misses = 0
    previous = blocks[0]
    for current in blocks[1:]:
        if current.get("discontinuity_before"):
            previous = current
            continue
        expected_ns = int(previous["pcm_frame_count"]) * 1_000_000_000 / sample_rate
        arrival_ns = int(current["callback_arrival_ns"]) - int(previous["callback_arrival_ns"])
        if arrival_ns > expected_ns * deadline_ratio:
            misses += 1
        previous = current
    return drift_ppm, misses


def stem_paths(session: Path, audio: list[dict], prefix: str) -> list[Path]:
    return [
        session / item["path"]
        for item in audio
        if Path(item["path"]).name.startswith(prefix + "-")
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("session_dir", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--system-expected", required=True, type=Path)
    parser.add_argument("--mic-expected", required=True, type=Path)
    args = parser.parse_args()

    manifest = json.loads((args.session_dir / "session.json").read_text(encoding="utf-8"))
    if manifest["state"] != "completed":
        raise SystemExit(f"HIL session is not completed: {manifest['state']}")
    audio = manifest.get("audio_files", [])
    mic_paths = stem_paths(args.session_dir, audio, "mic")
    system_paths = stem_paths(args.session_dir, audio, "system")
    if not mic_paths or not system_paths:
        raise SystemExit("HIL session must contain microphone and system stems")

    thresholds: dict[str, float | int] = {
        "max_dropped_frames": int(os.environ.get("KOE_HIL_MAX_DROPPED_FRAMES", "0")),
        "max_drift_ppm_abs": int(os.environ.get("KOE_HIL_MAX_DRIFT_PPM", "1000")),
        "max_lag_ms": int(os.environ.get("KOE_HIL_MAX_LAG_MS", "1000")),
        "max_duration_error_ms": int(os.environ.get("KOE_HIL_MAX_DURATION_ERROR_MS", "250")),
        "min_correlation": float(os.environ.get("KOE_HIL_MIN_CORRELATION", "0.90")),
        "min_isolation_margin": float(os.environ.get("KOE_HIL_MIN_ISOLATION_MARGIN", "0.20")),
        "max_peak_error": float(os.environ.get("KOE_HIL_MAX_PEAK_ERROR", "0.15")),
        "max_rms_error": float(os.environ.get("KOE_HIL_MAX_RMS_ERROR", "0.10")),
        "max_clipping_samples": int(os.environ.get("KOE_HIL_MAX_CLIPPING_SAMPLES", "0")),
        "max_callback_deadline_misses": int(
            os.environ.get("KOE_HIL_MAX_CALLBACK_DEADLINE_MISSES", "0")
        ),
        "callback_deadline_ratio": float(
            os.environ.get("KOE_HIL_CALLBACK_DEADLINE_RATIO", "2.0")
        ),
    }
    dropped = int(manifest.get("overflow_count", 0))
    if dropped > thresholds["max_dropped_frames"]:
        raise SystemExit(f"HIL capture dropped {dropped} frames")
    try:
        mic_expected = read_pcm([args.mic_expected])
        system_expected = read_pcm([args.system_expected])
        mic_actual = read_pcm(mic_paths, mic_expected.frames)
        system_actual = read_pcm(system_paths, system_expected.frames)
        mic_drift, mic_misses = source_timing_metrics(
            manifest,
            "microphone",
            mic_actual.rate,
            float(thresholds["callback_deadline_ratio"]),
        )
        system_drift, system_misses = source_timing_metrics(
            manifest,
            "system",
            system_actual.rate,
            float(thresholds["callback_deadline_ratio"]),
        )
        mic_metrics = validate_stem(
            "microphone", mic_actual, mic_expected, system_expected, thresholds
        )
        system_metrics = validate_stem(
            "system", system_actual, system_expected, mic_expected, thresholds
        )
        waveform_drift = max(
            abs(float(mic_metrics["waveform_duration_drift_ppm"])),
            abs(float(system_metrics["waveform_duration_drift_ppm"])),
        )
        measured_drift = max(abs(mic_drift), abs(system_drift), waveform_drift)
        deadline_misses = mic_misses + system_misses
        mic_timeline_frames = max(
            (
                int(block["pcm_start_frame"]) + int(block["pcm_frame_count"])
                for block in manifest.get("timeline_blocks", [])
                if block.get("source") == "microphone"
            ),
            default=mic_actual.frames,
        )
        duration_hours = max(mic_timeline_frames / mic_actual.rate / 3600, 1 / 3600)
        if measured_drift > thresholds["max_drift_ppm_abs"]:
            raise ValueError(f"measured clock drift {measured_drift:.1f} ppm exceeds threshold")
        if deadline_misses > thresholds["max_callback_deadline_misses"]:
            raise ValueError(f"capture had {deadline_misses} callback deadline misses")
        metrics = {
            "schema_version": 3,
            "session_id": manifest["session_id"],
            "callback_deadline_misses": deadline_misses,
            "callback_deadline_misses_per_hour": deadline_misses / duration_hours,
            "dropped_frames": dropped,
            "clock_drift_ppm_max_abs": measured_drift,
            "clock_drift_ppm": {
                "microphone_timeline": mic_drift,
                "system_timeline": system_drift,
                "waveform_duration_max_abs": waveform_drift,
            },
            "correction_discontinuities": len(manifest.get("discontinuities", [])),
            "microphone": mic_metrics,
            "system": system_metrics,
            "thresholds": thresholds,
        }
    except (ValueError, wave.Error) as error:
        raise SystemExit(f"HIL audio validation failed: {error}") from error

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(metrics, indent=2) + "\n", encoding="utf-8")
    print("HIL microphone/system waveform, timing, mapping, and isolation passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
