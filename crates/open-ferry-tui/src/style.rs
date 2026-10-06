// Ported with how github.com/charmbracelet/lipgloss renders a style
// (Style.Render, applyBorder, applyMargins, alignTextHorizontal,
// alignTextVertical, JoinHorizontal; v1.1.0, MIT, see
// licenses/lipgloss-LICENSE), how github.com/muesli/termenv writes SGR
// (Style.Styled; v0.16.0, MIT, see licenses/termenv-LICENSE) and how
// github.com/charmbracelet/x/cellbuf wraps styled text (Wrap; v0.0.15, MIT,
// see licenses/charmbracelet-x-LICENSE).
// https://github.com/charmbracelet/lipgloss
// https://github.com/muesli/termenv
// https://github.com/charmbracelet/x

//! Text styles, as upstream builds them with lipgloss.
//!
//! [`Style::render`] writes text with the same SGR sequences, padding,
//! wrapping, borders and margins lipgloss writes, so each view comes out
//! as upstream's does, line for line.
//!
//! Only what the views use is here: bold, italic and reverse text, colours,
//! width, height, the maximum width and height, padding, a bottom margin,
//! inline rendering and rounded or normal borders with a colour. Text is
//! always left- and top-aligned, as upstream's views align it.
//!
//! Deviations from upstream:
//! - Colours are always written in 24-bit (or 256-colour, for the colours
//!   upstream gives as numbers), as lipgloss writes them on a true-colour
//!   terminal. lipgloss reads the terminal's colour profile and writes
//!   fewer colours, or none, on terminals that support fewer;
//!   crossterm-backed ratatui leaves that to the terminal.

use crate::ansi::{self, Pen, Token};

/// A colour, as `lipgloss.Color` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Color {
    /// A 24-bit colour (`"#RRGGBB"`).
    Rgb(u8, u8, u8),
    /// An ANSI 256-colour palette index (`"240"`).
    Ansi(u8),
}

impl Color {
    /// termenv's SGR parameters for this colour, from a base of 30
    /// (foreground) or 40 (background).
    fn params(self, base: u8) -> String {
        match self {
            Self::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
            Self::Ansi(n) if n < 8 => (base + n).to_string(),
            Self::Ansi(n) if n < 16 => (base + 60 + n - 8).to_string(),
            Self::Ansi(n) => format!("{};5;{n}", base + 8),
        }
    }
}

/// The characters a border is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Border {
    top: &'static str,
    bottom: &'static str,
    left: &'static str,
    right: &'static str,
    top_left: &'static str,
    top_right: &'static str,
    bottom_left: &'static str,
    bottom_right: &'static str,
}

/// lipgloss's `NormalBorder`.
pub(crate) const NORMAL_BORDER: Border = Border {
    top: "─",
    bottom: "─",
    left: "│",
    right: "│",
    top_left: "┌",
    top_right: "┐",
    bottom_left: "└",
    bottom_right: "┘",
};

/// lipgloss's `RoundedBorder`.
pub(crate) const ROUNDED_BORDER: Border = Border {
    top: "─",
    bottom: "─",
    left: "│",
    right: "│",
    top_left: "╭",
    top_right: "╮",
    bottom_left: "╰",
    bottom_right: "╯",
};

/// A lipgloss style. Each setter marks its property as set, as lipgloss
/// does, which matters: a style with nothing set renders text untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Style {
    bold: Option<bool>,
    italic: Option<bool>,
    reverse: Option<bool>,
    fg: Option<Color>,
    bg: Option<Color>,
    width: Option<i64>,
    height: Option<i64>,
    max_width: Option<i64>,
    max_height: Option<i64>,
    /// Top, right, bottom and left padding.
    padding: [Option<i64>; 4],
    margin_bottom: Option<i64>,
    inline: Option<bool>,
    border: Option<Border>,
    /// Which sides have a border: top, right, bottom and left.
    border_sides: [Option<bool>; 4],
    border_fg: Option<Color>,
}

impl Style {
    /// lipgloss's `NewStyle`.
    pub(crate) const fn new() -> Self {
        Self {
            bold: None,
            italic: None,
            reverse: None,
            fg: None,
            bg: None,
            width: None,
            height: None,
            max_width: None,
            max_height: None,
            padding: [None; 4],
            margin_bottom: None,
            inline: None,
            border: None,
            border_sides: [None; 4],
            border_fg: None,
        }
    }

