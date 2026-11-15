# koe

A privacy-first recording & transcription app.

## Features

- Offline-first
- On-device transcription
- System audio + microphone capture
- Global-hotkey dictation: speak, and the text is typed into the focused app
- One session directory per meeting: Ogg Opus or WAV audio with its transcript
- Simple local storage
- Featherlight UI
- Cross platform

## Install

Tagging a commit `v<version>` builds every platform's assets in GitHub Actions
and attaches them to the release. The builds come from the flake's `packages`,
so the release job downloads them from the Nix cache instead of compiling.

The macOS release is a DMG holding `Koe.app`. It is ad-hoc signed and not
notarized, so the first launch needs **Open** from the control-click menu, or
`xattr -d com.apple.quarantine /Applications/Koe.app` before opening it.

The Linux tarball holds `koe-desktop` and this README. Its dynamic libraries come
from `/nix/store`, so the binary runs where that store is mounted. On NixOS or
with Nix installed, install from the flake instead:

```console
nix profile install github:mokmok-dev/koe
```

## Desktop app

The desktop app shows the recording state in the menu bar and an overlay HUD
while a take or a dictation runs. While a take records, the HUD is a small card
in the top-right corner of the primary display with the elapsed time and one
Pause/Resume button; a pause keeps the file open, and the paused time is left
out of the timer and out of the saved file. GPUI owns the window and the
overlay, and
`tray-icon` owns the menu bar item. Build it with the host feature:

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
keyboard focus, Japanese included. The overlay HUD shows the words as they are
recognized while it captures: the macOS system model and a streaming GGUF
family report them as they go, while a batch model such as Whisper reports at
the stop. A misfire is correctable: **Cancel
dictation** in the menu bar discards without typing. On Linux the hotkey
needs X11 — without an X server koe refuses to arm it, and under Wayland it
fires only while an X11/XWayland window has focus — and the text is pasted
through the clipboard (with the previous clipboard restored), so a running
input method cannot mangle it; a GGUF model file is required. macOS and
Windows type Unicode key events; macOS uses the system speech model and
needs the accessibility permission. The binding, model, and language persist
in a settings file and are edited in the window.
