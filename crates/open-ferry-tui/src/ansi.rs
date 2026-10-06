// Ported with how github.com/charmbracelet/x/ansi measures, truncates and
// strips styled text (StringWidth, Truncate, Strip; v0.11.6, MIT, see
// licenses/charmbracelet-x-LICENSE), how github.com/charmbracelet/x/cellbuf
// reads SGR parameters (ReadStyle; v0.0.15, MIT, see
// licenses/charmbracelet-x-LICENSE), and how
// github.com/charmbracelet/bubbletea's standard renderer puts a view on the
// screen (flush; v1.3.10, MIT, see licenses/bubbletea-LICENSE).
// https://github.com/charmbracelet/x
// https://github.com/charmbracelet/bubbletea

//! Styled text: the terminal escape sequences the views are written in, and
//! how they reach a ratatui buffer.
//!
//! Upstream's views are strings with SGR sequences in them, built by
//! lipgloss, and Bubble Tea writes them to the terminal line by line. The
//! views here are built the same way (see `style`), so their layout follows
//! upstream's cell for cell; [`render`] then plays a view into a ratatui
//! [`Buffer`] as Bubble Tea's renderer would put it on the screen:
//!
//! - the view is split into lines and only its last `height` lines are
//!   kept;
//! - each line is truncated to the width;
//! - SGR sequences set the pen, which carries from one line to the next, as
//!   it does on a terminal;
//! - a line shorter than the width is cleared to its end with the pen's
//!   background, as `EraseLineRight` clears it; rows below the view are
//!   blank.
//!
//! Text is cut into grapheme clusters and measured as ratatui measures a
//! cell, so a view's widths match the cells it fills.
//!
//! Deviations from upstream:
//! - x/ansi takes each ASCII byte as its own cluster, and so splits a
//!   combining mark from the letter before it; here they stay one cluster,
//!   as ratatui keeps them. Their width is the same.
//! - Escape sequences other than SGR draw nothing here; on a terminal they
//!   would do whatever they ask. The views carry no others, and server text
//!   is cleaned of control characters before it reaches a view.

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

const ESC: char = '\x1b';

