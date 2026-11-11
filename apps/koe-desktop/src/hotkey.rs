//! The dictation hotkey: a binding parsed from user text and registered
//! system-wide, so it fires while koe stays in the background.
//!
//! The parser and the display text are pure and tested headless. The
//! registration wraps `global-hotkey`, which supports Windows, macOS, and
//! X11. On Linux the manager inside `global-hotkey` cannot report its own
//! failed connection — it returns `Ok` and swallows the thread's error — so
//! [`Hotkeys::new`] probes the X connection first: without an X server it
//! refuses rather than arming a grab that can never fire. Under Wayland
//! with `XWayland` the grab only reaches X11 and `XWayland` windows, which the
//! READMEs document.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::repaint::Repaint;

/// The binding a fresh install uses.
///
/// `Ctrl+Shift+Space` is free at the OS level on Windows, macOS, and the
/// common Linux desktops. Press it to start a dictation, press it again to
/// stop and type.
pub const DEFAULT_BINDING: &str = "Ctrl+Shift+Space";

/// The default binding as a parsed hotkey.
#[must_use]
pub fn default_hotkey() -> HotKey {
    HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space)
}

/// Why a binding text names no usable hotkey.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingError {
    /// A `+` with nothing around it.
    EmptyToken,
    /// The binding names no key after its modifiers.
    NoKey,
    /// The binding names no modifier, so it would fire on every plain press.
    NoModifier,
    /// The binding names more than one key.
    TwoKeys,
    /// The token names no known modifier or key.
    UnknownToken(String),
}

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyToken => formatter.write_str("a part of the binding is empty"),
            Self::NoKey => formatter.write_str("the binding names no key"),
            Self::NoModifier => formatter
                .write_str("the binding needs at least one modifier (Ctrl, Alt, Shift, Super)"),
            Self::TwoKeys => formatter.write_str("the binding names more than one key"),
            Self::UnknownToken(token) => write!(formatter, "`{token}` is not a known key"),
        }
    }
}

/// Parses `text` as one hotkey binding, for example `Ctrl+Shift+Space`.
///
/// Tokens are split on `+` and ignore case and surrounding space. At least
/// one modifier and exactly one key are required, so a bare `Space` can never
/// swallow the user's typing.
///
/// # Errors
///
/// Returns [`BindingError`] naming what the text got wrong.
pub fn parse(text: &str) -> Result<HotKey, BindingError> {
    let mut mods = Modifiers::empty();
    let mut key = None;
    for token in text.split('+') {
        let token = token.trim();
        if token.is_empty() {
            return Err(BindingError::EmptyToken);
        }
        if let Some(modifier) = modifier_for(token) {
            mods |= modifier;
        } else if let Some(code) = key_for(token) {
            if key.replace(code).is_some() {
                return Err(BindingError::TwoKeys);
            }
        } else {
            return Err(BindingError::UnknownToken(token.to_owned()));
        }
    }
    let Some(key) = key else {
        return Err(BindingError::NoKey);
    };
    if mods.is_empty() {
        return Err(BindingError::NoModifier);
    }
    Ok(HotKey::new(Some(mods), key))
}

/// Writes `hotkey` the way the settings field shows it.
#[must_use]
pub fn display(hotkey: &HotKey) -> String {
    let mut text = String::new();
    if hotkey.mods.contains(Modifiers::CONTROL) {
        text.push_str("Ctrl+");
    }
    if hotkey.mods.contains(Modifiers::ALT) {
        text.push_str("Alt+");
    }
    if hotkey.mods.contains(Modifiers::SHIFT) {
        text.push_str("Shift+");
    }
    if hotkey.mods.contains(Modifiers::META) {
        text.push_str(if cfg!(target_os = "macos") {
            "Cmd+"
        } else {
            "Super+"
        });
    }
    text.push_str(&key_name(hotkey.key));
    text
}

/// The registered binding and the queue of its presses.
pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    hotkey: HotKey,
    presses: Arc<Mutex<VecDeque<u32>>>,
}

