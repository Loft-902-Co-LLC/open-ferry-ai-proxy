// Ported with how github.com/charmbracelet/bubbles edits a line of text
// (textinput: New, SetValue, setValueInternal, Value, SetCursor,
// CursorStart, CursorEnd, Focus, Blur, insertRunesFromUserInput,
// handleOverflow, deleteBeforeCursor, deleteAfterCursor,
// deleteWordBackward, deleteWordForward, wordBackward, wordForward,
// echoTransform, Update, View, placeholderView, DefaultKeyMap; cursor:
// View; runeutil: Sanitize; v1.0.0, MIT, see licenses/bubbles-LICENSE).
// https://github.com/charmbracelet/bubbles

//! A one-line text input, as the bubbles text input the TUI's prompts use.
//!
//! Deviations from upstream:
//! - The cursor doesn't blink: it is always shown, in reverse video, while
//!   the input has focus. Upstream's blinks every 530 ms.
//! - Ctrl+V does nothing; text the terminal pastes still goes in.
//!   Upstream's reads the system clipboard on Ctrl+V.
//! - Alt+Delete and Alt+D at the second-to-last character delete the last
//!   one; upstream's panics there, reading past the end of the text.
//! - Suggestions and validation aren't ported: the TUI uses neither.

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi;
use crate::keys::Key;
use crate::style::{Color, Style};

/// How upstream shows the text under the cursor when it has focus.
const CURSOR: Style = Style::new().inline(true).reverse(true);

/// bubbles' placeholder style.
const PLACEHOLDER: Style = Style::new().fg(Color::Ansi(240));

