# koe-capture

Cross-platform microphone and system audio capture for [koe](../..).

The crate talks to the operating system's audio stack through its system
library. `[dependencies]` is empty: no `cpal`, no COM binding crate, no async
runtime, no helper process.

## Platforms

| Target | Feature | Backend | System audio |
| --- | --- | --- | --- |
| Linux | `linux` | `PulseAudio` (`libpulse`, `libpulse-simple`) | monitor sources of the default sink |
| macOS | `macos` | CoreAudio / AudioToolbox | CoreAudio process tap on the default output, wrapped in a private aggregate device |
| Windows | `windows` | WASAPI | shared-mode loopback on the render endpoint |

A build with no platform feature keeps the typed core and every property test,
and returns `Error::Unsupported` from each capture call. That is the
configuration used for mutation testing.

The features are additive. Modules are also gated on `target_os`, so
`--all-features` builds only the backend of the target in use.

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
cargo test -p koe-capture

# Linux backend: enumerate and capture from the running server.
cargo test -p koe-capture --features linux

# Every gate: clippy, tests, mutation score, macOS check, Windows check.
nix flake check

# Only the mutation gate.
nix build .#checks.x86_64-linux.mutation
```

The mutation gate and the two cross-target checks are defined for
`x86_64-linux`. The macOS and ARM jobs run clippy and the test suite on their
native targets. CI also runs the Windows adapter on a Windows runner.

The OS adapters live under `src/sys/` and are excluded from mutation testing:
their calls only produce a result against a live audio server, so a surviving
mutant there is unobservable, not untested. Every decision they could get wrong
(device ranking, format validation, sample decoding, frame and queue
arithmetic) lives in the safe core and stays in scope.
