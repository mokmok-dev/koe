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

On macOS the app starts in the menu bar with no window. Choose **Open koe** in the menu to show the control window, or **Settings…** to show it on the settings screen. On Linux and Windows the window opens at launch. The menu opens on a left click, and **Open koe** shows the window again. Use **Hide to menu bar** to keep only the tray icon. Closing the window keeps the process alive while the tray icon exists; **Quit** exits. The process runs as an accessory app on macOS, so it has no Dock icon. Set `KOE_TRAY_DEBUG=1` to print tray events while diagnosing a menu that does not act. Set `KOE_HUD=off` to disable the overlay window if a compositor places it badly.

The tray icon needs a StatusNotifier host on Linux. KDE, Sway, and most bars provide one. GNOME needs an AppIndicator extension. Without a host, the window is the only control. Hiding the window leaves the app running with no visible control, so relaunch it from the same binary.

## Dictation

Press the dictation hotkey anywhere in the OS — with koe hidden to the menu bar and another app focused — and koe starts capturing the microphone. Press it again and the speech is transcribed on-device and inserted into the focused app, as if it came from the keyboard. A misfire is correctable: **Cancel dictation** in the menu bar discards the take and types nothing. The tray icon turns purple while a dictation runs, and the window shows the typed text or the failure.

The interaction is a toggle, deliberately: press to start, press again to stop and insert. hex, the organization's other dictation app, defaults to hold-to-talk with a 450 ms pre-roll; koe opens the microphone at the press instead, so speech in the first fraction of a second can be missed. A pre-roll means an always-open microphone, which a meeting recorder will not do — the microphone is a shared resource with the meeting flow, and the mutual exclusion between dictation and a take is a tested invariant. The stop commits; the cancel discards.

The binding, the model, and the language live in the window's Dictation section — on the settings screen, reached from the header button or the menu bar's **Settings…** item — and persist in a settings file (`~/.config/koe/settings.conf` on Linux, `~/Library/Application Support/koe/settings.conf` on macOS, `%APPDATA%\koe\settings.conf` on Windows), written atomically like a session record. The default binding is `Ctrl+Shift+Space`, chosen to be free of OS-level shortcuts on Windows, macOS, and the common Linux desktops; a binding without a modifier is rejected, so a bare `Space` can never swallow typing. The hotkey field commits on Enter — typing toward a binding passes through real shortcuts like `Ctrl+S`, and a live rebind would grab every one of them — and an Enter in the field also retries a registration that failed. The GPUI shell reads the same file but has no settings screen; **Settings…** opens its window, and edits happen in the Rune shell.

Dictation and a meeting take share one microphone, so each refuses while the other is active: the hotkey reports that a take owns the microphone, and Start reports that a dictation owns it. The rule lives in the pure state machines (`src/dictation.rs`) and is unit-tested.

Platform limits, honestly:

- **Linux.** `global-hotkey` registers shortcuts on X11 only, and its Linux manager cannot report its own failed registration — it returns success even when its X connection dies — so koe probes the X connection before arming the binding: without an X server (a Wayland session without XWayland, or no display) the app reports "No global hotkey" and dictation stays off. With XWayland the grab only sees X11 and XWayland windows: while a native Wayland window holds focus, the hotkey does not fire. Text insertion pastes through the clipboard — set the clipboard, send Ctrl+V, restore what was there half a second later — because a synthetic keysym is indistinguishable from a physical key and a running input method (fcitx, ibus — the norm on a Japanese desktop) would filter it; a paste inserts without crossing the input method. The paste snapshots what was there first — text and images, each up to hex's 32 MB cap; a bigger clipboard is left untouched and the text is typed as Unicode instead, which does not use an input method but cannot bypass one that is running. Only plain text and images come back: a rich-text clipboard (a browser's, a mail client's) loses its formatting, and a clipboard holding anything else — a file manager's file list, application-specific formats — is replaced by the transcript, because arboard exposes no way to enumerate and restore the other targets. An X selection only outlives its owner through a clipboard manager, so koe holds one clipboard handle for its whole run and the restored clipboard lives as long as koe does; on a desktop without a manager (dwm, i3, XMonad — most tiling setups) whatever koe last put there dies with koe. Terminals treat Ctrl+V as a quoted-insert literal: dictating into one inserts nothing, latches the terminal's literal-next state so the next control key lands as text, and looks like success — the shortcut sends, the terminal reads nothing from the clipboard, and the restore puts the previous clipboard back, so the transcript is discarded. koe has no per-app chord preference (hex's TerminalPaste flavour is out of scope); dictate into a text field. Both insertion paths reach X11 and XWayland windows only. The model must be a GGUF file (the same `--model` family as `koe-cli`); without one the hotkey refuses and the window says so.
- **macOS.** The system speech model is used when no GGUF path is set, like `koe-cli` without `--model`. Text is typed as Unicode key events, which needs the accessibility permission; whether a running input method filters those events is unverified, as are these paths generally — they were not built here.
- **Windows.** The GGUF model is required, and `SendInput` types the text as Unicode key events. Not verified here.

