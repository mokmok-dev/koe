# koe-capture

Cross-platform microphone and system audio capture for [koe](../..).

The public crate is a thin facade. The typed core is `no_std` with `alloc`,
and each operating system has its own backend crate. `[dependencies]` holds
only the workspace path crates: no `cpal`, no COM binding crate, no async
runtime, and no helper process.

## Crates

| Crate | std | Role |
| --- | --- | --- |
| `koe-capture-core` | `no_std + alloc` | types, device selection, sample decoding, frame queue, `CaptureStream` |
| `koe-capture-linux` | std | `PulseAudio` backend |
| `koe-capture-macos` | std | `CoreAudio` backend |
| `koe-capture-windows` | std | WASAPI backend |
| `koe-capture` | std | feature-selected facade and public API |

## Platforms

| Target | Feature | System audio |
| --- | --- | --- |
| Linux | `linux` | monitor sources of the default sink |
| macOS | `macos` | `CoreAudio` process tap pinned to the selected output device, wrapped in a private aggregate device |
| Windows | `windows` | shared-mode loopback on the render endpoint |

Enable exactly one platform feature. A build with no platform feature keeps
the full API and returns `Error::Unsupported` from each capture call.

The macOS system-audio entry is stereo, because the tap is a stereo mix. The
backend pins the tap to the selected output device, so each `system:` entry
records the audio played to that device. `read` blocks, so capture the
microphone and system audio on separate threads, each opening its own stream.

## Usage

```rust
use koe_capture::{Source, StreamSpec};

# fn main() -> Result<(), koe_capture::Error> {
for device in koe_capture::devices()? {
    println!("{}: {} ({:?})", device.id(), device.name(), device.source());
}

let device = koe_capture::default_device(Source::SystemAudio)?;
let spec = StreamSpec::new(
    Source::SystemAudio,
    device.default_sample_rate(),
    device.channels(),
)?;
let mut capture = koe_capture::open_device(&device, &spec)?;
let mut samples = vec![0.0_f32; usize::from(spec.channels()) * 480];
loop {
    let frames = capture.read(&mut samples)?;
    println!("{frames} frames");
}
# }
```

`read` returns interleaved `f32` samples in `-1.0..=1.0`, as frames written to
your buffer. The buffer length must be a multiple of the channel count.

## Build prerequisites

- Linux: `libpulseaudio` development files. The flake's dev shell provides it.
- macOS: macOS 14.2 or later for system audio; the microphone works on
  earlier versions. The app bundle needs
  `NSMicrophoneUsageDescription` for the microphone and
  `NSAudioCaptureUsageDescription` for the tap, plus the Microphone and
  Screen & System Audio Recording permissions. A bundle signed with the
  hardened runtime additionally needs the
  `com.apple.security.device.audio-input` entitlement; without it macOS
  refuses audio input before it shows any permission prompt, so every
  `AudioUnitRender` returns `-10863` even though the audio unit started and
  the microphone is granted. A bare `cargo run` binary has no bundle, so TCC
  cannot grant it and capture stays silent.
- Windows: Windows 10 1703 or later for event-driven loopback. The requested
  `StreamSpec` must match the endpoint's native mix format; use the selected
  `DeviceInfo`'s `default_sample_rate()` and `channels()` with `open_device`.

## Verification

```bash
# Core, property tests, no hardware or audio server needed.
cargo test -p koe-capture-core

# Facade and the host backend.
cargo test -p koe-capture --features linux

# Every Linux gate: clippy, tests, macOS check, Windows check.
# `cargo test` also runs the doctests.
nix flake check

# Mutation testing: the whole workspace, or only the lines a branch changed.
# CI also excludes koe-transcribe-ggml and koe-record, whose rebuilds dominate.
cargo mutants --iterate
cargo mutants --iterate --in-diff <(git diff origin/main)

# Inspect the default microphone and system-audio devices on the host.
cargo run -p koe-capture --features linux --example probe
```

Mutation testing runs in the `mutants` workflow. Native macOS and Windows CI
jobs run clippy and the test suite against their backends. They compile and
link, but they do not open a real capture device.