/// The presses the event handler collects. The handler is a process-wide
/// `OnceCell` inside `global-hotkey`, so the queue it feeds must outlive any
/// `Hotkeys` that a retried registration creates later.
static PRESSES: std::sync::OnceLock<Arc<Mutex<VecDeque<u32>>>> = std::sync::OnceLock::new();

/// Installs the handler once and returns the queue it feeds.
fn press_queue(repaint: Repaint) -> Arc<Mutex<VecDeque<u32>>> {
    PRESSES
        .get_or_init(move || {
            let presses = Arc::new(Mutex::new(VecDeque::new()));
            let queue = Arc::clone(&presses);
            GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                if event.state() == HotKeyState::Pressed
                    && let Ok(mut queue) = queue.lock()
                {
                    queue.push_back(event.id);
                }
                repaint();
            }));
            presses
        })
        .clone()
}

impl Hotkeys {
    /// Registers `hotkey` system-wide and routes presses to the UI repaint.
    ///
    /// On macOS and Windows the manager must live on the thread that runs the
    /// event loop, which every koe shell does.
    ///
    /// # Errors
    ///
    /// On macOS and Windows, returns the registration error, for example
    /// when another application already owns the binding. On Linux, where
    /// `global-hotkey` swallows registration failures, returns the
    /// reachability probe's error: a session without an X server (Wayland
    /// without `XWayland`, or no display at all) cannot fire the grab.
    pub fn new(hotkey: HotKey, repaint: Repaint) -> Result<Self, String> {
        // The Linux manager returns Ok even when its X connection dies, so
        // the connection is proven here before the grab is believed armed.
        #[cfg(target_os = "linux")]
        x_reachable()?;
        let manager = GlobalHotKeyManager::new().map_err(|error| error.to_string())?;
        manager
            .register(hotkey)
            .map_err(|error| error.to_string())?;
        let presses = press_queue(repaint);
        Ok(Self {
            manager,
            hotkey,
            presses,
        })
    }

    /// Replaces the binding, restoring the previous one when the new one
    /// cannot be registered.
    ///
    /// # Errors
    ///
    /// Returns the registration error; the previous binding stays active
    /// whenever the restore succeeds.
    pub fn rebind(&mut self, hotkey: HotKey) -> Result<(), String> {
        if hotkey == self.hotkey {
            return Ok(());
        }
        let _ = self.manager.unregister(self.hotkey);
        match self.manager.register(hotkey) {
            Ok(()) => {
                self.hotkey = hotkey;
                Ok(())
            }
            Err(error) => {
                let _ = self.manager.register(self.hotkey);
                Err(error.to_string())
            }
        }
    }

    /// Takes how many times the binding was pressed since the last poll, so
    /// two presses inside one poll window still mean two toggles.
    #[must_use]
    pub fn presses(&self) -> usize {
        let mut presses = 0;
        if let Ok(mut queue) = self.presses.lock() {
            for id in queue.drain(..) {
                presses += usize::from(id == self.hotkey.id());
            }
        }
        presses
    }
}

/// Proves an X server answers, with nothing else on the line that could
/// fail: a bare connection answers only the reachability question, while a
/// typing backend's constructor also fails for keymap reasons that would be
/// misreported as a missing server.
///
/// # Errors
///
/// Returns the connection error when no X server is reachable, which is the
/// case the `global-hotkey` manager cannot report by itself.
#[cfg(target_os = "linux")]
fn x_reachable() -> Result<(), String> {
    x11rb::connect(None)
        .map(|_| ())
        .map_err(|error| format!("no X server is reachable: {error}"))
}

/// The modifier a token names, if any.
fn modifier_for(token: &str) -> Option<Modifiers> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Modifiers::CONTROL),
        "alt" | "option" => Some(Modifiers::ALT),
        "shift" => Some(Modifiers::SHIFT),
        "super" | "meta" | "cmd" | "command" => Some(Modifiers::META),
        "cmdorctrl" | "ctrlorcmd" => Some(if cfg!(target_os = "macos") {
            Modifiers::META
        } else {
            Modifiers::CONTROL
        }),
        _ => None,
    }
}

