# koe

A privacy-first recording & transcription app.

## Features

- Offline-first
- On-device transcription
- System audio + microphone capture
- Global-hotkey dictation: speak, and the text is typed into the focused app
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
has a recorder screen and a settings screen that holds the source, format,
folder, and dictation controls. The tray icon and the overlay
show the state while the window is hidden. See
[`apps/koe-desktop/README.md`](apps/koe-desktop/README.md) for the framework
choice and the platform limits.

While koe sits in the background, a configurable global hotkey (the default is
`Ctrl+Shift+Space`) starts dictation from the microphone; press it again and
the speech is transcribed on-device and inserted into whatever app holds
keyboard focus, Japanese included. A misfire is correctable: **Cancel
dictation** in the menu bar discards without typing. On Linux the hotkey
needs X11 — without an X server koe refuses to arm it, and under Wayland it
fires only while an X11/XWayland window has focus — and the text is pasted
through the clipboard (with the previous clipboard restored), so a running
input method cannot mangle it; a GGUF model file is required. macOS and
Windows type Unicode key events; macOS uses the system speech model and
needs the accessibility permission. The binding, model, and language persist
in a settings file and are edited in the window.

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
