// Ported from CLIProxyAPI internal/tui/styles.go (the colours and styles)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The TUI's colours and styles, as upstream defines them. Upstream also
//! defines a few it never uses (`colorSecondary`, `colorBg`,
//! `sectionStyle`, `tableCellStyle`, `tableSelectedStyle` and
//! `logLevelStyle`); those aren't here.
//!
//! Deviations from upstream:
//! - [`rule`], the line the tabs draw under their headings, is empty for a
//!   negative width; upstream's `strings.Repeat` panics on one. Bubble Tea
//!   never reports a negative size, so this only guards.

use crate::style::{Color, NORMAL_BORDER, Style};

pub(crate) const COLOR_PRIMARY: Color = Color::Rgb(0x7c, 0x3a, 0xed);
pub(crate) const COLOR_SUCCESS: Color = Color::Rgb(0x22, 0xc5, 0x5e);
pub(crate) const COLOR_WARNING: Color = Color::Rgb(0xea, 0xb3, 0x08);
pub(crate) const COLOR_ERROR: Color = Color::Rgb(0xef, 0x44, 0x44);
pub(crate) const COLOR_INFO: Color = Color::Rgb(0x3b, 0x82, 0xf6);
pub(crate) const COLOR_MUTED: Color = Color::Rgb(0x6b, 0x72, 0x80);
pub(crate) const COLOR_SURFACE: Color = Color::Rgb(0x31, 0x32, 0x44);
pub(crate) const COLOR_TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
pub(crate) const COLOR_SUBTEXT: Color = Color::Rgb(0xa6, 0xad, 0xc8);
pub(crate) const COLOR_BORDER: Color = Color::Rgb(0x45, 0x47, 0x5a);
pub(crate) const COLOR_HIGHLIGHT: Color = Color::Rgb(0xf5, 0xc2, 0xe7);
pub(crate) const COLOR_WHITE: Color = Color::Rgb(0xff, 0xff, 0xff);

pub(crate) const TAB_ACTIVE: Style = Style::new()
    .bold(true)
    .fg(COLOR_WHITE)
    .bg(COLOR_PRIMARY)
    .padding(0, 2);

pub(crate) const TAB_INACTIVE: Style = Style::new()
    .fg(COLOR_SUBTEXT)
    .bg(COLOR_SURFACE)
    .padding(0, 2);

pub(crate) const TAB_BAR: Style = Style::new()
    .bg(COLOR_SURFACE)
    .padding_left(1)
    .padding_bottom(0);

pub(crate) const TITLE: Style = Style::new().bold(true).fg(COLOR_HIGHLIGHT).margin_bottom(1);

pub(crate) const SUBTITLE: Style = Style::new().fg(COLOR_SUBTEXT).italic(true);

pub(crate) const LABEL: Style = Style::new().fg(COLOR_INFO).bold(true).width(24);

pub(crate) const VALUE: Style = Style::new().fg(COLOR_TEXT);

pub(crate) const ERROR: Style = Style::new().fg(COLOR_ERROR).bold(true);

pub(crate) const SUCCESS: Style = Style::new().fg(COLOR_SUCCESS);

pub(crate) const WARNING: Style = Style::new().fg(COLOR_WARNING);

pub(crate) const STATUS_BAR: Style = Style::new()
    .fg(COLOR_SUBTEXT)
    .bg(COLOR_SURFACE)
    .padding_left(1)
    .padding_right(1);

pub(crate) const HELP: Style = Style::new().fg(COLOR_MUTED);

pub(crate) const LOG_DEBUG: Style = Style::new().fg(COLOR_MUTED);
pub(crate) const LOG_INFO: Style = Style::new().fg(COLOR_INFO);
pub(crate) const LOG_WARN: Style = Style::new().fg(COLOR_WARNING);
pub(crate) const LOG_ERROR: Style = Style::new().fg(COLOR_ERROR);

pub(crate) const TABLE_HEADER: Style = Style::new()
    .bold(true)
    .fg(COLOR_HIGHLIGHT)
    .border_bottom(true)
    .border_style(NORMAL_BORDER)
    .border_fg(COLOR_BORDER);

/// A rule `width` cells wide, as the tabs draw with `strings.Repeat("─",
/// width)`.
pub(crate) fn rule(width: i64) -> String {
    "─".repeat(usize::try_from(width).unwrap_or(0))
}