/// The key a token names, if any.
fn key_for(token: &str) -> Option<Code> {
    use Code as C;
    let token = token.to_ascii_lowercase();
    let code = match token.as_str() {
        "a" => C::KeyA,
        "b" => C::KeyB,
        "c" => C::KeyC,
        "d" => C::KeyD,
        "e" => C::KeyE,
        "f" => C::KeyF,
        "g" => C::KeyG,
        "h" => C::KeyH,
        "i" => C::KeyI,
        "j" => C::KeyJ,
        "k" => C::KeyK,
        "l" => C::KeyL,
        "m" => C::KeyM,
        "n" => C::KeyN,
        "o" => C::KeyO,
        "p" => C::KeyP,
        "q" => C::KeyQ,
        "r" => C::KeyR,
        "s" => C::KeyS,
        "t" => C::KeyT,
        "u" => C::KeyU,
        "v" => C::KeyV,
        "w" => C::KeyW,
        "x" => C::KeyX,
        "y" => C::KeyY,
        "z" => C::KeyZ,
        "0" => C::Digit0,
        "1" => C::Digit1,
        "2" => C::Digit2,
        "3" => C::Digit3,
        "4" => C::Digit4,
        "5" => C::Digit5,
        "6" => C::Digit6,
        "7" => C::Digit7,
        "8" => C::Digit8,
        "9" => C::Digit9,
        "f1" => C::F1,
        "f2" => C::F2,
        "f3" => C::F3,
        "f4" => C::F4,
        "f5" => C::F5,
        "f6" => C::F6,
        "f7" => C::F7,
        "f8" => C::F8,
        "f9" => C::F9,
        "f10" => C::F10,
        "f11" => C::F11,
        "f12" => C::F12,
        "space" => C::Space,
        "tab" => C::Tab,
        "enter" | "return" => C::Enter,
        "escape" | "esc" => C::Escape,
        "backspace" => C::Backspace,
        "delete" | "del" => C::Delete,
        "insert" => C::Insert,
        "home" => C::Home,
        "end" => C::End,
        "pageup" => C::PageUp,
        "pagedown" => C::PageDown,
        "up" | "arrowup" => C::ArrowUp,
        "down" | "arrowdown" => C::ArrowDown,
        "left" | "arrowleft" => C::ArrowLeft,
        "right" | "arrowright" => C::ArrowRight,
        "comma" | "," => C::Comma,
        "period" | "." => C::Period,
        "slash" | "/" => C::Slash,
        "semicolon" | ";" => C::Semicolon,
        "quote" | "'" => C::Quote,
        "bracketleft" | "[" => C::BracketLeft,
        "bracketright" | "]" => C::BracketRight,
        "minus" | "-" => C::Minus,
        "equal" | "=" => C::Equal,
        "backquote" | "`" => C::Backquote,
        "backslash" | "\\" => C::Backslash,
        _ => return None,
    };
    Some(code)
}

