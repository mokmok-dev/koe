# Release HIL matrix

Hardware-in-the-loop testing (spec/07) uses physical or virtual test machines;
hosted CI runners cannot provide real microphones, loopback, permission
denial, hot-plug or sleep/resume. The automated suite is
`.github/workflows/release-hil.yaml` + `scripts/hil/*`; the manual matrix
below is the Milestone 7 release gate.

## Test machines

| Label | OS | Capabilities exercised |
| --- | --- | --- |
| `koe-hil-macos` / `koe-hil-macos-intel` | macOS 14.6+ (Apple Silicon / Intel) | CoreAudio mic, 14.6+ process tap, TCC deny/revoke |
| `koe-hil-windows` | Windows 11 x64 | WASAPI mic, loopback, MSIX install/uninstall, capability deny |
| `koe-hil-linux` / `koe-hil-linux-arm64` | Linux desktop x86_64 / arm64 (PipeWire) | PipeWire source/sink, signed-inventory AppImage install/uninstall |

Each machine sets distinct, at-least-one-hour `KOE_HIL_MIC_TEST_WAV` and
`KOE_HIL_SYSTEM_TEST_WAV` 48 kHz PCM16 speech fixtures matching the requested
capture format. Release scripts reject a
capture duration below 3600 seconds. `KOE_HIL_MIC_PLAYER` routes
only the microphone fixture while the platform player routes the system
fixture. `KOE_HIL_MODEL_SELECTOR` names the release model and
`KOE_HIL_OFFLINE_RUNNER` executes the recording command behind an OS firewall;
the suite requires a final transcript and then unloads/removes the model.
`KOE_HIL_PLATFORM_GATES` is a machine-owned executable that must test permission
deny/revoke, hot-plug/default switch, sleep/resume, and forced-crash recovery
against the downloaded package and fail nonzero on any regression. The oracle aligns both captures and checks correlation, duration, sample rate,
channel mapping, peak/RMS, clipping, drift, and cross-fixture isolation.
Operator audio is never used as a test artifact.

## Matrix (release gate)

| Case | macOS | Windows | Linux |
| --- | --- | --- | --- |
| microphone capture | ✅ automated | ✅ automated | ✅ automated |
| system audio capture | ✅ 14.6+ tap | ✅ loopback | ✅ PipeWire sink |
| permission deny/revoke | ✅ machine platform gate | ✅ machine platform gate | ✅ PipeWire policy gate |
| device hot-plug | ✅ machine platform gate | ✅ machine platform gate | ✅ machine platform gate |
| sleep/resume | ✅ machine platform gate | ✅ machine platform gate | ✅ machine platform gate |
| default device switch | ✅ machine platform gate | ✅ machine platform gate | ✅ machine platform gate |
| clock drift (mic + system) | automated dual capture + release soak | automated dual capture + release soak | automated dual capture + release soak |
| crash recovery | ✅ machine platform gate | ✅ machine platform gate | ✅ machine platform gate |
| clean machine install/uninstall | notarized `.app` extract/launch/remove | MSIX `Add/Remove-AppxPackage` | AppImage copy/launch/remove |

## Recording results

For each release, record in this table (release PR):

| Release | Machine | Date | Mic result | System result | Drift ppm | Install | Uninstall |
| --- | --- | --- | --- | --- | --- | --- | --- |
| vX.Y.Z | koe-hil-macos |  | pass/fail | pass/fail |  | pass/fail | pass/fail |
| vX.Y.Z | koe-hil-macos-intel |  | pass/fail | pass/fail |  | pass/fail | pass/fail |
| vX.Y.Z | koe-hil-windows |  | pass/fail | pass/fail |  | pass/fail | pass/fail |
| vX.Y.Z | koe-hil-linux |  | pass/fail | pass/fail |  | pass/fail | pass/fail |
| vX.Y.Z | koe-hil-linux-arm64 |  | pass/fail | pass/fail |  | pass/fail | pass/fail |

## Metrics

The automated suite emits `target/hil/metrics.json`:

- callback deadline misses / hour
- dropped frames / hour
- clock drift ppm and correction discontinuity
- aligned waveform correlation and lag for both mic/system fixtures
- duration/sample count, sample rate, channel mapping, peak/RMS error
- clipping count and mic/system isolation margin

Callback deadline misses are measured from consecutive callback-arrival and
block-duration timestamps. Drift is independently calculated both from PCM frame
progress against source-capture timestamps and from captured-versus-fixture
waveform duration; the oracle does not trust the optional correction summary. Dropped frames and callback misses remain hard
zero-tolerance gates. Exact sample rate and every channel mapping are checked,
and the checked threshold values are embedded in each metrics artifact.

The packaged update gate also applies the release, launches it, rolls back to
the originally running executable, launches that fallback, and rejects signed
expired metadata, replay, and a tampered artifact.

Thresholds are decided from the Milestone 1 baseline and recorded in the
release notes; the HIL gate fails when a measured metric regresses more than
the documented tolerance.
