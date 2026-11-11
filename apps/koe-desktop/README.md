# koe desktop

The koe desktop app records the microphone and the system audio from a menu bar icon. An overlay HUD shows the timer and the level of each source while a take runs, and the recognized text while a dictation runs. The menu bar item is a `tray-icon` status item.

## Run

Build with the host feature. The platform feature is required: a build with none of `linux`, `macos`, and `windows` starts, but `koe-capture` returns `Error::Unsupported` from every capture call, so a take fails as soon as it opens its sources.

```console
cargo run -p koe-desktop --features linux
cargo run -p koe-desktop --features macos
cargo run -p koe-desktop --features windows
```

On macOS the app starts in the menu bar with no window. Choose **Open koe** in the menu to show the control window, or **Settings…** to show it on the settings screen. On Linux and Windows the window opens at launch. The menu opens on a left click, and **Open koe** shows the window again. Use **Hide to menu bar** to keep only the tray icon. Closing the window keeps the process alive while the tray icon exists; **Quit** exits. The process runs as an accessory app on macOS, so it has no Dock icon. Set `KOE_TRAY_DEBUG=1` to print tray events while diagnosing a menu that does not act. Set `KOE_HUD=off` to disable the overlay window if a compositor places it badly.

The tray icon needs a StatusNotifier host on Linux. KDE, Sway, and most bars provide one. GNOME needs an AppIndicator extension. Without a host, the window is the only control. Hiding the window leaves the app running with no visible control, so relaunch it from the same binary.

## Dictation

Press the dictation hotkey anywhere in the OS — with koe hidden to the menu bar and another app focused — and koe starts capturing the microphone. Press it again and the speech is transcribed on-device and inserted into the focused app, as if it came from the keyboard. A misfire is correctable: **Cancel dictation** in the menu bar discards the take and types nothing. The tray icon turns purple while a dictation runs, and the window shows the typed text or the failure.

The overlay HUD appears for a dictation too. It shows the words as the model recognizes them — the committed text followed by the provisional tail — with the elapsed time and a DICTATING badge while the microphone is open, or TRANSCRIBING once the capture stopped and the last words are being inferred. The text box holds two wrapped lines and shows the newest 44 characters of a longer transcript, so the window keeps its fixed size through a long dictation. A hotkey press that is refused — a meeting take owns the microphone, or no transcription model is set — shows its reason in the window notice and the tray tooltip, and holds it in the HUD for five seconds while no take or dictation is running: a running capture keeps the HUD to itself, so a refusal never interrupts a take's timer or a dictation's transcript. A refusal longer than the HUD's two lines is cut at its end there; the window notice keeps the whole sentence. `KOE_HUD=off` disables the HUD for takes and dictations alike.

The interaction is a toggle, deliberately: press to start, press again to stop and insert. hex, the organization's other dictation app, defaults to hold-to-talk with a 450 ms pre-roll; koe opens the microphone at the press instead, so speech in the first fraction of a second can be missed. A pre-roll means an always-open microphone, which a meeting recorder will not do — the microphone is a shared resource with the meeting flow, and the mutual exclusion between dictation and a take is a tested invariant. The stop commits; the cancel discards.

The binding, the model, and the language live in the window's Dictation section — on the settings screen, reached from the header button or the menu bar's **Settings…** item — and persist in a settings file (`~/.config/koe/settings.conf` on Linux, `~/Library/Application Support/koe/settings.conf` on macOS, `%APPDATA%\koe\settings.conf` on Windows), written atomically like a session record. The default binding is `Ctrl+Shift+Space`, chosen to be free of OS-level shortcuts on Windows, macOS, and the common Linux desktops; a binding without a modifier is rejected, so a bare `Space` can never swallow typing. The hotkey field commits on Enter — typing toward a binding passes through real shortcuts like `Ctrl+S`, and a live rebind would grab every one of them — and an Enter in the field also retries a registration that failed. The model and language fields persist as they change.

