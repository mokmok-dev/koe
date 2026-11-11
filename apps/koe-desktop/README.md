# koe desktop

The koe desktop app records the microphone and the system audio from a menu bar icon. An overlay HUD shows the timer and the level of each source while a take runs. The menu bar item is a `tray-icon` status item.

## Run

Build with the host feature. The platform feature is required: a build with none of `linux`, `macos`, and `windows` starts, but `koe-capture` returns `Error::Unsupported` from every capture call, so a take fails as soon as it opens its sources.

```console
cargo run -p koe-desktop --features linux
cargo run -p koe-desktop --features macos
cargo run -p koe-desktop --features windows
```

On macOS the app starts in the menu bar with no window. Choose **Open koe** in the menu to show the control window. On Linux and Windows the window opens at launch. The menu opens on a left click, and **Open koe** shows the window again. Use **Hide to menu bar** to keep only the tray icon. Closing the window keeps the process alive while the tray icon exists; **Quit** exits. The process runs as an accessory app on macOS, so it has no Dock icon. Set `KOE_TRAY_DEBUG=1` to print tray events while diagnosing a menu that does not act. Set `KOE_HUD=off` to disable the overlay window if a compositor places it badly.

The tray icon needs a StatusNotifier host on Linux. KDE, Sway, and most bars provide one. GNOME needs an AppIndicator extension. Without a host, the window is the only control. Hiding the window leaves the app running with no visible control, so relaunch it from the same binary.

## Why GPUI with tray-icon

The app needs two OS surfaces. One is a menu bar item. The other is a transparent, always-on-top overlay. GPUI gives the overlay first-class support and owns one event loop. `tray-icon` gives the menu bar item on macOS, Windows, and Linux.

The window is built from gpui-kit components: `TitleBar`, `Button`, `Checkbox`, `Alert`, `GroupBox`, and `Progress`. The HUD is painted directly so every pixel comes from the take state.

The alternatives fail in specific ways.

- **A hand-written tray on macOS.** Writing the `NSStatusItem` by hand means `objc2-app-kit`: Hex, another Anomaly app, does that in 999 lines with 21 unsafe blocks. koe's workspace denies unsafe code, so `tray-icon` is the safer bridge.
- **wgpu alone.** It is a renderer, not a UI framework.

GPUI is pinned by gpui-kit to `gpui-pre =0.3.11`. Its click-through API (`Window::set_input_region`) is Wayland-only, so on X11 and macOS the non-interactive HUD still receives pointer events in its 340×88 area. On Wayland the HUD uses an empty input region.

## What it does

The app writes one Ogg Opus or WAV file per take to the recordings folder. The tray icon and tooltip show the state and the elapsed time. The HUD shows the timer, one live dot per source, a level bar per source, and a warning when a source drops. The window shows the state, the sources, the format, the output folder, and the last take.

The app records audio only. Transcription is the next step. `src/state.rs` models the audio lifecycle, so `koe-session` can take over when transcript tracking lands.

## Layout

| File | Role |
| --- | --- |
| `src/main.rs` | Process entry |
| `src/app.rs` | GPUI app, tick loop, window lifecycle, tray sync, HUD updates |
| `src/ui.rs` | GPUI main window built from gpui-kit components |
| `src/overlay.rs` | GPUI overlay HUD |
| `src/hud.rs` | HUD state and text shared by the window and the overlay |
| `src/state.rs` | Recording state machine |
| `src/engine.rs` | Recorder thread and capture loops |
| `src/tray.rs` | `tray-icon` icon and menu |
| `src/icon.rs` | Code-drawn tray icon |
| `src/settings.rs` | Source, format, and output path |
| `src/error.rs` | Typed failures and their messages |

## Verify

```console
cargo test -p koe-desktop --features linux
cargo clippy -p koe-desktop --all-targets --features linux -- --deny warnings
```

The tests cover the state transitions, the tray labels and menu ids, time formatting, path selection, plan construction, and the HUD text. The GPUI window was exercised under Xvfb: the start path reached its failure state, source and format selection updated, dismiss, hide, close-to-tray, and quit behaved. The GPUI HUD was rendered in a throwaway preview because real capture is blocked on this host.

Real capture was not verified on the development host. Against a PulseAudio 17 null sink, the app crashes when a source opens, and the existing `cargo run -p koe-cli --features linux -- record` command aborts with `Assertion '!e->dead' failed` against the same sink. The fault is in the shared capture backend rather than the desktop shell. The tray icon's actual display was not verified because the host has no StatusNotifier watcher; the labels and menu ids are tested.
