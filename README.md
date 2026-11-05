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

## Command line

The `koe` CLI records the microphone and the system audio into one file and
transcribes 16-bit PCM WAV files offline. Build it with the backend for the
host:

```console
cargo run -p koe-cli --features linux -- devices
cargo run -p koe-cli --features linux -- record take.ogg --source both
cargo run -p koe-cli --features linux -- transcribe take.wav --model model.gguf --language ja
```

Use `--features macos` or `--features windows` on those platforms. `record`
writes Ogg Opus (`.ogg`, `.oga`, `.opus`) or WAV (`.wav`) and stops after
`--duration` or on Ctrl-C. `transcribe` reads 16-bit PCM WAV and prints
committed text, keeping the text it received when the engine fails partway.
On macOS, omit `--model` to use the system speech model.
