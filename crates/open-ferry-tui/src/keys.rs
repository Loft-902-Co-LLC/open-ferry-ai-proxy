// Ported with how Bubble Tea names key presses (Key, Key.String, keyNames;
// v1.3.10, MIT, see licenses/bubbletea-LICENSE).
// https://github.com/charmbracelet/bubbletea

//! Key presses, named as Bubble Tea names them, which is what the tabs
//! match on.
//!
//! Deviations from upstream:
//! - Keys come from crossterm rather than from Bubble Tea's own input
//!   reader, so a terminal may report a few combinations differently; the
//!   keys the TUI binds (letters, digits, arrows, Enter, Esc, Tab,
//!   Shift+Tab, Backspace, Delete, Home, End, Page Up and Down, Space and
//!   Ctrl with a letter) are named as Bubble Tea names them.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A key press (Bubble Tea's `Key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    /// The key's name, or `None` for typed text (`KeyRunes`).
    name: Option<&'static str>,
    /// The text the key types: the characters typed or pasted, and a
    /// space for the space bar.
    pub(crate) runes: Vec<char>,
    /// Whether Alt was held.
    pub(crate) alt: bool,
    /// Whether the text was pasted.
    pub(crate) paste: bool,
}

impl Key {
    /// A named key, such as `"enter"` or `"ctrl+c"`; `" "` is the space bar.
    pub(crate) fn named(name: &'static str) -> Self {
        let runes = if name == " " { vec![' '] } else { Vec::new() };
        Self {
            name: Some(name),
            runes,
            alt: false,
            paste: false,
        }
    }

    /// Typed text.
    pub(crate) fn text(s: &str) -> Self {
        Self {
            name: None,
            runes: s.chars().collect(),
            alt: false,
            paste: false,
        }
    }

    /// Pasted text.
    pub(crate) fn pasted(s: &str) -> Self {
        Self {
            paste: true,
            ..Self::text(s)
        }
    }

    /// The key as Bubble Tea's `Key.String` gives it: `alt+` when Alt was
    /// held, then the name, or the text typed (in brackets when pasted).
    pub(crate) fn string(&self) -> String {
        let mut out = String::new();
        if self.alt {
            out.push_str("alt+");
        }
        match self.name {
            Some(name) => out.push_str(name),
            None => {
                if self.paste {
                    out.push('[');
                }
                out.extend(self.runes.iter());
                if self.paste {
                    out.push(']');
                }
            }
        }
        out
    }

    /// The key a crossterm key event is, if Bubble Tea would report one.
    pub(crate) fn from_crossterm(event: &KeyEvent) -> Option<Self> {
        if event.kind == KeyEventKind::Release {
            return None;
        }
        let mods = event.modifiers;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let shift = mods.contains(KeyModifiers::SHIFT);
        let alt = mods.contains(KeyModifiers::ALT);
        let mut key = match event.code {
            KeyCode::Char(c) if ctrl => Self::named(ctrl_name(c)?),
            KeyCode::Char(' ') => Self::named(" "),
            KeyCode::Char(c) => Self::text(c.encode_utf8(&mut [0; 4])),
            KeyCode::Enter => Self::named("enter"),
            KeyCode::Esc => Self::named("esc"),
            KeyCode::Tab if shift => Self::named("shift+tab"),
            KeyCode::Tab => Self::named("tab"),
            KeyCode::BackTab => Self::named("shift+tab"),
            KeyCode::Backspace => Self::named("backspace"),
            KeyCode::Delete => Self::named("delete"),
            KeyCode::Insert => Self::named("insert"),
            KeyCode::Up => Self::named(arrow(
                ctrl,
                shift,
                ["up", "ctrl+up", "shift+up", "ctrl+shift+up"],
            )),
            KeyCode::Down => Self::named(arrow(
                ctrl,
                shift,
                ["down", "ctrl+down", "shift+down", "ctrl+shift+down"],
            )),
            KeyCode::Left => Self::named(arrow(
                ctrl,
                shift,
                ["left", "ctrl+left", "shift+left", "ctrl+shift+left"],
            )),
            KeyCode::Right => Self::named(arrow(
                ctrl,
                shift,
                ["right", "ctrl+right", "shift+right", "ctrl+shift+right"],
            )),
            KeyCode::Home => Self::named(arrow(
                ctrl,
                shift,
                ["home", "ctrl+home", "shift+home", "ctrl+shift+home"],
            )),
            KeyCode::End => Self::named(arrow(
                ctrl,
                shift,
                ["end", "ctrl+end", "shift+end", "ctrl+shift+end"],
            )),
            KeyCode::PageUp => Self::named(if ctrl { "ctrl+pgup" } else { "pgup" }),
            KeyCode::PageDown => Self::named(if ctrl { "ctrl+pgdown" } else { "pgdown" }),
            KeyCode::F(n) => Self::named(function_key(n)?),
            _ => return None,
        };
        key.alt = alt;
        Some(key)
    }
}

