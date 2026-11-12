use serde::{Deserialize, Serialize};

/// Editor chrome theme — colors for egui panels, dock headers,
/// toolbar, text, and borders. Serializable so a user can save/load
/// a theme file later; for now there is no persistence UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditorTheme {
    /// Main panel / viewport background.
    pub background: [f32; 3],
    /// Primary text foreground.
    pub foreground: [f32; 3],
    /// Muted text (labels, secondary info, off-state).
    pub muted_foreground: [f32; 3],
    /// Accent (selection, active tab underline, toggle on, hover highlights).
    pub accent: [f32; 3],
    /// Panel separator / border color.
    pub border: [f32; 3],
    /// Panel background (can equal `background` if you want flat; slightly
    /// different for a dock-header feel).
    pub panel: [f32; 3],
    /// Panel header background (dock title bars).
    pub panel_header: [f32; 3],
    /// Font family for the editor chrome (empty = egui default).
    pub font: String,
    /// Base font size in points.
    pub font_size: f32,
}

impl EditorTheme {
    /// Default theme — Godot 4 dark palette, adapted for egui.
    /// RGB values are in the range 0.0..=1.0.
    pub fn builtin_default() -> Self {
        Self {
            // Godot dark theme defaults (from docs/editor_theme_default.md)
            background: [0.17, 0.17, 0.17],           // #2b2b2b
            foreground: [1.0, 1.0, 1.0],              // #ffffff
            muted_foreground: [0.53, 0.53, 0.53],     // #888888
            accent: [0.36, 0.80, 0.93],               // #5cccee
            border: [0.10, 0.10, 0.10],               // #1a1a1a
            panel: [0.24, 0.24, 0.24],                // #3c3c3c
            panel_header: [0.14, 0.14, 0.14],         // #242424
            font: String::new(),                       // egui default
            font_size: 12.0,
        }
    }