Dictation and a meeting take share one microphone, so each refuses while the other is active: the hotkey reports that a take owns the microphone, and Start reports that a dictation owns it. The rule lives in the pure state machines (`src/dictation.rs`) and is unit-tested.

Platform limits, honestly:

- **Linux.** `global-hotkey` registers shortcuts on X11 only, and its Linux manager cannot report its own failed registration — it returns success even when its X connection dies — so koe probes the X connection before arming the binding: without an X server (a Wayland session without XWayland, or no display) the app reports "No global hotkey" and dictation stays off. With XWayland the grab only sees X11 and XWayland windows: while a native Wayland window holds focus, the hotkey does not fire. Text insertion pastes through the clipboard — set the clipboard, send Ctrl+V, restore what was there half a second later — because a synthetic keysym is indistinguishable from a physical key and a running input method (fcitx, ibus — the norm on a Japanese desktop) would filter it; a paste inserts without crossing the input method. The paste snapshots what was there first — text and images, each up to hex's 32 MB cap; a bigger clipboard is left untouched and the text is typed as Unicode instead, which does not use an input method but cannot bypass one that is running. Only plain text and images come back: a rich-text clipboard (a browser's, a mail client's) loses its formatting, and a clipboard holding anything else — a file manager's file list, application-specific formats — is replaced by the transcript, because arboard exposes no way to enumerate and restore the other targets. An X selection only outlives its owner through a clipboard manager, so koe holds one clipboard handle for its whole run and the restored clipboard lives as long as koe does; on a desktop without a manager (dwm, i3, XMonad — most tiling setups) whatever koe last put there dies with koe. Terminals treat Ctrl+V as a quoted-insert literal: dictating into one inserts nothing, latches the terminal's literal-next state so the next control key lands as text, and looks like success — the shortcut sends, the terminal reads nothing from the clipboard, and the restore puts the previous clipboard back, so the transcript is discarded. koe has no per-app chord preference (hex's TerminalPaste flavour is out of scope); dictate into a text field. Both insertion paths reach X11 and XWayland windows only. The model must be a GGUF file (the same `--model` family as `koe-cli`); without one the hotkey refuses and the window says so.
- **macOS.** The system speech model is used when no GGUF path is set, like `koe-cli` without `--model`. Text is typed as Unicode key events, which needs the accessibility permission; whether a running input method filters those events is unverified, as are these paths generally — they were not built here.
- **Windows.** The GGUF model is required, and `SendInput` types the text as Unicode key events. Not verified here.

While a dictation runs, the model loads once per dictation on a background thread; the first words are buffered (bounded to a minute of audio) so they are not lost to the load, and loads serialize so rapid stop-and-restart cycles cannot stack models in memory. A stalled microphone delays the dictation's save, not the UI, like the meeting recorder.

## Why GPUI with tray-icon

The app needs two OS surfaces. One is a menu bar item. The other is a transparent, always-on-top overlay. GPUI gives the overlay first-class support and owns one event loop. `tray-icon` gives the menu bar item on macOS, Windows, and Linux.

The window is built from gpui-kit components: `TitleBar`, `Button`, `Checkbox`, `Alert`, `GroupBox`, and `Progress`. The HUD is painted directly so every pixel comes from the HUD state.

The alternatives fail in specific ways.

- **A hand-written tray on macOS.** Writing the `NSStatusItem` by hand means `objc2-app-kit`: Hex, another Anomaly app, does that in 999 lines with 21 unsafe blocks. koe's workspace denies unsafe code, so `tray-icon` is the safer bridge.
- **wgpu alone.** It is a renderer, not a UI framework.

GPUI is pinned by gpui-kit to `gpui-pre =0.3.11`. Its click-through API (`Window::set_input_region`) is Wayland-only, so on X11 and macOS the non-interactive HUD still receives pointer events in its 340×88 area. On Wayland the HUD uses an empty input region.

## What it does

