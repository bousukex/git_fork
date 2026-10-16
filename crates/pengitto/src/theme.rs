//! Fork-inspired light/dark themes.
//!
//! egui's `Visuals` cover standard widgets; everything we paint ourselves
//! (graph lanes, ref badges, diff backgrounds) reads colors from [`Palette`].

use egui::{Color32, CornerRadius, Stroke, Style, Theme, Visuals};

#[derive(Debug, Clone)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub sidebar_bg: Color32,
    pub toolbar_bg: Color32,
    pub separator: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    pub accent: Color32,
    pub selection_bg: Color32,
    pub selection_bg_unfocused: Color32,
    pub row_hover: Color32,
    pub diff_add_bg: Color32,
    pub diff_add_strong: Color32,
    pub diff_del_bg: Color32,
    pub diff_del_strong: Color32,
    pub diff_hunk_bg: Color32,
    pub diff_gutter: Color32,
    pub badge_local: Color32,
    pub badge_remote: Color32,
    pub badge_tag: Color32,
    pub badge_head: Color32,
    pub badge_stash: Color32,
    pub badge_text: Color32,
    pub status_added: Color32,
    pub status_modified: Color32,
    pub status_deleted: Color32,
    pub status_renamed: Color32,
    pub status_untracked: Color32,
    pub status_conflict: Color32,
    pub lanes: [Color32; 10],
    /// Code colors by token kind, in [`pengitto_core::highlight::Token`] order:
    /// comment, string, keyword, number, type, function, constant,
    /// punctuation.
    pub syntax: [Color32; 8],
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        dark: false,
        bg: hex(0xFFFFFF),
        sidebar_bg: hex(0xF4F5F7),
        toolbar_bg: hex(0xECEDF0),
        separator: hex(0xD9DBE0),
        text: hex(0x1E1F22),
        text_weak: hex(0x6B6F76),
        accent: hex(0x2F6FDB),
        selection_bg: hex(0xCFE1FB),
        selection_bg_unfocused: hex(0xE3E6EB),
        row_hover: hex(0xEEF3FB),
        diff_add_bg: hex(0xE6F6E6),
        diff_add_strong: hex(0xB8E6B8),
        diff_del_bg: hex(0xFBE9E9),
        diff_del_strong: hex(0xF4C0C0),
        diff_hunk_bg: hex(0xEEF1F6),
        diff_gutter: hex(0x9A9EA6),
        badge_local: hex(0x3B7DDD),
        badge_remote: hex(0x7A5CC7),
        badge_tag: hex(0xD18B1F),
        badge_head: hex(0x2E9E5B),
        badge_stash: hex(0x8A8F98),
        badge_text: hex(0xFFFFFF),
        status_added: hex(0x2E9E5B),
        status_modified: hex(0xD18B1F),
        status_deleted: hex(0xD64545),
        status_renamed: hex(0x3B7DDD),
        status_untracked: hex(0x8A8F98),
        status_conflict: hex(0xC2185B),
        lanes: [
            hex(0x3B7DDD),
            hex(0xE8833A),
            hex(0x2E9E5B),
            hex(0x9B59B6),
            hex(0xD64545),
            hex(0x17A2B8),
            hex(0xC9A227),
            hex(0xE0569B),
            hex(0x6D7F2B),
            hex(0x5C6BC0),
        ],
        syntax: [hex(0x6E7781), hex(0x0A3069), hex(0xCF222E), hex(0x0550AE), hex(0x953800), hex(0x8250DF), hex(0x0550AE), hex(0x57606A)],
    };

    pub const DARK: Palette = Palette {
        dark: true,
        bg: hex(0x1F2023),
        sidebar_bg: hex(0x26272B),
        toolbar_bg: hex(0x2C2D32),
        separator: hex(0x3A3B41),
        text: hex(0xDCDDE0),
        text_weak: hex(0x8E9199),
        accent: hex(0x5B9BFF),
        selection_bg: hex(0x274B7A),
        selection_bg_unfocused: hex(0x34363C),
        row_hover: hex(0x2B3240),
        diff_add_bg: hex(0x1E3324),
        diff_add_strong: hex(0x2C5A38),
        diff_del_bg: hex(0x3A2224),
        diff_del_strong: hex(0x6A3236),
        diff_hunk_bg: hex(0x2A2E36),
        diff_gutter: hex(0x6F737B),
        badge_local: hex(0x3A73C9),
        badge_remote: hex(0x7058B8),
        badge_tag: hex(0xB57A1C),
        badge_head: hex(0x2A8A51),
        badge_stash: hex(0x5F636B),
        badge_text: hex(0xF4F5F7),
        status_added: hex(0x4CC27A),
        status_modified: hex(0xE0A040),
        status_deleted: hex(0xEF6B6B),
        status_renamed: hex(0x6AA5FF),
        status_untracked: hex(0x8E9199),
        status_conflict: hex(0xF06292),
        lanes: [
            hex(0x5B9BFF),
            hex(0xF39A55),
            hex(0x4CC27A),
            hex(0xB37FD0),
            hex(0xEF6B6B),
            hex(0x3CC3D6),
            hex(0xE0C050),
            hex(0xF27BB5),
            hex(0x9DB04A),
            hex(0x8794E0),
        ],
        syntax: [hex(0x8B949E), hex(0xA5D6FF), hex(0xFF7B72), hex(0x79C0FF), hex(0xFFA657), hex(0xD2A8FF), hex(0x79C0FF), hex(0x9DA5B0)],
    };

    pub fn syntax_color(&self, token: pengitto_core::highlight::Token) -> Color32 {
        use pengitto_core::highlight::Token;
        self.syntax[match token {
            Token::Comment => 0,
            Token::String => 1,
            Token::Keyword => 2,
            Token::Number => 3,
            Token::Type => 4,
            Token::Function => 5,
            Token::Constant => 6,
            Token::Punctuation => 7,
        }]
    }

    pub fn get(dark: bool) -> &'static Palette {
        if dark { &Self::DARK } else { &Self::LIGHT }
    }

    pub fn of(ctx: &egui::Context) -> &'static Palette {
        Self::get(ctx.theme() == Theme::Dark)
    }

    pub fn lane(&self, index: usize) -> Color32 {
        self.lanes[index % self.lanes.len()]
    }
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.extreme_bg_color = if p.dark { hex(0x18191B) } else { hex(0xFFFFFF) };
    v.faint_bg_color = p.sidebar_bg;
    v.code_bg_color = p.diff_hunk_bg;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.selection_bg;
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.window_stroke = Stroke::new(1.0, p.separator);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.text_edit_bg_color = Some(v.extreme_bg_color);
    v.ime_composition.active_underline_stroke = Stroke::new(2.0, p.accent);
    v.indent_has_left_vline = false;

    let radius = CornerRadius::same(5);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    for (wv, fill) in [
        (&mut w.inactive, if p.dark { hex(0x34363C) } else { hex(0xFFFFFF) }),
        (&mut w.hovered, if p.dark { hex(0x3D4048) } else { hex(0xF2F4F7) }),
        (&mut w.active, if p.dark { hex(0x464A53) } else { hex(0xE4E8EE) }),
        (&mut w.open, if p.dark { hex(0x3D4048) } else { hex(0xEEF1F5) }),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.corner_radius = radius;
        wv.bg_stroke = Stroke::new(1.0, p.separator);
        wv.fg_stroke = Stroke::new(1.0, p.text);
    }
    // Buttons fill with `weak_bg_fill`; slider rails and check boxes with
    // `bg_fill`, which must stand out from a white window.
    if !p.dark {
        w.inactive.bg_fill = hex(0xE1E5EA);
    }
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    v
}

fn tune_style(style: &mut Style) {
    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(16.0, FontFamily::Name(crate::fonts::BOLD.into()))),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.interact_size.y = 22.0;
    style.interaction.selectable_labels = false;
}

/// Installs both themes; egui picks one according to the system preference
/// (or the user's override via [`egui::Context::set_theme`]).
pub fn install(ctx: &egui::Context) {
    for (theme, palette) in [(Theme::Light, &Palette::LIGHT), (Theme::Dark, &Palette::DARK)] {
        ctx.style_mut_of(theme, |style| {
            style.visuals = visuals(palette);
            tune_style(style);
        });
    }
}