    /// Fills every colour/size the egui `Visuals` and text styles derive from
    /// this theme. Call whenever the theme changes (cheap; egui diffs styles).
    pub fn apply_to_egui(&self, ctx: &egui::Context) {
        let mut v = egui::Visuals::dark();
        let fg = self.foreground_color();
        let muted = self.muted_color();
        let accent = self.accent_color();
        let border = self.border_color();
        let panel = self.panel_color();
        let bg = self.background_color();
        let header = self.panel_header_color();
        v.panel_fill = panel;
        v.window_fill = panel;
        v.extreme_bg_color = bg;
        v.faint_bg_color = header;
        v.code_bg_color = bg;
        v.window_stroke = egui::Stroke::new(1.0_f32, border);
        v.hyperlink_color = accent;
        v.selection.bg_fill = accent.linear_multiply(0.45);
        v.selection.stroke = egui::Stroke::new(1.0_f32, accent);
        v.widgets.noninteractive.bg_fill = panel;
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, border);
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, fg);
        v.widgets.inactive.bg_fill = header;
        v.widgets.inactive.weak_bg_fill = header;
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, fg);
        v.widgets.hovered.bg_fill = header.linear_multiply(1.4);
        v.widgets.hovered.weak_bg_fill = header.linear_multiply(1.4);
        v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5_f32, accent);
        v.widgets.active.bg_fill = accent.linear_multiply(0.6);
        v.widgets.active.weak_bg_fill = accent.linear_multiply(0.6);
        v.widgets.active.fg_stroke = egui::Stroke::new(1.5_f32, fg);
        v.widgets.open.bg_fill = header;
        v.override_text_color = None;
        ctx.set_visuals(v);
        let _ = muted;

        let size = self.font_size.clamp(8.0, 32.0);
        ctx.style_mut(|s| {
            use egui::{FontFamily, FontId, TextStyle};
            // `font` is "" (egui's proportional font) or "monospace".
            let body_family = if self.font.eq_ignore_ascii_case("monospace") { FontFamily::Monospace } else { FontFamily::Proportional };
            let set = |s: &mut egui::Style, ts: TextStyle, k: f32| {
                if let Some(f) = s.text_styles.get_mut(&ts) {
                    let family = if ts == TextStyle::Monospace { FontFamily::Monospace } else { body_family.clone() };
                    *f = FontId::new(size * k, family);
                }
            };
            set(s, TextStyle::Body, 1.0);
            set(s, TextStyle::Button, 1.0);
            set(s, TextStyle::Monospace, 1.0);
            set(s, TextStyle::Small, 0.8);
            set(s, TextStyle::Heading, 1.45);
        });
    }

    /// Convert RGB [0..1] to egui Color32.
    pub fn rgb_to_color32(&self, rgb: [f32; 3]) -> egui::Color32 {
        let r = (rgb[0].clamp(0.0, 1.0) * 255.0) as u8;
        let g = (rgb[1].clamp(0.0, 1.0) * 255.0) as u8;
        let b = (rgb[2].clamp(0.0, 1.0) * 255.0) as u8;
        egui::Color32::from_rgb(r, g, b)
    }

    /// Background as egui Color32.
    pub fn background_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.background)
    }

    /// Foreground text as egui Color32.
    pub fn foreground_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.foreground)
    }

    /// Muted text as egui Color32.
    pub fn muted_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.muted_foreground)
    }

    /// Accent highlight as egui Color32.
    pub fn accent_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.accent)
    }

    /// Border/separator as egui Color32.
    pub fn border_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.border)
    }

    /// Panel background as egui Color32.
    pub fn panel_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.panel)
    }

    /// Panel header background as egui Color32.
    pub fn panel_header_color(&self) -> egui::Color32 {
        self.rgb_to_color32(self.panel_header)
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::builtin_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_to_egui_sets_visuals_and_font_size() {
        let ctx = egui::Context::default();
        let mut t = EditorTheme::builtin_default();
        t.font_size = 20.0;
        t.apply_to_egui(&ctx);
        assert_eq!(ctx.style().visuals.panel_fill, t.panel_color());
        assert_eq!(ctx.style().visuals.selection.stroke.color, t.accent_color());
        assert_eq!(ctx.style().text_styles[&egui::TextStyle::Body].size, 20.0);
    }

    #[test]
    fn font_choice_switches_the_chrome_family_and_back() {
        let ctx = egui::Context::default();
        let mut t = EditorTheme::builtin_default();
        t.apply_to_egui(&ctx);
        assert_eq!(ctx.style().text_styles[&egui::TextStyle::Body].family, egui::FontFamily::Proportional);
        t.font = "monospace".into();
        t.apply_to_egui(&ctx);
        for ts in [egui::TextStyle::Body, egui::TextStyle::Button, egui::TextStyle::Heading, egui::TextStyle::Small] {
            assert_eq!(ctx.style().text_styles[&ts].family, egui::FontFamily::Monospace, "{ts:?}");
        }
        t.font = String::new();
        t.apply_to_egui(&ctx);
        assert_eq!(ctx.style().text_styles[&egui::TextStyle::Body].family, egui::FontFamily::Proportional);
        assert_eq!(ctx.style().text_styles[&egui::TextStyle::Monospace].family, egui::FontFamily::Monospace);
    }

    #[test]
    fn theme_ron_roundtrip() {
        let mut t = EditorTheme::builtin_default();
        t.accent = [0.9, 0.2, 0.1];
        let s = ron::to_string(&t).unwrap();
        let back: EditorTheme = ron::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn test_builtin_theme_is_valid() {
        let theme = EditorTheme::builtin_default();
        // All RGB values must be in [0..1]
        assert!(theme.background.iter().all(|&v| v >= 0.0 && v <= 1.0));
        assert!(theme.foreground.iter().all(|&v| v >= 0.0 && v <= 1.0));
        assert!(theme.muted_foreground.iter().all(|&v| v >= 0.0 && v <= 1.0));
        assert!(theme.accent.iter().all(|&v| v >= 0.0 && v <= 1.0));
        assert!(theme.border.iter().all(|&v| v >= 0.0 && v <= 1.0));
        assert!(theme.panel.iter().all(|&v| v >= 0.0 && v <= 1.0));
        assert!(theme.panel_header.iter().all(|&v| v >= 0.0 && v <= 1.0));
    }

    #[test]
    fn test_default_theme_bg_fg_differ() {
        let t = EditorTheme::builtin_default();
        // Background and foreground must be visibly different
        assert_ne!(
            (t.background[0] as i32, t.background[1] as i32, t.background[2] as i32),
            (t.foreground[0] as i32, t.foreground[1] as i32, t.foreground[2] as i32),
            "default theme background must differ from foreground"
        );
    }

    #[test]
    fn test_color_conversion_roundtrip() {
        let theme = EditorTheme::builtin_default();
        let bg_color = theme.background_color();
        // egui Color32 is stored as [r, g, b, a], so we can read it back
        let r = bg_color.r() as f32 / 255.0;
        let g = bg_color.g() as f32 / 255.0;
        let b = bg_color.b() as f32 / 255.0;
        // Within rounding error (~1/255), should match the original
        assert!((r - theme.background[0]).abs() < 0.01);
        assert!((g - theme.background[1]).abs() < 0.01);
        assert!((b - theme.background[2]).abs() < 0.01);
    }
}