    pub(crate) const fn bold(mut self, on: bool) -> Self {
        self.bold = Some(on);
        self
    }

    pub(crate) const fn italic(mut self, on: bool) -> Self {
        self.italic = Some(on);
        self
    }

    pub(crate) const fn reverse(mut self, on: bool) -> Self {
        self.reverse = Some(on);
        self
    }

    pub(crate) const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub(crate) const fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub(crate) const fn width(mut self, width: i64) -> Self {
        self.width = Some(width);
        self
    }

    pub(crate) const fn height(mut self, height: i64) -> Self {
        self.height = Some(height);
        self
    }

    pub(crate) const fn max_width(mut self, width: i64) -> Self {
        self.max_width = Some(width);
        self
    }

    pub(crate) const fn max_height(mut self, height: i64) -> Self {
        self.max_height = Some(height);
        self
    }

    /// lipgloss's `Padding(vertical, horizontal)`.
    pub(crate) const fn padding(mut self, vertical: i64, horizontal: i64) -> Self {
        self.padding = [
            Some(vertical),
            Some(horizontal),
            Some(vertical),
            Some(horizontal),
        ];
        self
    }

    pub(crate) const fn padding_left(mut self, n: i64) -> Self {
        self.padding[3] = Some(n);
        self
    }

    pub(crate) const fn padding_right(mut self, n: i64) -> Self {
        self.padding[1] = Some(n);
        self
    }

    pub(crate) const fn padding_bottom(mut self, n: i64) -> Self {
        self.padding[2] = Some(n);
        self
    }

    pub(crate) const fn margin_bottom(mut self, n: i64) -> Self {
        self.margin_bottom = Some(n);
        self
    }

    pub(crate) const fn inline(mut self, on: bool) -> Self {
        self.inline = Some(on);
        self
    }

    /// lipgloss's `Border(border)`, which draws every side.
    pub(crate) const fn border(mut self, border: Border) -> Self {
        self.border = Some(border);
        self
    }

    /// lipgloss's `BorderStyle(border)`.
    pub(crate) const fn border_style(self, border: Border) -> Self {
        self.border(border)
    }

    pub(crate) const fn border_bottom(mut self, on: bool) -> Self {
        self.border_sides[2] = Some(on);
        self
    }

    pub(crate) const fn border_fg(mut self, color: Color) -> Self {
        self.border_fg = Some(color);
        self
    }

    fn has_props(&self) -> bool {
        *self != Self::new()
    }

    /// lipgloss's `Style.Render`.
    pub(crate) fn render(&self, text: &str) -> String {
        if !self.has_props() {
            return convert_tabs(text);
        }
        let get = |v: Option<i64>| v.unwrap_or(0);
        let width = get(self.width);
        let height = get(self.height);
        let [top_pad, right_pad, bottom_pad, left_pad] = self.padding.map(get);
        let inline = self.inline.unwrap_or(false);
        let reverse = self.reverse.unwrap_or(false);

        let mut te: Vec<String> = Vec::new();
        let mut te_whitespace: Vec<String> = Vec::new();
        if self.bold.unwrap_or(false) {
            te.push("1".into());
        }
        if self.italic.unwrap_or(false) {
            te.push("3".into());
        }
        if reverse {
            te_whitespace.push("7".into());
            te.push("7".into());
        }
        if let Some(fg) = self.fg {
            te.push(fg.params(30));
            if reverse {
                te_whitespace.push(fg.params(30));
            }
        }
        if let Some(bg) = self.bg {
            te.push(bg.params(40));
            te_whitespace.push(bg.params(40));
        }

        let mut s = convert_tabs(text).replace("\r\n", "\n");
        if inline {
            s = s.replace('\n', "");
        }
        if !inline && width > 0 {
            s = wrap(&s, width - left_pad - right_pad);
        }
        s = s
            .split('\n')
            .map(|line| styled(&te, line))
            .collect::<Vec<_>>()
            .join("\n");

        if !inline {
            if left_pad > 0 {
                s = pad(&s, left_pad, &te_whitespace, true);
            }
            if right_pad > 0 {
                s = pad(&s, right_pad, &te_whitespace, false);
            }
            if top_pad > 0 {
                s = "\n".repeat(count(top_pad)) + &s;
            }
            if bottom_pad > 0 {
                s.push_str(&"\n".repeat(count(bottom_pad)));
            }
        }

        if height > 0 {
            let lines = i64::try_from(s.matches('\n').count() + 1).unwrap_or(i64::MAX);
            if height >= lines {
                s.push_str(&"\n".repeat(count(height - lines)));
            }
        }

        if s.contains('\n') || width != 0 {
            s = align_left(&s, width, &te_whitespace);
        }

        if !inline {
            s = self.apply_border(&s);
            s = self.apply_margins(&s);
        }

        if let Some(max) = self.max_width.filter(|&m| m > 0) {
            s = s
                .split('\n')
                .map(|line| ansi::truncate(line, max, ""))
                .collect::<Vec<_>>()
                .join("\n");
        }
        if let Some(max) = self.max_height.filter(|&m| m > 0) {
            s = s
                .split('\n')
                .take(count(max))
                .collect::<Vec<_>>()
                .join("\n");
        }
        s
    }

