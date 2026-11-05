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
| macOS | `macos` | `CoreAudio` process tap on the default output, wrapped in a private aggregate device |
| Windows | `windows` | shared-mode loopback on the render endpoint |

Enable exactly one platform feature. A build with no platform feature keeps
the full API and returns `Error::Unsupported` from each capture call.

## Usage

```rust
use koe_capture::{Source, StreamSpec};

# fn main() -> Result<(), koe_capture::Error> {
let spec = StreamSpec::new(Source::SystemAudio, 48_000, 2)?;
for device in koe_capture::devices()? {
    println!("{}: {} ({:?})", device.id(), device.name(), device.source());
}

let mut capture = koe_capture::open(&spec)?;
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
- macOS: nothing beyond the system frameworks.
- Windows: nothing beyond `ole32` and `kernel32`.

## Verification

```bash
# Core, property tests, no hardware or audio server needed.
cargo test -p koe-capture-core

# Facade and the host backend.
cargo test -p koe-capture --features linux

# Every gate: clippy, tests, mutation score, macOS check, Windows check.
nix flake check

# Only the mutation gate.
nix build .#checks.x86_64-linux.mutation
```

The mutation gate targets `koe-capture-core`. The OS crates are separate
crates, so no mutation exclusion is needed. The macOS and ARM jobs run clippy
and the test suite on their native targets. CI also runs the Windows adapter
on a Windows runner.