/// The name [`display`](fn.display.html) writes for `code`.
///
/// Only the keys whose own [`Code`] name reads wrong are renamed; every other
/// key keeps the name `Code` writes for itself.
fn key_name(code: Code) -> String {
    match code {
        Code::KeyA => "A",
        Code::KeyB => "B",
        Code::KeyC => "C",
        Code::KeyD => "D",
        Code::KeyE => "E",
        Code::KeyF => "F",
        Code::KeyG => "G",
        Code::KeyH => "H",
        Code::KeyI => "I",
        Code::KeyJ => "J",
        Code::KeyK => "K",
        Code::KeyL => "L",
        Code::KeyM => "M",
        Code::KeyN => "N",
        Code::KeyO => "O",
        Code::KeyP => "P",
        Code::KeyQ => "Q",
        Code::KeyR => "R",
        Code::KeyS => "S",
        Code::KeyT => "T",
        Code::KeyU => "U",
        Code::KeyV => "V",
        Code::KeyW => "W",
        Code::KeyX => "X",
        Code::KeyY => "Y",
        Code::KeyZ => "Z",
        Code::Digit0 => "0",
        Code::Digit1 => "1",
        Code::Digit2 => "2",
        Code::Digit3 => "3",
        Code::Digit4 => "4",
        Code::Digit5 => "5",
        Code::Digit6 => "6",
        Code::Digit7 => "7",
        Code::Digit8 => "8",
        Code::Digit9 => "9",
        Code::ArrowUp => "Up",
        Code::ArrowDown => "Down",
        Code::ArrowLeft => "Left",
        Code::ArrowRight => "Right",
        other => return other.to_string(),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_default_binding_parses_to_its_hotkey() {
        assert_eq!(parse(DEFAULT_BINDING), Ok(default_hotkey()));
        assert_eq!(
            default_hotkey(),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space)
        );
        assert_eq!(display(&default_hotkey()), DEFAULT_BINDING);
    }

    #[test]
    fn tokens_ignore_case_and_space() {
        let expected = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyJ);
        assert_eq!(parse("ctrl+alt+j"), Ok(expected));
        assert_eq!(parse("CTRL + ALT + J"), Ok(expected));
    }

    #[test]
    fn every_modifier_token_maps_to_its_bit() {
        assert_eq!(
            parse("Ctrl+A"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::KeyA))
        );
        assert_eq!(
            parse("Control+A"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::KeyA))
        );
        assert_eq!(
            parse("Alt+A"),
            Ok(HotKey::new(Some(Modifiers::ALT), Code::KeyA))
        );
        assert_eq!(
            parse("Option+A"),
            Ok(HotKey::new(Some(Modifiers::ALT), Code::KeyA))
        );
        assert_eq!(
            parse("Shift+A"),
            Ok(HotKey::new(Some(Modifiers::SHIFT), Code::KeyA))
        );
        assert_eq!(
            parse("Super+A"),
            Ok(HotKey::new(Some(Modifiers::META), Code::KeyA))
        );
        assert_eq!(
            parse("Meta+A"),
            Ok(HotKey::new(Some(Modifiers::META), Code::KeyA))
        );
        assert_eq!(
            parse("Cmd+A"),
            Ok(HotKey::new(Some(Modifiers::META), Code::KeyA))
        );
        assert_eq!(
            parse("Command+A"),
            Ok(HotKey::new(Some(Modifiers::META), Code::KeyA))
        );
    }

    #[test]
    fn cmdorctrl_follows_the_platform() {
        let hotkey = parse("CmdOrCtrl+A");
        #[cfg(target_os = "macos")]
        assert_eq!(hotkey, Ok(HotKey::new(Some(Modifiers::META), Code::KeyA)));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            hotkey,
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::KeyA))
        );
    }

    #[test]
    fn every_key_token_parses_and_displays_back() {
        for text in [
            "Ctrl+A",
            "Ctrl+0",
            "Ctrl+F1",
            "Ctrl+F12",
            "Ctrl+Space",
            "Ctrl+Tab",
            "Ctrl+Enter",
            "Ctrl+Escape",
            "Ctrl+Backspace",
            "Ctrl+Delete",
            "Ctrl+Insert",
            "Ctrl+Home",
            "Ctrl+End",
            "Ctrl+PageUp",
            "Ctrl+PageDown",
            "Ctrl+Up",
            "Ctrl+Down",
            "Ctrl+Left",
            "Ctrl+Right",
            "Ctrl+Comma",
            "Ctrl+Period",
            "Ctrl+Slash",
            "Ctrl+Semicolon",
            "Ctrl+Quote",
            "Ctrl+BracketLeft",
            "Ctrl+BracketRight",
            "Ctrl+Minus",
            "Ctrl+Equal",
            "Ctrl+Backquote",
            "Ctrl+Backslash",
        ] {
            let hotkey = parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(display(&hotkey), text, "display of {text}");
        }
    }

    #[test]
    fn alias_tokens_parse_to_their_key() {
        for (alias, primary) in [
            ("Ctrl+Esc", "Ctrl+Escape"),
            ("Ctrl+Del", "Ctrl+Delete"),
            ("Ctrl+Return", "Ctrl+Enter"),
            ("Ctrl+ArrowUp", "Ctrl+Up"),
            ("ctrl+a", "Ctrl+A"),
            ("ctrl+alt+a", "Ctrl+Alt+A"),
        ] {
            assert_eq!(
                parse(alias).unwrap_or_else(|error| panic!("{alias}: {error}")),
                parse(primary).unwrap_or_else(|error| panic!("{primary}: {error}")),
                "{alias} and {primary} must name one key"
            );
        }
    }

    #[test]
    fn punctuation_tokens_match_their_keys() {
        assert_eq!(
            parse("Ctrl+,"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Comma))
        );
        assert_eq!(
            parse("Ctrl+."),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Period))
        );
        assert_eq!(
            parse("Ctrl+/"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Slash))
        );
        assert_eq!(
            parse("Ctrl+;"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Semicolon))
        );
        assert_eq!(
            parse("Ctrl+'"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Quote))
        );
        assert_eq!(
            parse("Ctrl+["),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::BracketLeft))
        );
        assert_eq!(
            parse("Ctrl+]"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::BracketRight))
        );
        assert_eq!(
            parse("Ctrl+-"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Minus))
        );
        assert_eq!(
            parse("Ctrl+="),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Equal))
        );
        assert_eq!(
            parse("Ctrl+`"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Backquote))
        );
        assert_eq!(
            parse("Ctrl+\\"),
            Ok(HotKey::new(Some(Modifiers::CONTROL), Code::Backslash))
        );
    }

    #[test]
    fn every_rejected_binding_names_its_problem() {
        assert_eq!(parse(""), Err(BindingError::EmptyToken));
        assert_eq!(parse("  "), Err(BindingError::EmptyToken));
        assert_eq!(parse("Ctrl+Shift"), Err(BindingError::NoKey));
        assert_eq!(parse("Space"), Err(BindingError::NoModifier));
        assert_eq!(parse("F1"), Err(BindingError::NoModifier));
        assert_eq!(parse("Ctrl++A"), Err(BindingError::EmptyToken));
        assert_eq!(parse("Ctrl+"), Err(BindingError::EmptyToken));
        assert_eq!(parse("Ctrl+A+B"), Err(BindingError::TwoKeys));
        assert_eq!(
            parse("Ctrl+Foo"),
            Err(BindingError::UnknownToken("Foo".to_owned()))
        );
        assert_eq!(parse("Ctrl+Space+X"), Err(BindingError::TwoKeys));
    }

    #[test]
    fn errors_read_as_sentences() {
        for error in [
            BindingError::EmptyToken,
            BindingError::NoKey,
            BindingError::NoModifier,
            BindingError::TwoKeys,
            BindingError::UnknownToken("Foo".to_owned()),
        ] {
            assert_ne!(error.to_string(), "");
        }
        assert_eq!(
            BindingError::UnknownToken("Foo".to_owned()).to_string(),
            "`Foo` is not a known key"
        );
    }

    #[test]
    fn display_round_trips_through_parse() {
        for text in ["Ctrl+Shift+Space", "Alt+F7", "Super+D", "Shift+Comma"] {
            let hotkey = parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            let shown = display(&hotkey);
            assert_eq!(parse(&shown), Ok(hotkey), "round trip of {text}");
        }
    }

    #[test]
    fn display_orders_modifiers_and_names_keys() {
        let hotkey = parse("shift+super+ctrl+alt+pagedown").unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(display(&hotkey), "Ctrl+Alt+Shift+Cmd+PageDown");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(display(&hotkey), "Ctrl+Alt+Shift+Super+PageDown");
    }
}
