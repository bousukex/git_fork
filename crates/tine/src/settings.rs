//! Appearance settings kept with the app state. Theme and zoom are egui's
//! own (stored in its memory); these are the rest.

use serde::{Deserialize, Serialize};

pub const FONT_SIZES: std::ops::RangeInclusive<f32> = 10.0..=20.0;
pub const TAB_WIDTHS: std::ops::RangeInclusive<u8> = 1..=8;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Code font size in diffs and blame.
    pub diff_font_size: f32,
    /// Columns a tab advances to in diffs and blame.
    pub tab_width: u8,
    pub syntax_highlighting: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { diff_font_size: 12.5, tab_width: 4, syntax_highlighting: true }
    }
}

impl Settings {
    /// Within the allowed ranges (stored values may come from anywhere).
    pub fn validated(self) -> Self {
        let d = Settings::default();
        Settings {
            diff_font_size: if self.diff_font_size.is_finite() {
                self.diff_font_size.clamp(*FONT_SIZES.start(), *FONT_SIZES.end())
            } else {
                d.diff_font_size
            },
            tab_width: self.tab_width.clamp(*TAB_WIDTHS.start(), *TAB_WIDTHS.end()),
            syntax_highlighting: self.syntax_highlighting,
        }
    }

    /// The part that changes how diffs and blame are prepared.
    pub fn doc_options(&self) -> DocOptions {
        DocOptions { tab_width: self.tab_width, highlight: self.syntax_highlighting }
    }
}

/// How diff and blame text is prepared (changing it rebuilds them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocOptions {
    pub tab_width: u8,
    pub highlight: bool,
}

impl Default for DocOptions {
    fn default() -> Self {
        Settings::default().doc_options()
    }
}

const KEY: &str = "tine-settings";

/// The settings in effect, for views that draw code (set by the app each
/// frame).
pub fn current(ctx: &egui::Context) -> Settings {
    ctx.data(|d| d.get_temp(egui::Id::new(KEY))).unwrap_or_default()
}

pub fn set_current(ctx: &egui::Context, settings: Settings) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(KEY), settings));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_values_are_brought_back() {
        let wild = Settings { diff_font_size: 99.0, tab_width: 0, syntax_highlighting: false }.validated();
        assert_eq!(wild, Settings { diff_font_size: 20.0, tab_width: 1, syntax_highlighting: false });
        assert_eq!(Settings { diff_font_size: f32::NAN, ..Default::default() }.validated().diff_font_size, 12.5);
    }
}
