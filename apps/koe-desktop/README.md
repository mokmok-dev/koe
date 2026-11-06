# koe desktop

The koe desktop app records the microphone and the system audio from a menu bar icon. A Rune overlay shows the timer and the level of each source while a take runs. The menu bar item is a `tray-icon` status item.

## Run

The default build is the Rune shell.

```console
cargo run -p koe-desktop --features linux
cargo run -p koe-desktop --features macos
cargo run -p koe-desktop --features windows
```

The previous egui shell remains available for comparison.

```console
cargo run -p koe-desktop --no-default-features --features egui,linux
```

The first window is the control surface. Use **Hide to menu bar** to keep only the tray icon. On macOS, the process runs as an accessory app, so it has no Dock icon. In the Rune shell, `KOE_HUD=off` hides the overlay if a compositor places it badly.

The tray icon needs a StatusNotifier host on Linux. KDE, Sway, and most bars provide one. GNOME needs an AppIndicator extension. Without a host, the window is the only control. Hiding the window leaves the app running with no visible control, so relaunch it from the same binary.

## Why Rune with tray-icon

The app needs two OS surfaces. One is a menu bar item. The other is a transparent, always-on-top overlay. Rune gives the overlay first-class support and owns one winit event loop. `tray-icon` gives the menu bar item on macOS, Windows, and Linux. That split keeps one UI stack and one event loop.

| Part | Owner |
| --- | --- |
| Main window | Rune `AppSpec` and `WindowSpec` |
| Overlay HUD | Rune `OverlaySpec` |
| Menu bar item | `tray-icon` |
| Event loop | Rune on winit |
| Recording | `koe-capture` and `koe-record` |
| Recording state | `Phase` in `src/state.rs` |

The alternatives fail in specific ways.

- **egui base with a Rune HUD.** eframe and Rune each create a winit `EventLoop`, and winit rejects the second with `RecreationAttempt`. Rune's overlay is a host on Rune's `App`, not a reusable widget. A separate HUD process is possible but adds IPC, a second GPU device, and a second lifecycle for a 340×88 window.
- **Rune's own tray on macOS.** `crates/rune/src/tray.rs` supports Linux and Windows. macOS needs app code. Hex, another Anomaly app, writes its own `NSStatusItem` with `objc2-app-kit`, 999 lines with 21 unsafe blocks. koe's workspace denies unsafe code, so `tray-icon` is the safer bridge.
- **GPUI.** It has no tray or status item API and would need a second event loop integration.
- **wgpu alone.** It is a renderer, not a UI framework.

Rune is pre-release and pinned to a git revision. The Rune window, the real Rune HUD module, and the failure path with the tray object were rendered on Linux under Xvfb with Mesa's `lavapipe`. The macOS and Windows builds were not built here.

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
| `src/app.rs` | egui fallback app |
| `src/ui.rs` | egui fallback window |
| `src/hud.rs` | egui fallback HUD |
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
cargo test -p koe-desktop --no-default-features --features egui,linux
```

The tests cover the state transitions, the tray labels and menu ids, time formatting, path selection, plan construction, and the HUD text. The Rune window and the tray-backed failure path were rendered in the app. The Rune HUD module was rendered in a throwaway overlay because real capture is blocked on this host.

Real capture was not verified on the development host. Against a PulseAudio 17 null sink, the app crashes when a source opens, and the existing `cargo run -p koe-cli --features linux -- record` command aborts with `Assertion '!e->dead' failed` against the same sink. The fault is in the shared capture backend rather than the desktop shell. The tray icon's actual display was not verified because the host has no StatusNotifier watcher; the labels and menu ids are tested.
