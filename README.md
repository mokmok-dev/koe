# koe

A privacy-first recording & transcription app.

## Features

- Offline-first
- On-device transcription
- System audio + microphone capture
- Single-file Ogg Opus or WAV recording
- Simple local storage
- Featherlight UI
- Cross platform

## Desktop app

The desktop app shows the recording state in the menu bar and an overlay HUD
while a take runs. GPUI owns the window and the overlay, and `tray-icon` owns
the menu bar item. Build it with the host feature:

```console
cargo run -p koe-desktop --features linux
```

Use `--features macos` or `--features windows` on those platforms. The window
holds the source, format, and folder controls. The tray icon and the overlay
show the state while the window is hidden. See
[`apps/koe-desktop/README.md`](apps/koe-desktop/README.md) for the framework
choice and the platform limits.

## Command line

The `koe` CLI records the microphone and the system audio into one file and
transcribes 16-bit PCM WAV files offline. Build it with the backend for the
host:

```console
cargo run -p koe-cli --features linux -- devices
cargo run -p koe-cli --features linux -- record take.ogg --source both --model model.gguf
cargo run -p koe-cli --features linux -- transcribe take.wav --model model.gguf --language ja
```

Use `--features macos` or `--features windows` on those platforms. `record`
writes Ogg Opus (`.ogg`, `.oga`, `.opus`) or WAV (`.wav`) and stops after
`--duration` or on Ctrl-C. It also writes a transcript beside the audio, by
default next to the output with a `.txt` extension. The transcript file is
created when recording starts and completed when it stops. If one source
delivers no audio, `record` keeps the other source recording and warns. A
transcription fault prints a warning and keeps the audio and the committed
text. Pass `--model` for GGUF transcription, `--transcript` to choose the
path, `--timestamps` to add time ranges, or `--no-transcribe` to record only.
`transcribe` reads 16-bit PCM WAV and prints committed text, keeping the text
it received when the engine fails partway. On macOS, omit `--model` to use the
system speech model.