/// A piece of styled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    /// A whole escape sequence, from `ESC` to its final byte.
    Escape(&'a str),
    /// A control character other than `ESC`. It takes no cells.
    Control(&'a str),
    /// A grapheme cluster and the cells it takes.
    Grapheme(&'a str, usize),
}

/// Cuts `s` into escape sequences, control characters and grapheme
/// clusters.
pub(crate) fn tokens(s: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(first) = rest.chars().next() {
        if first == ESC {
            let len = escape_len(rest);
            let (seq, tail) = rest.split_at(len);
            out.push(Token::Escape(seq));
            rest = tail;
        } else if first.is_control() {
            let (ctrl, tail) = rest.split_at(first.len_utf8());
            out.push(Token::Control(ctrl));
            rest = tail;
        } else {
            let end = rest
                .char_indices()
                .find(|&(_, c)| c.is_control())
                .map_or(rest.len(), |(i, _)| i);
            let (run, tail) = rest.split_at(end);
            for g in run.graphemes(true) {
                out.push(Token::Grapheme(g, usize::from(g.cell_width())));
            }
            rest = tail;
        }
    }
    out
}

/// The length in bytes of the escape sequence `s` starts with.
fn escape_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    match bytes.get(1) {
        // CSI: parameters and intermediates, then a final byte.
        Some(b'[') => {
            let mut i = 2;
            while let Some(&b) = bytes.get(i) {
                i += 1;
                if (0x40..=0x7e).contains(&b) {
                    return i;
                }
                if !(0x20..=0x3f).contains(&b) {
                    // Not a CSI byte: the sequence ends before it.
                    return i - 1;
                }
            }
            bytes.len()
        }
        // OSC, DCS, SOS, PM and APC run to BEL (OSC only) or ST.
        Some(&kind @ (b']' | b'P' | b'X' | b'^' | b'_')) => {
            let mut i = 2;
            while let Some(&b) = bytes.get(i) {
                if b == 0x07 && kind == b']' {
                    return i + 1;
                }
                if b == 0x1b {
                    return if bytes.get(i + 1) == Some(&b'\\') {
                        i + 2
                    } else {
                        i
                    };
                }
                i += 1;
            }
            bytes.len()
        }
        // Any other: intermediates, then one final character.
        Some(_) => {
            let mut i = 1;
            while let Some(&b) = bytes.get(i) {
                if (0x20..=0x2f).contains(&b) {
                    i += 1;
                    continue;
                }
                return s
                    .get(i..)
                    .and_then(|t| t.chars().next())
                    .map_or(i, |c| i + c.len_utf8());
            }
            bytes.len()
        }
        None => 1,
    }
}

/// The cells `s` takes, leaving out escape sequences (x/ansi's
/// `StringWidth`).
pub(crate) fn width(s: &str) -> usize {
    tokens(s)
        .into_iter()
        .map(|t| match t {
            Token::Grapheme(_, w) => w,
            _ => 0,
        })
        .sum()
}

/// The widest line of `s` (lipgloss's `Width`).
pub(crate) fn block_width(s: &str) -> usize {
    s.split('\n').map(width).max().unwrap_or(0)
}

/// `s` without its escape sequences (x/ansi's `Strip`).
#[cfg(test)]
pub(crate) fn strip(s: &str) -> String {
    tokens(s)
        .into_iter()
        .filter_map(|t| match t {
            Token::Escape(_) => None,
            Token::Control(c) | Token::Grapheme(c, _) => Some(c),
        })
        .collect()
}

/// Cuts `s` to `length` cells, ending it with `tail` when it is cut
/// (x/ansi's `Truncate`). Escape sequences after the cut are kept, so the
/// styles they close still close.
pub(crate) fn truncate(s: &str, length: i64, tail: &str) -> String {
    let sw = i64::try_from(width(s)).unwrap_or(i64::MAX);
    if sw <= length {
        return s.to_owned();
    }
    let length = length - i64::try_from(width(tail)).unwrap_or(i64::MAX);
    if length < 0 {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut cur: i64 = 0;
    let mut ignoring = false;
    for token in tokens(s) {
        match token {
            Token::Grapheme(g, w) => {
                cur += i64::try_from(w).unwrap_or(i64::MAX);
                if cur > length && !ignoring {
                    ignoring = true;
                    out.push_str(tail);
                }
                if cur > length {
                    continue;
                }
                out.push_str(g);
            }
            Token::Control(c) => {
                if !ignoring {
                    out.push_str(c);
                }
            }
            Token::Escape(e) => out.push_str(e),
        }
    }
    out
}

/// Bit flags for the attributes an SGR sequence sets.
const BOLD: u16 = 1;
const FAINT: u16 = 1 << 1;
const ITALIC: u16 = 1 << 2;
const UNDERLINE: u16 = 1 << 3;
const SLOW_BLINK: u16 = 1 << 4;
const RAPID_BLINK: u16 = 1 << 5;
const REVERSE: u16 = 1 << 6;
const CONCEAL: u16 = 1 << 7;
const STRIKETHROUGH: u16 = 1 << 8;

/// The attributes and colours SGR sequences have set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Pen {
    attrs: u16,
    fg: Option<Color>,
    bg: Option<Color>,
}

impl Pen {
    /// Whether no attribute or colour is set (cellbuf's `Style.Empty`).
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the pen leaves spaces looking blank: no reverse, background
    /// or underline (cellbuf's `hasBlankStyle` in `Wrap`).
    pub(crate) fn is_blank(&self) -> bool {
        self.attrs & (REVERSE | UNDERLINE) == 0 && self.bg.is_none()
    }

    /// Applies an escape sequence if it is SGR; others leave the pen as it
    /// is. Returns whether it was SGR.
    pub(crate) fn apply(&mut self, seq: &str) -> bool {
        let Some(params) = seq.strip_prefix("\x1b[").and_then(|p| p.strip_suffix('m')) else {
            return false;
        };
        if !params
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b';' || b == b':')
        {
            return false;
        }
        self.read(params);
        true
    }

    /// Reads SGR parameters (cellbuf's `ReadStyle`).
    fn read(&mut self, params: &str) {
        let list: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while let Some(&param) = list.get(i) {
            i += 1;
            let mut sub = param.split(':');
            let code: u16 = sub.next().and_then(|c| c.parse().ok()).unwrap_or(0);
            match code {
                0 => *self = Self::default(),
                1 => self.attrs |= BOLD,
                2 => self.attrs |= FAINT,
                3 => self.attrs |= ITALIC,
                4 => match sub.next().and_then(|s| s.parse::<u8>().ok()) {
                    Some(0) => self.attrs &= !UNDERLINE,
                    _ => self.attrs |= UNDERLINE,
                },
                5 => self.attrs |= SLOW_BLINK,
                6 => self.attrs |= RAPID_BLINK,
                7 => self.attrs |= REVERSE,
                8 => self.attrs |= CONCEAL,
                9 => self.attrs |= STRIKETHROUGH,
                22 => self.attrs &= !(BOLD | FAINT),
                23 => self.attrs &= !ITALIC,
                24 => self.attrs &= !UNDERLINE,
                25 => self.attrs &= !(SLOW_BLINK | RAPID_BLINK),
                27 => self.attrs &= !REVERSE,
                28 => self.attrs &= !CONCEAL,
                29 => self.attrs &= !STRIKETHROUGH,
                30..=37 => self.fg = Some(basic(code - 30)),
                38 => self.fg = extended(&mut sub, &list, &mut i),
                39 => self.fg = None,
                40..=47 => self.bg = Some(basic(code - 40)),
                48 => self.bg = extended(&mut sub, &list, &mut i),
                49 => self.bg = None,
                // Underline colours draw nothing here; skip their arguments.
                58 => {
                    extended(&mut sub, &list, &mut i);
                }
                90..=97 => self.fg = Some(basic(code - 90 + 8)),
                100..=107 => self.bg = Some(basic(code - 100 + 8)),
                _ => {}
            }
        }
    }

    /// An SGR sequence that sets this pen from a reset one (cellbuf's
    /// `Style.Sequence`).
    pub(crate) fn sequence(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut params: Vec<String> = Vec::new();
        for (flag, code) in [
            (BOLD, "1"),
            (FAINT, "2"),
            (ITALIC, "3"),
            (UNDERLINE, "4"),
            (SLOW_BLINK, "5"),
            (RAPID_BLINK, "6"),
            (REVERSE, "7"),
            (CONCEAL, "8"),
            (STRIKETHROUGH, "9"),
        ] {
            if self.attrs & flag != 0 {
                params.push(code.to_owned());
            }
        }
        if let Some(fg) = self.fg {
            params.push(color_params(fg, 30));
        }
        if let Some(bg) = self.bg {
            params.push(color_params(bg, 40));
        }
        format!("\x1b[{}m", params.join(";"))
    }

    /// The ratatui style that draws like this pen.
    pub(crate) fn style(&self) -> Style {
        let mut style = Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        for (flag, modifier) in [
            (BOLD, Modifier::BOLD),
            (FAINT, Modifier::DIM),
            (ITALIC, Modifier::ITALIC),
            (UNDERLINE, Modifier::UNDERLINED),
            (SLOW_BLINK, Modifier::SLOW_BLINK),
            (RAPID_BLINK, Modifier::RAPID_BLINK),
            (REVERSE, Modifier::REVERSED),
            (CONCEAL, Modifier::HIDDEN),
            (STRIKETHROUGH, Modifier::CROSSED_OUT),
        ] {
            if self.attrs & flag != 0 {
                style = style.add_modifier(modifier);
            }
        }
        style
    }

    /// The background a line is cleared with.
    fn erase_style(&self) -> Style {
        self.bg
            .map_or_else(Style::default, |bg| Style::default().bg(bg))
    }
}