    /// lipgloss's `applyBorder`.
    fn apply_border(&self, s: &str) -> String {
        let Some(mut border) = self.border else {
            return s.to_owned();
        };
        let implicit = self.border_sides.iter().all(Option::is_none);
        let [has_top, has_right, has_bottom, has_left] = if implicit {
            [true; 4]
        } else {
            self.border_sides.map(|side| side.unwrap_or(false))
        };
        if !(has_top || has_right || has_bottom || has_left) {
            return s.to_owned();
        }
        let lines: Vec<&str> = s.split('\n').collect();
        let mut width = lines.iter().map(|l| ansi::width(l)).max().unwrap_or(0);
        if has_left {
            width += ansi::width(border.left);
        }
        if has_top {
            if !has_left {
                border.top_left = "";
            }
            if !has_right {
                border.top_right = "";
            }
        }
        if has_bottom {
            if !has_left {
                border.bottom_left = "";
            }
            if !has_right {
                border.bottom_right = "";
            }
        }
        let paint = |text: &str| match self.border_fg {
            Some(fg) => styled(&[fg.params(30)], text),
            None => text.to_owned(),
        };
        let mut out = String::new();
        if has_top {
            out.push_str(&paint(&horizontal_edge(
                border.top_left,
                border.top,
                border.top_right,
                width,
            )));
            out.push('\n');
        }
        for (i, line) in lines.iter().enumerate() {
            if has_left {
                out.push_str(&paint(border.left));
            }
            out.push_str(line);
            if has_right {
                out.push_str(&paint(border.right));
            }
            if i + 1 < lines.len() {
                out.push('\n');
            }
        }
        if has_bottom {
            out.push('\n');
            out.push_str(&paint(&horizontal_edge(
                border.bottom_left,
                border.bottom,
                border.bottom_right,
                width,
            )));
        }
        out
    }

    /// lipgloss's `applyMargins`, for the bottom margin the views use.
    fn apply_margins(&self, s: &str) -> String {
        let mut s = s.to_owned();
        let bottom = self.margin_bottom.unwrap_or(0);
        if bottom > 0 {
            let spaces = " ".repeat(ansi::block_width(&s));
            s.push_str(&format!("\n{spaces}").repeat(count(bottom)));
        }
        s
    }
}