The app writes one Ogg Opus or WAV file per take to the recordings folder. The tray icon and tooltip show the state and the elapsed time. The HUD shows the timer, one live dot per source, a level bar per source, and a warning when a source drops; for a dictation it shows the state, the elapsed time, and the live transcript instead of the meters. A running take or dictation always owns the HUD: a refused press never replaces its timer, dots, meters, warning, or transcript, and the refusal draws only while nothing is running. The window has two screens, flipped by the header button and the menu bar's **Settings…** item: the recorder screen shows the state, the start and stop controls, and the last take; the settings screen holds the dictation hotkey, model, and language next to the sources, the format, and the output folder.

A dictation captures only the microphone, holds no file, and ends by inserting text into the focused app — pasted through the clipboard on Linux, typed as Unicode events elsewhere. `src/state.rs` models the audio lifecycle, so `koe-session` can take over when transcript tracking lands.

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
| `src/dictation.rs` | Dictation state machine and the mic-owner rule |
| `src/dictation_control.rs` | The dictation machine, worker, settings, and hotkey a shell embeds |
| `src/dictation_worker.rs` | Dictation thread: microphone → transcript → keystrokes |
| `src/hotkey.rs` | Binding parser and the system-wide registration |
| `src/tray.rs` | `tray-icon` icon and menu |
| `src/icon.rs` | Code-drawn tray icon |
| `src/settings.rs` | Source, format, output path, and the persisted dictation settings |
| `src/error.rs` | Typed failures and their messages |

## Verify

```console
cargo test -p koe-desktop --features linux
cargo clippy -p koe-desktop --all-targets --features linux -- --deny warnings
```

The tests cover the state transitions, the tray labels and menu ids, time formatting, path selection, plan construction, the HUD text, the HUD states a take and a dictation derive, the transcript tail, the dictation transitions, cancel, the microphone-owner rule, the hotkey parser and its display form, the dictation control's press and draft paths, the settings file round trip and its fallbacks, and the dictation text, live transcript, and backlog helpers. The GPUI window was exercised under Xvfb: the start path reached its failure state, source and format selection updated, dismiss, hide, close-to-tray, and quit behaved. The GPUI HUD was rendered in a throwaway preview because real capture is blocked on this host.

Real capture was not verified on the development host. Against a PulseAudio 17 null sink, the app crashes when a source opens, and the existing `cargo run -p koe-cli --features linux -- record` command aborts with `Assertion '!e->dead' failed` against the same sink. The fault is in the shared capture backend rather than the desktop shell. The tray icon's actual display was not verified because the host has no StatusNotifier watcher; the labels and menu ids are tested.

The dictation path was not exercised end to end on this host: there is no display server to register a global hotkey against, real capture is broken against the null sink, and no GGUF model was loaded. What is verified is the pure core — the parser, the state machines, the dictation control's press and cancel paths, the persistence round trip, and the backlog and text helpers — under `cargo test`. The hotkey registration, the microphone capture, the live transcription, and both insertion paths — the clipboard paste and the Unicode typing fallback — need a real desktop session to verify, and the paste path in particular needs a check against a running fcitx or ibus.

The dictation settings screen was added after the window run above and was not exercised end to end here: under Xvfb the Vulkan adapter has no DRI3 presentation, so the window never opened. The fields' state, the hotkey parser and display, and the settings round trip are covered by the tests above.

The dictation HUD was not drawn on this host: the overlay needs a display server, and nothing here can feed speech into a model. What is covered is its state derivation, its transcript cap, and the live transcript the dictation thread publishes, including the final committed text that replaces the provisional tail before insertion. Whether the non-focus HUD window leaves keyboard focus in the target app is also unverified, and it matters more here than for the take HUD: the take HUD opens before a target app is chosen, while the dictation HUD opens a popup while the target app already holds focus, so a focus change would break both insertion paths — the clipboard paste and the Unicode typing fallback — before they run.
