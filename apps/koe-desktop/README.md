# koe desktop

The koe desktop app records the microphone and the system audio from a menu bar icon. An overlay HUD shows the timer and the level of each source while a take runs. The menu bar item is a `tray-icon` status item.

## Run

The default build is the Rune shell.

```console
cargo run -p koe-desktop --features linux
cargo run -p koe-desktop --features macos
cargo run -p koe-desktop --features windows
```

The GPUI shell is available through the `gpui` feature.

```console
cargo run -p koe-desktop --no-default-features --features gpui,linux
```

On macOS the app starts in the menu bar with no window. Choose **Open koe** in the menu to show the control window. On Linux and Windows the window opens at launch. The menu opens on a left click, and **Open koe** shows the window again. Use **Hide to menu bar** to keep only the tray icon. Closing the window keeps the process alive while the tray icon exists; **Quit** exits. The process runs as an accessory app on macOS, so it has no Dock icon. Set `KOE_TRAY_DEBUG=1` to print tray events while diagnosing a menu that does not act. Set `KOE_HUD=off` to disable the overlay window if a compositor places it badly.

The tray icon needs a StatusNotifier host on Linux. KDE, Sway, and most bars provide one. GNOME needs an AppIndicator extension. Without a host, the window is the only control. Hiding the window leaves the app running with no visible control, so relaunch it from the same binary.

## Why Rune with tray-icon

The app needs two OS surfaces. One is a menu bar item. The other is a transparent, always-on-top overlay. Rune gives the overlay first-class support and owns one winit event loop. `tray-icon` gives the menu bar item on macOS, Windows, and Linux. That split keeps one UI stack and one event loop.

| Part | Rune shell | GPUI shell |
| --- | --- | --- |
| Main window | Rune `AppSpec` and `WindowSpec` | `gpui_kit::open_window` |
| Overlay HUD | Rune `OverlaySpec` | `WindowKind::PopUp` window |
| Menu bar item | `tray-icon` | `tray-icon` |
| Event loop | Rune on winit | GPUI |
| Recording | `koe-capture` and `koe-record` | `koe-capture` and `koe-record` |
| Recording state | `Phase` in `src/state.rs` | `Phase` in `src/state.rs` |

The GPUI shell builds its window from gpui-kit components: `TitleBar`, `Button`, `Checkbox`, `Alert`, `GroupBox`, and `Progress`. The HUD is painted directly so every pixel comes from the take state.

The alternatives fail in specific ways.

- **egui base with a Rune HUD.** eframe and Rune each create a winit `EventLoop`, and winit rejects the second with `RecreationAttempt`. Rune's overlay is a host on Rune's `App`, not a reusable widget. A separate HUD process is possible but adds IPC, a second GPU device, and a second lifecycle for a 340×88 window.
- **Rune's own tray on macOS.** `crates/rune/src/tray.rs` supports Linux and Windows. macOS needs app code. Hex, another Anomaly app, writes its own `NSStatusItem` with `objc2-app-kit`, 999 lines with 21 unsafe blocks. koe's workspace denies unsafe code, so `tray-icon` is the safer bridge.
- **wgpu alone.** It is a renderer, not a UI framework.

Rune is pre-release and pinned to a git revision. The Rune window, the real Rune HUD module, and the failure path with the tray object were rendered on Linux under Xvfb with Mesa's `lavapipe`. The macOS and Windows builds were not built here.

GPUI is pinned by gpui-kit to `gpui-pre =0.3.11`. Its click-through API (`Window::set_input_region`) is Wayland-only, so on X11 and macOS the non-interactive HUD still receives pointer events in its 340×88 area. On Wayland the HUD uses an empty input region.

## What it does

The app writes one Ogg Opus or WAV file per take to the recordings folder. The tray icon and tooltip show the state and the elapsed time. The HUD shows the timer, one live dot per source, a level bar per source, and a warning when a source drops. The window shows the state, the sources, the format, the output folder, and the last take.

The app records audio only. Transcription is the next step. `src/state.rs` models the audio lifecycle, so `koe-session` can take over when transcript tracking lands.

## Layout

| File | Role |
| --- | --- |
| `src/main.rs` | Process entry and shell selection |
| `src/rune_app.rs` | Rune app, event pump, tray sync, overlay updates |
| `src/rune_ui.rs` | Rune main window |
| `src/rune_hud.rs` | Rune overlay HUD |
| `src/gpui_app.rs` | GPUI app, tick loop, window lifecycle, tray sync, HUD updates |
| `src/gpui_ui.rs` | GPUI main window built from gpui-kit components |
| `src/gpui_hud.rs` | GPUI overlay HUD |
| `src/hud.rs` | HUD state and text shared by both shells |
| `src/state.rs` | Recording state machine |
| `src/engine.rs` | Recorder thread and capture loops |
| `src/tray.rs` | `tray-icon` icon and menu |
| `src/icon.rs` | Code-drawn tray icon |
| `src/repaint.rs` | UI wake-up callback shared by both shells |
| `src/settings.rs` | Source, format, and output path |
| `src/error.rs` | Typed failures and their messages |

## Verify

```console
cargo test -p koe-desktop --features linux
cargo clippy -p koe-desktop --all-targets --features linux -- --deny warnings
cargo test -p koe-desktop --no-default-features --features gpui,linux
cargo clippy -p koe-desktop --all-targets --no-default-features --features gpui,linux -- --deny warnings
```

The tests cover the state transitions, the tray labels and menu ids, time formatting, path selection, plan construction, and the HUD text. The Rune window and the tray-backed failure path were rendered in the app. The GPUI window was exercised under Xvfb: the start path reached its failure state, source and format selection updated, dismiss, hide, close-to-tray, and quit behaved. The GPUI HUD was rendered in a throwaway preview because real capture is blocked on this host.

Real capture was not verified on the development host. Against a PulseAudio 17 null sink, the app crashes when a source opens, and the existing `cargo run -p koe-cli --features linux -- record` command aborts with `Assertion '!e->dead' failed` against the same sink. The fault is in the shared capture backend rather than the desktop shell. The tray icon's actual display was not verified because the host has no StatusNotifier watcher; the labels and menu ids are tested.
