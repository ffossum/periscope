//! The UI color palette, derived from the active syntect theme.

use ratatui::style::Color;
use syntect::highlighting::{Color as SynColor, Theme, ThemeSet};

/// UI colors derived from the active syntect theme's settings.
#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) fg: Color,
    pub(super) bg: Color,
    pub(super) darker_bg: Color,
    /// Block borders.
    pub(super) border: Color,
    /// The center separator between the two columns.
    pub(super) separator: Color,
    /// Line-number gutter for context lines.
    pub(super) gutter: Color,
    /// Hunk header foreground and background band.
    pub(super) hunk_fg: Color,
    pub(super) hunk_bg: Color,
    /// Removed-side cell background, brighter intra-line emphasis, and gutter.
    pub(super) removed_bg: Color,
    pub(super) removed_emph_bg: Color,
    pub(super) removed_gutter: Color,
    /// Added-side cell background, brighter intra-line emphasis, and gutter.
    pub(super) added_bg: Color,
    pub(super) added_emph_bg: Color,
    pub(super) added_gutter: Color,
    /// Search-match foreground (high contrast), plus the background for other
    /// matches and the currently-selected one.
    pub(super) search_fg: Color,
    pub(super) search_bg: Color,
    pub(super) search_current_bg: Color,
}

impl Palette {
    pub(super) fn from_theme(theme: &Theme) -> Self {
        let s = &theme.settings;
        let bg = s.background.map(rgb).unwrap_or((0, 0, 0));
        let fg = s.foreground.map(rgb).unwrap_or((220, 220, 220));
        let gutter = s
            .gutter_foreground
            .map(conv)
            .unwrap_or_else(|| mix(bg, fg, 0.5));

        // Themes don't define diff add/remove colors, so tint the theme
        // background toward red/green. Blending off the background keeps the
        // tints in step with light vs dark themes.
        const RED: (u8, u8, u8) = (220, 80, 80);
        const GREEN: (u8, u8, u8) = (90, 190, 110);
        const BLUE: (u8, u8, u8) = (110, 130, 250);
        // Search hits use a fixed yellow/orange standout, dark text on top.
        const YELLOW: (u8, u8, u8) = (224, 198, 92);
        const ORANGE: (u8, u8, u8) = (240, 150, 70);

        Self {
            fg: rgb_color(fg),
            bg: rgb_color(bg),
            darker_bg: mix(bg, (0, 0, 0), 0.1),
            border: gutter,
            separator: gutter,
            gutter,
            hunk_fg: rgb_color(fg),
            // A subtle band, lighter than the background and tinted blue.
            hunk_bg: mix(bg, BLUE, 0.07),
            removed_bg: mix(bg, RED, 0.14),
            removed_emph_bg: mix(bg, RED, 0.30),
            removed_gutter: rgb_color(RED),
            added_bg: mix(bg, GREEN, 0.12),
            added_emph_bg: mix(bg, GREEN, 0.30),
            added_gutter: rgb_color(GREEN),
            search_fg: rgb_color(bg),
            search_bg: rgb_color(YELLOW),
            search_current_bg: rgb_color(ORANGE),
        }
    }
}

/// Load the syntax-highlighting theme.
///
/// The Enki-Tokyo-Night `.tmTheme` is embedded at compile time so it loads
/// regardless of the working directory (periscope runs as git's `pager.diff`).
pub(super) fn load_theme() -> Theme {
    const ENKI_TOKYO_NIGHT: &str = include_str!("../../themes/Enki-Tokyo-Night.tmTheme");
    let mut cursor = std::io::Cursor::new(ENKI_TOKYO_NIGHT.as_bytes());
    ThemeSet::load_from_reader(&mut cursor).expect("bundled Enki-Tokyo-Night theme parses")
}

/// A syntect color as an `(r, g, b)` tuple.
fn rgb(c: SynColor) -> (u8, u8, u8) {
    (c.r, c.g, c.b)
}

/// A syntect color as a ratatui [`Color`].
pub(super) fn conv(c: SynColor) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// An `(r, g, b)` tuple as a ratatui [`Color`].
fn rgb_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// Linear blend of two colors; `t` is the weight given to `b`.
fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let lerp = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Color::Rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}