/// One of the sixteen ANSI colours.
fn basic(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// The SGR parameters for a colour, from a base of 30 (foreground) or 40
/// (background).
fn color_params(color: Color, base: u8) -> String {
    let basic = |n: u8| {
        if n < 8 {
            (base + n).to_string()
        } else {
            (base + 60 + n - 8).to_string()
        }
    };
    match color {
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        Color::Indexed(n) => format!("{};5;{n}", base + 8),
        Color::Black => basic(0),
        Color::Red => basic(1),
        Color::Green => basic(2),
        Color::Yellow => basic(3),
        Color::Blue => basic(4),
        Color::Magenta => basic(5),
        Color::Cyan => basic(6),
        Color::Gray => basic(7),
        Color::DarkGray => basic(8),
        Color::LightRed => basic(9),
        Color::LightGreen => basic(10),
        Color::LightYellow => basic(11),
        Color::LightBlue => basic(12),
        Color::LightMagenta => basic(13),
        Color::LightCyan => basic(14),
        Color::White => basic(15),
        Color::Reset => (base + 9).to_string(),
    }
}

/// Reads the arguments of an extended colour (`38`, `48` or `58`), either
/// as colon sub-parameters or as the parameters that follow.
fn extended<'a>(
    sub: &mut impl Iterator<Item = &'a str>,
    list: &[&'a str],
    i: &mut usize,
) -> Option<Color> {
    let subs: Vec<&str> = sub.collect();
    let num = |s: Option<&&str>| s.and_then(|s| s.parse::<u8>().ok());
    if !subs.is_empty() {
        return match subs.first().copied() {
            Some("5") => num(subs.get(1)).map(Color::Indexed),
            Some("2") => {
                // 38:2:<colour space>:r:g:b, or 38:2:r:g:b.
                let rgb = if subs.len() >= 5 {
                    subs.get(2..5)
                } else {
                    subs.get(1..4)
                };
                rgb.and_then(|c| Some(Color::Rgb(num(c.first())?, num(c.get(1))?, num(c.get(2))?)))
            }
            _ => None,
        };
    }
    match list.get(*i).copied() {
        Some("5") => {
            let color = num(list.get(*i + 1)).map(Color::Indexed);
            *i += 2;
            color
        }
        Some("2") => {
            let color = Some(Color::Rgb(
                num(list.get(*i + 1))?,
                num(list.get(*i + 2))?,
                num(list.get(*i + 3))?,
            ));
            *i += 4;
            color
        }
        _ => None,
    }
}

/// Plays `view` into `area` of `buf` as Bubble Tea's standard renderer
/// would draw it on a terminal of that size.
pub(crate) fn render(view: &str, area: Rect, buf: &mut Buffer) {
    let width = area.width;
    let height = usize::from(area.height);
    let lines: Vec<&str> = view.split('\n').collect();
    let skip = lines.len().saturating_sub(height);
    let mut pen = Pen::default();
    for (row, line) in lines.iter().skip(skip).enumerate() {
        let Ok(row) = u16::try_from(row) else { break };
        let y = area.y + row;
        let line = truncate(line, i64::from(width), "");
        let mut x: u16 = 0;
        for token in tokens(&line) {
            match token {
                Token::Escape(seq) => {
                    pen.apply(seq);
                }
                Token::Control(_) => {}
                Token::Grapheme(g, w) => {
                    let Ok(w) = u16::try_from(w) else { break };
                    if w == 0 {
                        continue;
                    }
                    if x.saturating_add(w) > width {
                        break;
                    }
                    buf.set_stringn(area.x + x, y, g, usize::from(w), pen.style());
                    x += w;
                }
            }
        }
        if x < width {
            buf.set_style(Rect::new(area.x + x, y, width - x, 1), pen.erase_style());
        }
    }
}

/// The text of each row of `buf`, as a terminal shows it, with trailing
/// blanks and blank rows at the end left out.
#[cfg(test)]
pub(crate) fn buffer_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut rows = Vec::new();
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        let mut x = area.left();
        while x < area.right() {
            let symbol = buf[(x, y)].symbol();
            row.push_str(symbol);
            x += symbol.cell_width().max(1);
        }
        rows.push(row.trim_end().to_owned());
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let mut out = rows.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: widths as x/ansi v0.11.6's StringWidth gave them,
    // recorded with Go 1.26.4.
    #[test]
    fn measures_as_x_ansi_does() {
        for s in [
            "●", "○", "▸", "─", "╭", "│", "┌", "✎", "✓", "✗", "⚠", "✦", "⚙", "↑", "•", "→", "…",
            "é", "á",
        ] {
            assert_eq!(width(s), 1, "{s}");
        }
        for s in [
            "⏳",
            "🔑",
            "📄",
            "📊",
            "📋",
            "🔐",
            "🟧",
            "🟩",
            "仪",
            "⚠\u{fe0f}",
            "👍🏽",
            "🇺🇸",
        ] {
            assert_eq!(width(s), 2, "{s}");
        }
        for s in ["\u{301}", "\u{200d}", "\t", "\x07"] {
            assert_eq!(width(s), 0, "{s:?}");
        }
        assert_eq!(width("\x1b[1;38;2;1;2;3mab\x1b[0m"), 2);
        assert_eq!(width("e\u{301}x"), 2);
    }

    // Not upstream's: truncation as x/ansi v0.11.6's Truncate did it,
    // recorded with Go 1.26.4.
    #[test]
    fn truncates_as_x_ansi_does() {
        assert_eq!(truncate("ab仪cd", 3, ""), "ab");
        assert_eq!(truncate("\x1b[1mab仪cd\x1b[0m", 3, ""), "\x1b[1mab\x1b[0m");
        assert_eq!(truncate("a\x07bcdef", 3, ""), "a\x07bc");
        assert_eq!(truncate("abcdef\x07\x1b[0m", 3, ""), "abc\x1b[0m");
        assert_eq!(truncate("ab", 0, ""), "");
        assert_eq!(truncate("ab", 2, "…"), "ab");
        assert_eq!(truncate("abcdef", 4, "…"), "abc…");
        assert_eq!(truncate("abc", 1, "……"), "");
        assert_eq!(strip("\x1b[1mab\x1b[0m\x1b]8;;x\x07c"), "abc");
    }

    // Not upstream's: SGR sequences as cellbuf's ReadStyle reads them.
    #[test]
    fn reads_sgr_as_cellbuf_does() {
        let mut pen = Pen::default();
        assert!(pen.apply("\x1b[1;38;2;255;255;255;48;2;124;58;237m"));
        assert_eq!(
            pen.style(),
            Style::default()
                .fg(Color::Rgb(255, 255, 255))
                .bg(Color::Rgb(124, 58, 237))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(pen.sequence(), "\x1b[1;38;2;255;255;255;48;2;124;58;237m");
        assert!(!pen.is_blank());
        assert!(pen.apply("\x1b[m"));
        assert!(pen.is_empty());
        assert!(pen.apply("\x1b[38;5;240;7m"));
        assert_eq!(
            pen.style(),
            Style::default()
                .fg(Color::Indexed(240))
                .add_modifier(Modifier::REVERSED)
        );
        assert!(pen.apply("\x1b[27;39;48:2::1:2:3m"));
        assert_eq!(pen.style(), Style::default().bg(Color::Rgb(1, 2, 3)));
        assert!(!pen.apply("\x1b[2K"));
    }

    // Not upstream's: a view is drawn as Bubble Tea's renderer draws it.
    #[test]
    fn renders_as_bubble_tea_does() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 2));
        render("gone\n\x1b[48;5;4mab仪cdef\n\x1b[1mx仪", buf.area, &mut buf);
        assert_eq!(buffer_text(&buf), "ab仪cd\nx仪\n");
        assert_eq!(buf[(0, 0)].bg, Color::Indexed(4));
        // The pen carries to the next line and clears its end.
        assert_eq!(buf[(0, 1)].bg, Color::Indexed(4));
        assert!(buf[(0, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(5, 1)].bg, Color::Indexed(4));
        assert_eq!(buf[(5, 1)].symbol(), " ");
    }
}
