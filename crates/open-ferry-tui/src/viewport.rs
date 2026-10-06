// Ported with how github.com/charmbracelet/bubbles scrolls a viewport (New,
// SetContent, AtTop, AtBottom, maxYOffset, visibleLines, SetYOffset,
// PageDown, PageUp, HalfPageDown, HalfPageUp, ScrollDown, ScrollUp,
// GotoBottom, Update, View, DefaultKeyMap; v1.0.0, MIT, see
// licenses/bubbles-LICENSE).
// https://github.com/charmbracelet/bubbles

//! A scrolling view of a tab's content, as the bubbles viewport every tab
//! shows its content in.
//!
//! Deviations from upstream:
//! - There is no mouse wheel scrolling: upstream's TUI never turns the
//!   mouse on, so its viewports never get wheel events either.
//! - Left and right do nothing, as upstream's do with the viewport's
//!   horizontal step left at 0; there is no horizontal scrolling.

use crate::ansi;
use crate::keys::Key;
use crate::style::Style;

/// A viewport (bubbles' `viewport.Model`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Viewport {
    pub(crate) width: i64,
    pub(crate) height: i64,
    y_offset: i64,
    lines: Vec<String>,
    longest_line_width: i64,
}

impl Viewport {
    /// `viewport.New`.
    pub(crate) fn new(width: i64, height: i64) -> Self {
        Self {
            width,
            height,
            ..Self::default()
        }
    }

    /// `SetContent`.
    pub(crate) fn set_content(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n");
        self.lines = s.split('\n').map(str::to_owned).collect();
        self.longest_line_width = self
            .lines
            .iter()
            .map(|l| len(ansi::width(l)))
            .max()
            .unwrap_or(0);
        if self.y_offset > self.line_count() - 1 {
            self.goto_bottom();
        }
    }

    fn line_count(&self) -> i64 {
        len(self.lines.len())
    }

    /// `YOffset`.
    pub(crate) fn y_offset(&self) -> i64 {
        self.y_offset
    }

    /// `AtTop`.
    pub(crate) fn at_top(&self) -> bool {
        self.y_offset <= 0
    }

    /// `AtBottom`.
    pub(crate) fn at_bottom(&self) -> bool {
        self.y_offset >= self.max_y_offset()
    }

    fn max_y_offset(&self) -> i64 {
        (self.line_count() - self.height).max(0)
    }

    fn visible_lines(&self) -> Vec<String> {
        let (h, w) = (self.height, self.width);
        let mut lines: Vec<String> = Vec::new();
        if !self.lines.is_empty() {
            let top = self.y_offset.max(0);
            let bottom = clamp(self.y_offset + h, top, self.line_count());
            let top = usize::try_from(top).unwrap_or(0);
            let bottom = usize::try_from(bottom).unwrap_or(0);
            lines = self.lines.get(top..bottom).unwrap_or_default().to_vec();
        }
        if self.longest_line_width <= w || w == 0 {
            return lines;
        }
        lines.iter().map(|l| ansi::truncate(l, w, "")).collect()
    }

    /// `SetYOffset`.
    pub(crate) fn set_y_offset(&mut self, n: i64) {
        self.y_offset = clamp(n, 0, self.max_y_offset());
    }

    /// `PageDown`.
    pub(crate) fn page_down(&mut self) {
        if !self.at_bottom() {
            self.scroll_down(self.height);
        }
    }

    /// `PageUp`.
    pub(crate) fn page_up(&mut self) {
        if !self.at_top() {
            self.scroll_up(self.height);
        }
    }

    /// `HalfPageDown`.
    pub(crate) fn half_page_down(&mut self) {
        if !self.at_bottom() {
            self.scroll_down(self.height / 2);
        }
    }

    /// `HalfPageUp`.
    pub(crate) fn half_page_up(&mut self) {
        if !self.at_top() {
            self.scroll_up(self.height / 2);
        }
    }

    /// `ScrollDown`.
    pub(crate) fn scroll_down(&mut self, n: i64) {
        if self.at_bottom() || n == 0 || self.lines.is_empty() {
            return;
        }
        self.set_y_offset(self.y_offset + n);
    }

    /// `ScrollUp`.
    pub(crate) fn scroll_up(&mut self, n: i64) {
        if self.at_top() || n == 0 || self.lines.is_empty() {
            return;
        }
        self.set_y_offset(self.y_offset - n);
    }

    /// `GotoBottom`.
    pub(crate) fn goto_bottom(&mut self) {
        self.set_y_offset(self.max_y_offset());
    }

    /// `Update` for a key press, with `DefaultKeyMap`'s bindings.
    pub(crate) fn update(&mut self, key: &Key) {
        match key.string().as_str() {
            "pgdown" | " " | "f" => self.page_down(),
            "pgup" | "b" => self.page_up(),
            "d" | "ctrl+d" => self.half_page_down(),
            "u" | "ctrl+u" => self.half_page_up(),
            "down" | "j" => self.scroll_down(1),
            "up" | "k" => self.scroll_up(1),
            _ => {}
        }
    }

    /// `View`.
    pub(crate) fn view(&self) -> String {
        let (w, h) = (self.width, self.height);
        Style::new()
            .width(w)
            .height(h)
            .max_height(h)
            .max_width(w)
            .render(&self.visible_lines().join("\n"))
    }
}

/// A length as an `i64`, as Go's `int` holds it.
fn len(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// bubbles' `clamp`: `v` within `low` and `high`, which swap if `high` is
/// the lower.
fn clamp(v: i64, low: i64, high: i64) -> i64 {
    let (low, high) = if high < low { (high, low) } else { (low, high) };
    v.max(low).min(high)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Not upstream's: scrolls as bubbles' viewport scrolls, keys and all.
    #[test]
    fn scrolls_as_bubbles_does() {
        let mut vp = Viewport::new(3, 4);
        vp.set_content(&numbered(10));
        assert!(vp.at_top());
        assert_eq!(vp.view(), "1  \n2  \n3  \n4  ");
        vp.update(&Key::named("pgdown"));
        assert_eq!(vp.y_offset(), 4);
        vp.update(&Key::text("d"));
        assert_eq!(vp.y_offset(), 6);
        assert!(vp.at_bottom());
        vp.update(&Key::text("j"));
        assert_eq!(vp.y_offset(), 6);
        assert_eq!(vp.view(), "7  \n8  \n9  \n10 ");
        vp.update(&Key::named("up"));
        vp.update(&Key::text("u"));
        assert_eq!(vp.y_offset(), 3);
        vp.update(&Key::text("b"));
        assert_eq!(vp.y_offset(), 0);
        vp.update(&Key::text("h"));
        assert_eq!(vp.y_offset(), 0);
        // Content that shrinks below the offset goes to its bottom.
        vp.set_y_offset(6);
        vp.set_content(&numbered(5));
        assert_eq!(vp.y_offset(), 1);
        // An offset past the new content's bottom but within its lines
        // stays, as upstream's does.
        vp.set_content("abcdef\nxy");
        assert_eq!(vp.y_offset(), 1);
        assert_eq!(vp.view(), "xy \n   \n   \n   ");
        // Lines wider than the viewport are cut.
        vp.set_y_offset(0);
        assert_eq!(vp.y_offset(), 0);
        assert_eq!(vp.view(), "abc\nxy \n   \n   ");
        vp.goto_bottom();
        assert_eq!(vp.y_offset(), 0);
        assert_eq!(Viewport::default().view(), "");
    }
}