/// A count from a lipgloss int, with negatives as zero.
fn count(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

/// lipgloss's `maybeConvertTabs`, with its default tab width of 4.
fn convert_tabs(s: &str) -> String {
    s.replace('\t', "    ")
}

/// termenv's `Style.Styled`: the text between an SGR sequence and a reset,
/// or as it is when there is nothing to set.
fn styled(params: &[String], text: &str) -> String {
    if params.is_empty() {
        return text.to_owned();
    }
    format!("\x1b[{}m{text}\x1b[0m", params.join(";"))
}

/// lipgloss's `padLeft` and `padRight`.
fn pad(s: &str, n: i64, params: &[String], left: bool) -> String {
    let spaces = styled(params, &" ".repeat(count(n)));
    s.split('\n')
        .map(|line| {
            if left {
                format!("{spaces}{line}")
            } else {
                format!("{line}{spaces}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// lipgloss's `alignTextHorizontal` with `Left`: pads each line to the
/// widest, or to `width` if that is wider.
fn align_left(s: &str, width: i64, params: &[String]) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let widest = lines.iter().map(|l| ansi::width(l)).max().unwrap_or(0);
    let width = count(width);
    lines
        .iter()
        .map(|line| {
            let w = ansi::width(line);
            let mut short = widest - w;
            short += width.saturating_sub(short + w);
            if short > 0 {
                format!("{line}{}", styled(params, &" ".repeat(short)))
            } else {
                (*line).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// lipgloss's `renderHorizontalEdge`.
fn horizontal_edge(left: &str, middle: &str, right: &str, width: usize) -> String {
    let middle = if middle.is_empty() { " " } else { middle };
    let left_width = ansi::width(left);
    let right_width = ansi::width(right);
    let runes: Vec<char> = middle.chars().collect();
    let mut out = String::from(left);
    let mut i = left_width + right_width;
    let mut j = 0;
    while i < width + right_width {
        let Some(&r) = runes.get(j) else { break };
        out.push(r);
        j = (j + 1) % runes.len();
        // A zero-width edge character would never end the edge.
        i += runes
            .get(j)
            .map_or(1, |r| ansi::width(r.encode_utf8(&mut [0; 4])))
            .max(1);
    }
    out.push_str(right);
    out
}

/// lipgloss's `JoinHorizontal` with `Top`.
pub(crate) fn join_horizontal(blocks: &[&str]) -> String {
    match blocks {
        [] => return String::new(),
        [one] => return (*one).to_owned(),
        _ => {}
    }
    let blocks: Vec<(Vec<&str>, usize)> = blocks
        .iter()
        .map(|b| {
            let lines: Vec<&str> = b.split('\n').collect();
            let widest = lines.iter().map(|l| ansi::width(l)).max().unwrap_or(0);
            (lines, widest)
        })
        .collect();
    let height = blocks.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    let mut out = String::new();
    for i in 0..height {
        for (lines, widest) in &blocks {
            let line = lines.get(i).copied().unwrap_or("");
            out.push_str(line);
            out.push_str(&" ".repeat(widest.saturating_sub(ansi::width(line))));
        }
        if i + 1 < height {
            out.push('\n');
        }
    }
    out
}

/// cellbuf's `Wrap(s, limit, "")`: wraps styled text at spaces and
/// hyphens, breaking words longer than the limit, and closes and reopens
/// the current style around each break.
pub(crate) fn wrap(s: &str, limit: i64) -> String {
    if s.is_empty() {
        return String::new();
    }
    let Ok(limit) = usize::try_from(limit) else {
        return s.to_owned();
    };
    if limit < 1 {
        return s.to_owned();
    }
    let mut w = Wrapper {
        limit,
        buf: String::new(),
        word: String::new(),
        space: String::new(),
        style: Pen::default(),
        cur_style: Pen::default(),
        cur_width: 0,
        word_len: 0,
    };
    for token in ansi::tokens(s) {
        match token {
            Token::Control("\t") => {
                w.add_word();
                w.space.push('\t');
            }
            Token::Control("\n") => {
                w.flush_trailing_space();
                w.add_word();
                w.add_newline();
            }
            Token::Escape(seq) => {
                w.style.apply(seq);
                w.word.push_str(seq);
            }
            Token::Control(c) | Token::Grapheme(c, 0) => w.word.push_str(c),
            Token::Grapheme(g, width) => {
                if g == " " && w.style.is_blank() {
                    w.add_word();
                    w.space.push(' ');
                    continue;
                }
                if g == "-" {
                    w.add_space();
                    if w.cur_width + w.word_len + width <= w.limit {
                        w.add_word();
                        w.buf.push_str(g);
                        w.cur_width += width;
                        continue;
                    }
                }
                if w.word_len + width > w.limit {
                    w.add_word();
                }
                w.word.push_str(g);
                w.word_len += width;
                if w.cur_width + w.word_len + w.space.len() > w.limit {
                    w.add_newline();
                }
            }
        }
    }
    w.flush_trailing_space();
    w.add_word();
    if !w.cur_style.is_empty() {
        w.buf.push_str("\x1b[m");
    }
    w.buf
}

/// The state of a [`wrap`].
struct Wrapper {
    limit: usize,
    buf: String,
    word: String,
    space: String,
    style: Pen,
    cur_style: Pen,
    cur_width: usize,
    word_len: usize,
}

impl Wrapper {
    fn add_space(&mut self) {
        self.cur_width += self.space.len();
        self.buf.push_str(&self.space);
        self.space.clear();
    }

    fn add_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        self.cur_style = self.style;
        self.add_space();
        self.cur_width += self.word_len;
        self.buf.push_str(&self.word);
        self.word.clear();
        self.word_len = 0;
    }

    fn add_newline(&mut self) {
        if !self.cur_style.is_empty() {
            self.buf.push_str("\x1b[m");
        }
        self.buf.push('\n');
        if !self.cur_style.is_empty() {
            self.buf.push_str(&self.cur_style.sequence());
        }
        self.cur_width = 0;
        self.space.clear();
    }

    /// Keeps the spaces before a line break or the end if they fit, as
    /// cellbuf does when no word follows them.
    fn flush_trailing_space(&mut self) {
        if self.word_len == 0 {
            if self.cur_width + self.space.len() > self.limit {
                self.cur_width = 0;
            } else {
                self.buf.push_str(&self.space);
            }
            self.space.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIOLET: Color = Color::Rgb(0x7c, 0x3a, 0xed);

    // Not upstream's: renders as lipgloss v1.1.0 rendered the same styles
    // on a true-colour profile, recorded with Go 1.26.4.
    #[test]
    fn renders_as_lipgloss_does() {
        assert_eq!(Style::new().render("a\tb"), "a    b");
        assert_eq!(Style::new().bold(false).render("ab"), "ab");
        assert_eq!(
            Style::new()
                .bold(true)
                .fg(Color::Rgb(255, 255, 255))
                .bg(VIOLET)
                .padding(0, 2)
                .render("Tab"),
            "\x1b[48;2;124;58;237m  \x1b[0m\x1b[1;38;2;255;255;255;48;2;124;58;237mTab\x1b[0m\x1b[48;2;124;58;237m  \x1b[0m"
        );
        assert_eq!(
            Style::new().fg(Color::Ansi(240)).render("x"),
            "\x1b[38;5;240mx\x1b[0m"
        );
        assert_eq!(
            Style::new().bold(true).margin_bottom(1).render("Title"),
            "\x1b[1mTitle\x1b[0m\n     "
        );
        assert_eq!(Style::new().width(5).render("ab"), "ab   ");
        assert_eq!(Style::new().width(4).render("ab cd ef"), "ab  \ncd  \nef  ");
        assert_eq!(
            Style::new()
                .border(ROUNDED_BORDER)
                .padding(0, 1)
                .width(6)
                .render("ab"),
            "╭──────╮\n│ ab   │\n╰──────╯"
        );
        assert_eq!(
            Style::new()
                .border_bottom(true)
                .border_style(NORMAL_BORDER)
                .border_fg(Color::Ansi(240))
                .render("abc"),
            "abc\n\x1b[38;5;240m───\x1b[0m"
        );
        assert_eq!(
            Style::new()
                .width(3)
                .height(3)
                .max_width(3)
                .max_height(3)
                .render("abcdef\n1\n2\n3"),
            "abc\ndef\n1  "
        );
        assert_eq!(
            Style::new().reverse(true).inline(true).render("a\nb"),
            "\x1b[7mab\x1b[0m"
        );
    }

    // Not upstream's: wraps as cellbuf v0.0.15's Wrap wrapped the same
    // text, recorded with Go 1.26.4.
    #[test]
    fn wraps_as_cellbuf_does() {
        assert_eq!(wrap("", 5), "");
        assert_eq!(wrap("abc", 0), "abc");
        assert_eq!(wrap("hello world", 5), "hello\nworld");
        assert_eq!(wrap("abcdefgh", 3), "abc\ndef\ngh");
        assert_eq!(wrap("one-two", 4), "one-\ntwo");
        assert_eq!(wrap("a  \nb", 5), "a  \nb");
        assert_eq!(
            wrap("\x1b[1mabcd\x1b[0m", 2),
            "\x1b[1mab\x1b[m\n\x1b[1mcd\x1b[0m"
        );
    }

    // Not upstream's: blocks join as lipgloss's JoinHorizontal joins them.
    #[test]
    fn joins_as_lipgloss_does() {
        assert_eq!(join_horizontal(&["a\nbb", " ", "c"]), "a  c\nbb  ");
        assert_eq!(join_horizontal(&["x"]), "x");
    }
}