/// The name of an arrow-like key with Ctrl and Shift: plain, Ctrl, Shift,
/// both.
fn arrow(ctrl: bool, shift: bool, names: [&'static str; 4]) -> &'static str {
    let [plain, with_ctrl, with_shift, with_both] = names;
    match (ctrl, shift) {
        (false, false) => plain,
        (true, false) => with_ctrl,
        (false, true) => with_shift,
        (true, true) => with_both,
    }
}

/// The name of Ctrl with a character, as Bubble Tea names the control
/// codes.
fn ctrl_name(c: char) -> Option<&'static str> {
    Some(match c.to_ascii_lowercase() {
        '@' | ' ' | '`' | '2' => "ctrl+@",
        'a' => "ctrl+a",
        'b' => "ctrl+b",
        'c' => "ctrl+c",
        'd' => "ctrl+d",
        'e' => "ctrl+e",
        'f' => "ctrl+f",
        'g' => "ctrl+g",
        'h' => "ctrl+h",
        'i' => "tab",
        'j' => "ctrl+j",
        'k' => "ctrl+k",
        'l' => "ctrl+l",
        'm' => "enter",
        'n' => "ctrl+n",
        'o' => "ctrl+o",
        'p' => "ctrl+p",
        'q' => "ctrl+q",
        'r' => "ctrl+r",
        's' => "ctrl+s",
        't' => "ctrl+t",
        'u' => "ctrl+u",
        'v' => "ctrl+v",
        'w' => "ctrl+w",
        'x' => "ctrl+x",
        'y' => "ctrl+y",
        'z' => "ctrl+z",
        '[' | '3' => "esc",
        '\\' | '4' => "ctrl+\\",
        ']' | '5' => "ctrl+]",
        '^' | '6' => "ctrl+^",
        '_' | '-' | '7' => "ctrl+_",
        _ => return None,
    })
}

/// The name of a function key.
fn function_key(n: u8) -> Option<&'static str> {
    const NAMES: [&str; 20] = [
        "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14",
        "f15", "f16", "f17", "f18", "f19", "f20",
    ];
    NAMES.get(usize::from(n).checked_sub(1)?).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        Key::from_crossterm(&KeyEvent::new(code, modifiers)).map(|k| k.string())
    }

    // Not upstream's: keys are named as Bubble Tea's Key.String names
    // them.
    #[test]
    fn names_keys_as_bubble_tea_does() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(press(KeyCode::Char('q'), none).as_deref(), Some("q"));
        assert_eq!(
            press(KeyCode::Char('L'), KeyModifiers::SHIFT).as_deref(),
            Some("L")
        );
        assert_eq!(press(KeyCode::Char('c'), ctrl).as_deref(), Some("ctrl+c"));
        assert_eq!(press(KeyCode::Char('U'), ctrl).as_deref(), Some("ctrl+u"));
        assert_eq!(press(KeyCode::Char(' '), none).as_deref(), Some(" "));
        assert_eq!(
            press(KeyCode::Char('b'), KeyModifiers::ALT).as_deref(),
            Some("alt+b")
        );
        assert_eq!(press(KeyCode::Enter, none).as_deref(), Some("enter"));
        assert_eq!(press(KeyCode::Esc, none).as_deref(), Some("esc"));
        assert_eq!(press(KeyCode::Tab, none).as_deref(), Some("tab"));
        assert_eq!(
            press(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(),
            Some("shift+tab")
        );
        assert_eq!(
            press(KeyCode::Backspace, KeyModifiers::ALT).as_deref(),
            Some("alt+backspace")
        );
        assert_eq!(press(KeyCode::Left, ctrl).as_deref(), Some("ctrl+left"));
        assert_eq!(press(KeyCode::PageDown, none).as_deref(), Some("pgdown"));
        assert_eq!(press(KeyCode::F(12), none).as_deref(), Some("f12"));
        assert_eq!(press(KeyCode::F(0), none), None);
        assert_eq!(press(KeyCode::CapsLock, none), None);
        let mut release = KeyEvent::new(KeyCode::Char('q'), none);
        release.kind = KeyEventKind::Release;
        assert_eq!(Key::from_crossterm(&release), None);
        assert_eq!(Key::pasted("ab").string(), "[ab]");
        assert_eq!(Key::named(" ").runes, [' ']);
    }
}