/// A text input (bubbles' `textinput.Model`).
#[derive(Debug, Clone)]
pub(crate) struct TextInput {
    pub(crate) prompt: String,
    pub(crate) placeholder: String,
    /// Whether the text shows as `*`s (`EchoPassword`).
    pub(crate) password: bool,
    pub(crate) char_limit: usize,
    pub(crate) width: i64,
    value: Vec<char>,
    focus: bool,
    pos: usize,
    offset: usize,
    offset_right: usize,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInput {
    /// `textinput.New`.
    pub(crate) fn new() -> Self {
        Self {
            prompt: "> ".to_owned(),
            placeholder: String::new(),
            password: false,
            char_limit: 0,
            width: 0,
            value: Vec::new(),
            focus: false,
            pos: 0,
            offset: 0,
            offset_right: 0,
        }
    }

    /// `SetValue`.
    pub(crate) fn set_value(&mut self, s: &str) {
        let runes = sanitize(s.chars());
        self.set_value_internal(runes);
    }

    fn set_value_internal(&mut self, mut runes: Vec<char>) {
        let empty = self.value.is_empty();
        if self.char_limit > 0 && runes.len() > self.char_limit {
            runes.truncate(self.char_limit);
        }
        self.value = runes;
        if (self.pos == 0 && empty) || self.pos > self.value.len() {
            self.set_cursor(self.value.len());
        }
        self.handle_overflow();
    }

    /// `Value`.
    pub(crate) fn value(&self) -> String {
        self.value.iter().collect()
    }

    /// `SetCursor`.
    pub(crate) fn set_cursor(&mut self, pos: usize) {
        self.pos = pos.min(self.value.len());
        self.handle_overflow();
    }

    /// `CursorStart`.
    pub(crate) fn cursor_start(&mut self) {
        self.set_cursor(0);
    }

    /// `CursorEnd`.
    pub(crate) fn cursor_end(&mut self) {
        self.set_cursor(self.value.len());
    }

    /// `Focus`.
    pub(crate) fn focus(&mut self) {
        self.focus = true;
    }

    /// `Blur`.
    pub(crate) fn blur(&mut self) {
        self.focus = false;
    }

    fn insert_runes_from_user_input(&mut self, v: &[char]) {
        let mut paste = sanitize(v.iter().copied());
        let mut avail = 0;
        if self.char_limit > 0 {
            avail = self.char_limit.saturating_sub(self.value.len());
            if avail == 0 {
                return;
            }
            paste.truncate(avail);
        }
        let pos = self.pos.min(self.value.len());
        let tail = self.value.split_off(pos);
        let mut head = std::mem::take(&mut self.value);
        for r in paste {
            head.push(r);
            self.pos += 1;
            if self.char_limit > 0 {
                avail -= 1;
                if avail == 0 {
                    break;
                }
            }
        }
        head.extend(tail);
        self.set_value_internal(head);
    }

    fn handle_overflow(&mut self) {
        let width = self.width;
        let value_width = width_of(&self.value);
        if width <= 0 || value_width <= width {
            self.offset = 0;
            self.offset_right = self.value.len();
            return;
        }
        self.offset_right = self.offset_right.min(self.value.len());
        if self.pos < self.offset {
            self.offset = self.pos;
            let runes = self.value.get(self.offset..).unwrap_or_default();
            let (mut w, mut i) = (0, 0);
            while i < runes.len() && w <= width {
                w += runes.get(i).map_or(0, |&r| rune_width(r));
                if w <= width + 1 {
                    i += 1;
                }
            }
            self.offset_right = self.offset + i;
        } else if self.pos >= self.offset_right {
            self.offset_right = self.pos;
            let runes = self.value.get(..self.offset_right).unwrap_or_default();
            let n = i64::try_from(runes.len()).unwrap_or(i64::MAX);
            let (mut w, mut i) = (0, n - 1);
            while i > 0 && w < width {
                w += usize::try_from(i)
                    .ok()
                    .and_then(|i| runes.get(i))
                    .map_or(0, |&r| rune_width(r));
                if w <= width {
                    i -= 1;
                }
            }
            let back = usize::try_from(n - 1 - i).unwrap_or(0);
            self.offset = self.offset_right.saturating_sub(back);
        }
    }

    fn delete_before_cursor(&mut self) {
        self.value.drain(..self.pos.min(self.value.len()));
        self.offset = 0;
        self.set_cursor(0);
    }

    fn delete_after_cursor(&mut self) {
        self.value.truncate(self.pos);
        self.set_cursor(self.value.len());
    }

    fn is_space_at(&self, i: usize) -> bool {
        self.value.get(i).is_some_and(|c| c.is_whitespace())
    }

    fn delete_word_backward(&mut self) {
        if self.pos == 0 || self.value.is_empty() {
            return;
        }
        if self.password {
            self.delete_before_cursor();
            return;
        }
        let old_pos = self.pos;
        self.set_cursor(self.pos - 1);
        while self.is_space_at(self.pos) {
            if self.pos == 0 {
                break;
            }
            self.set_cursor(self.pos - 1);
        }
        while self.pos > 0 {
            if self.is_space_at(self.pos) {
                self.set_cursor(self.pos + 1);
                break;
            }
            self.set_cursor(self.pos - 1);
        }
        if old_pos > self.value.len() {
            self.value.truncate(self.pos);
        } else {
            let start = self.pos.min(old_pos);
            self.value.drain(start..old_pos);
        }
    }

    fn delete_word_forward(&mut self) {
        if self.pos >= self.value.len() || self.value.is_empty() {
            return;
        }
        if self.password {
            self.delete_after_cursor();
            return;
        }
        let old_pos = self.pos;
        self.set_cursor(self.pos + 1);
        // Upstream reads the character at the cursor here even when the
        // cursor has reached the end, and panics.
        while self.is_space_at(self.pos) {
            self.set_cursor(self.pos + 1);
            if self.pos >= self.value.len() {
                break;
            }
        }
        while self.pos < self.value.len() && !self.is_space_at(self.pos) {
            self.set_cursor(self.pos + 1);
        }
        let end = self.pos.min(self.value.len());
        if old_pos < end {
            self.value.drain(old_pos..end);
        }
        self.set_cursor(old_pos);
    }

    fn word_backward(&mut self) {
        if self.pos == 0 || self.value.is_empty() {
            return;
        }
        if self.password {
            self.cursor_start();
            return;
        }
        let mut i = self.pos;
        while i > 0 && self.is_space_at(i - 1) {
            self.set_cursor(self.pos.saturating_sub(1));
            i -= 1;
        }
        while i > 0 && !self.is_space_at(i - 1) {
            self.set_cursor(self.pos.saturating_sub(1));
            i -= 1;
        }
    }

    fn word_forward(&mut self) {
        if self.pos >= self.value.len() || self.value.is_empty() {
            return;
        }
        if self.password {
            self.cursor_end();
            return;
        }
        let mut i = self.pos;
        while i < self.value.len() && self.is_space_at(i) {
            self.set_cursor(self.pos + 1);
            i += 1;
        }
        while i < self.value.len() && !self.is_space_at(i) {
            self.set_cursor(self.pos + 1);
            i += 1;
        }
    }

    fn echo(&self, v: &[char]) -> String {
        if self.password {
            "*".repeat(width_of(v).try_into().unwrap_or(0))
        } else {
            v.iter().collect()
        }
    }

    /// `Update` for a key press; it does nothing without focus.
    pub(crate) fn update(&mut self, key: &Key) {
        if !self.focus {
            return;
        }
        match key.string().as_str() {
            "alt+backspace" | "ctrl+w" => self.delete_word_backward(),
            "backspace" | "ctrl+h" => {
                if !self.value.is_empty() {
                    let at = self.pos.saturating_sub(1).min(self.value.len());
                    let end = self.pos.min(self.value.len());
                    self.value.drain(at..end);
                    if self.pos > 0 {
                        self.set_cursor(self.pos - 1);
                    }
                }
            }
            "alt+left" | "ctrl+left" | "alt+b" => self.word_backward(),
            "left" | "ctrl+b" => {
                if self.pos > 0 {
                    self.set_cursor(self.pos - 1);
                }
            }
            "alt+right" | "ctrl+right" | "alt+f" => self.word_forward(),
            "right" | "ctrl+f" => {
                if self.pos < self.value.len() {
                    self.set_cursor(self.pos + 1);
                }
            }
            "home" | "ctrl+a" => self.cursor_start(),
            "delete" | "ctrl+d" => {
                if self.pos < self.value.len() {
                    self.value.remove(self.pos);
                }
            }
            "end" | "ctrl+e" => self.cursor_end(),
            "ctrl+k" => self.delete_after_cursor(),
            "ctrl+u" => self.delete_before_cursor(),
            "ctrl+v" | "down" | "ctrl+n" | "up" | "ctrl+p" => {}
            "alt+delete" | "alt+d" => self.delete_word_forward(),
            _ => {
                let runes = key.runes.clone();
                self.insert_runes_from_user_input(&runes);
            }
        }
        self.handle_overflow();
    }

    fn cursor_view(&self, ch: &str, text_style: Style) -> String {
        if self.focus {
            CURSOR.render(ch)
        } else {
            text_style.inline(true).render(ch)
        }
    }

    /// `View`.
    pub(crate) fn view(&self) -> String {
        if self.value.is_empty() && !self.placeholder.is_empty() {
            return self.placeholder_view();
        }
        let value = self
            .value
            .get(self.offset..self.offset_right)
            .unwrap_or_default();
        let pos = self.pos.saturating_sub(self.offset);
        let mut v = self.echo(value.get(..pos).unwrap_or(value));
        if let Some(&ch) = value.get(pos) {
            v.push_str(&self.cursor_view(&self.echo(&[ch]), Style::new()));
            v.push_str(&self.echo(value.get(pos + 1..).unwrap_or_default()));
        } else {
            v.push_str(&self.cursor_view(" ", Style::new()));
        }
        let value_width = width_of(value);
        if self.width > 0 && value_width <= self.width {
            let mut padding = (self.width - value_width).max(0);
            if value_width + padding <= self.width && pos < value.len() {
                padding += 1;
            }
            v.push_str(&" ".repeat(padding.try_into().unwrap_or(0)));
        }
        Style::new().render(&self.prompt) + &v
    }

    fn placeholder_view(&self) -> String {
        let p = Style::new().render(&self.prompt);
        let first = self.placeholder.graphemes(true).next().unwrap_or_default();
        let rest = self.placeholder.get(first.len()..).unwrap_or_default();
        let mut v = self.cursor_view(first, PLACEHOLDER);
        if self.width < 1 && ansi::width(rest) <= 1 {
            return p + &v;
        }
        let style = PLACEHOLDER.inline(true);
        if self.width > 0 {
            let width = self.width - block_width(&p) - block_width(&v);
            let rest = ansi::truncate(rest, width, "…");
            let avail = (width - block_width(&rest)).max(0);
            v.push_str(&style.render(&rest));
            v.push_str(&" ".repeat(avail.try_into().unwrap_or(0)));
        } else {
            v.push_str(&style.render(rest));
        }
        p + &v
    }
}

/// runeutil's sanitizer as the text input sets it up: tabs and line breaks
/// become spaces; invalid characters and other control characters go.
fn sanitize(runes: impl Iterator<Item = char>) -> Vec<char> {
    runes
        .filter(|&r| r != char::REPLACEMENT_CHARACTER)
        .filter_map(|r| match r {
            '\r' | '\n' | '\t' => Some(' '),
            r if r.is_control() => None,
            r => Some(r),
        })
        .collect()
}

fn width_of(runes: &[char]) -> i64 {
    i64::try_from(ansi::width(&runes.iter().collect::<String>())).unwrap_or(i64::MAX)
}

fn rune_width(r: char) -> i64 {
    i64::try_from(ansi::width(r.encode_utf8(&mut [0; 4]))).unwrap_or(0)
}

fn block_width(s: &str) -> i64 {
    i64::try_from(ansi::block_width(s)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The named keys these tests press; anything else is typed text.
    const NAMED: [&str; 11] = [
        "left",
        "right",
        "home",
        "end",
        "backspace",
        "delete",
        "ctrl+u",
        "ctrl+k",
        "ctrl+w",
        "ctrl+left",
        "ctrl+right",
    ];

    fn typed(input: &mut TextInput, keys: &[&str]) {
        for k in keys {
            let key = if let Some(name) = NAMED.iter().find(|n| *n == k) {
                Key::named(name)
            } else if *k == "alt+d" {
                let mut key = Key::text("d");
                key.alt = true;
                key
            } else {
                Key::text(k)
            };
            input.update(&key);
        }
    }

    // Not upstream's: edits as bubbles' text input edits.
    #[test]
    fn edits_as_bubbles_does() {
        let mut input = TextInput::new();
        typed(&mut input, &["a"]);
        assert_eq!(input.value(), "", "no focus, no typing");
        input.focus();
        typed(
            &mut input,
            &["ab", "c", "left", "left", "X", "end", "backspace"],
        );
        assert_eq!(input.value(), "aXb");
        typed(&mut input, &["home", "delete", "end", " d\te", "ctrl+w"]);
        assert_eq!(input.value(), "Xb d ");
        typed(&mut input, &["ctrl+left", "ctrl+k"]);
        assert_eq!(input.value(), "Xb ");
        typed(&mut input, &["home", "ctrl+right", "ctrl+u"]);
        assert_eq!(input.value(), " ");
        input.set_value("one two");
        typed(&mut input, &["home", "alt+d"]);
        assert_eq!(input.value(), " two");
        // Upstream panics here: the cursor at the second-to-last character.
        input.set_value("ab");
        typed(&mut input, &["end", "left", "alt+d"]);
        assert_eq!(input.value(), "a");
        input.char_limit = 3;
        input.set_value("abcdef");
        assert_eq!(input.value(), "abc");
        typed(&mut input, &["x"]);
        assert_eq!(input.value(), "abc");
        input.set_value("");
        typed(&mut input, &["\u{7}a\u{fffd}b"]);
        assert_eq!(input.value(), "ab");
    }

    // Not upstream's: shows as bubbles' text input shows, recorded with
    // Go 1.26.4.
    #[test]
    fn views_as_bubbles_does() {
        let mut input = TextInput::new();
        input.prompt = "  Key: ".into();
        assert_eq!(input.view(), "  Key:  ");
        input.focus();
        input.set_value("ab");
        assert_eq!(input.view(), "  Key: ab\x1b[7m \x1b[0m");
        input.set_cursor(0);
        assert_eq!(input.view(), "  Key: \x1b[7ma\x1b[0mb");
        input.width = 5;
        assert_eq!(input.view(), "  Key: \x1b[7ma\x1b[0mb    ");
        input.password = true;
        input.cursor_end();
        assert_eq!(input.view(), "  Key: **\x1b[7m \x1b[0m   ");
        input.password = false;
        input.width = 3;
        input.set_value("abcdefgh");
        input.cursor_end();
        assert_eq!(input.view(), "  Key: fgh\x1b[7m \x1b[0m");
        input.cursor_start();
        assert_eq!(input.view(), "  Key: \x1b[7ma\x1b[0mbcd");

        let mut input = TextInput::new();
        input.placeholder = "http://x/cb".into();
        input.width = 6;
        assert_eq!(
            input.view(),
            "> \x1b[38;5;240mh\x1b[0m\x1b[38;5;240mtt…\x1b[0m"
        );
        input.focus();
        input.width = 0;
        assert_eq!(
            input.view(),
            "> \x1b[7mh\x1b[0m\x1b[38;5;240mttp://x/cb\x1b[0m"
        );
    }
}