While a dictation runs, the model loads once per dictation on a background thread; the first words are buffered (bounded to a minute of audio) so they are not lost to the load, and loads serialize so rapid stop-and-restart cycles cannot stack models in memory. A stalled microphone delays the dictation's save, not the UI, like the meeting recorder.

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

The app writes one Ogg Opus or WAV file per take to the recordings folder. The tray icon and tooltip show the state and the elapsed time. The HUD shows the timer, one live dot per source, a level bar per source, and a warning when a source drops. The window has two screens, flipped by the header button and the menu bar's **Settings…** item: the recorder screen shows the state, the start and stop controls, and the last take; the settings screen holds the dictation hotkey, model, and language next to the sources, the format, and the output folder.

A dictation captures only the microphone, holds no file, and ends by inserting text into the focused app — pasted through the clipboard on Linux, typed as Unicode events elsewhere. `src/state.rs` models the audio lifecycle, so `koe-session` can take over when transcript tracking lands.

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
| `src/dictation.rs` | Dictation state machine and the mic-owner rule |
| `src/dictation_control.rs` | The dictation side of a shell, shared by both UIs |
| `src/dictation_worker.rs` | Dictation thread: microphone → transcript → keystrokes |
| `src/hotkey.rs` | Binding parser and the system-wide registration |
| `src/tray.rs` | `tray-icon` icon and menu |
| `src/icon.rs` | Code-drawn tray icon |
| `src/repaint.rs` | UI wake-up callback shared by both shells |
| `src/settings.rs` | Source, format, output path, and the persisted dictation settings |
| `src/error.rs` | Typed failures and their messages |

## Verify

```console
cargo test -p koe-desktop --features linux
cargo clippy -p koe-desktop --all-targets --features linux -- --deny warnings
cargo test -p koe-desktop --no-default-features --features gpui,linux
cargo clippy -p koe-desktop --all-targets --no-default-features --features gpui,linux -- --deny warnings
```

The tests cover the state transitions, the tray labels and menu ids, time formatting, path selection, plan construction, the HUD text, the dictation transitions, cancel, and the microphone-owner rule, the hotkey parser and its display form, the shared dictation control's press and draft paths, the settings file round trip and its fallbacks, and the dictation text and backlog helpers. The Rune window and the tray-backed failure path were rendered in the app. The GPUI window was exercised under Xvfb: the start path reached its failure state, source and format selection updated, dismiss, hide, close-to-tray, and quit behaved. The GPUI HUD was rendered in a throwaway preview because real capture is blocked on this host.

Real capture was not verified on the development host. Against a PulseAudio 17 null sink, the app crashes when a source opens, and the existing `cargo run -p koe-cli --features linux -- record` command aborts with `Assertion '!e->dead' failed` against the same sink. The fault is in the shared capture backend rather than the desktop shell. The tray icon's actual display was not verified because the host has no StatusNotifier watcher; the labels and menu ids are tested.

The dictation path was not exercised end to end on this host: there is no display server to register a global hotkey against, real capture is broken against the null sink, and no GGUF model was loaded. What is verified is the pure core — the parser, the state machines, the shared control's press and cancel paths, the persistence round trip, and the backlog and text helpers — under `cargo test`. The hotkey registration, the microphone capture, the live transcription, and both insertion paths — the clipboard paste and the Unicode typing fallback — need a real desktop session to verify, and the paste path in particular needs a check against a running fcitx or ibus.
