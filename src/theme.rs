/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! TOML theme loader for egui.
//!
//! A theme is a small hand-editable file on top of `Visuals::dark/light`:
//! palette + mono font + timeline colors + bevel width. See `themes/*.toml`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// On-disk theme file. Keep it tiny — everything else falls back to egui defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct ThemeFile {
    pub name: String,
    #[serde(default = "default_parent")]
    pub parent: String,
    #[serde(default)]
    pub palette: HashMap<String, String>,
    #[serde(default)]
    pub timeline: TimelineTheme,
    #[serde(default)]
    pub font: FontTheme,
}

fn default_parent() -> String {
    "dark".to_owned()
}

/// Timeline colors egui doesn't know about.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TimelineTheme {
    /// Timestamp color, e.g. grey.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Cycle of colors for sender names.
    #[serde(default)]
    pub nick_colors: Vec<String>,
    /// Bevel width for old-internet panels. 0 = flat, 2 = Win98.
    #[serde(default)]
    pub bevel_width: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FontTheme {
    #[serde(default)]
    pub mono_size: Option<f32>,
}

impl ThemeFile {
    pub fn load_from_str(s: &str) -> Result<Self> {
        toml::from_str(s).context("parse theme TOML")
    }

    pub fn builtin(name: &str) -> &'static str {
        match name {
            "midnight" => include_str!("../themes/midnight.toml"),
            "bbs-amber" => include_str!("../themes/bbs-amber.toml"),
            "win98" => include_str!("../themes/win98.toml"),
            _ => include_str!("../themes/dark.toml"),
        }
    }

    pub fn load_builtin(name: &str) -> Result<Self> {
        Self::load_from_str(Self::builtin(name))
    }

    /// Gold text. Falls back to warm gold.
    pub fn gold(&self) -> egui::Color32 {
        self.palette
            .get("gold")
            .and_then(|s| parse_hex(s))
            .unwrap_or(egui::Color32::from_rgb(0xd9, 0xa6, 0x48))
    }

    /// Accent color. Falls back to teal.
    pub fn accent(&self) -> egui::Color32 {
        self.palette
            .get("accent")
            .and_then(|s| parse_hex(s))
            .unwrap_or(egui::Color32::from_rgb(0x5c, 0xc8, 0xc0))
    }
}

/// Parse `#rgb` / `#rrggbb` / `#rrggbbaa` into egui color.
pub fn parse_hex(s: &str) -> Option<egui::Color32> {
    fn hex_nibble(c: char) -> Option<u8> {
        u8::from_str_radix(&format!("{c}{c}"), 16).ok()
    }
    let h = s.trim().trim_start_matches(['#', '$']);
    let (r, g, b, a) = match h.len() {
        3 => {
            let v: Vec<char> = h.chars().collect();
            if v.len() != 3 {
                return None;
            }
            (hex_nibble(v[0])?, hex_nibble(v[1])?, hex_nibble(v[2])?, 255)
        }
        6 => (
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
            255,
        ),
        8 => (
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
            u8::from_str_radix(&h[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some(egui::Color32::from_rgba_premultiplied(r, g, b, a))
}

/// Apply theme to egui ctx. Unknown keys are ignored; missing keys keep parent defaults.
pub fn apply_theme(ctx: &egui::Context, theme: &ThemeFile) {
    let egui_theme = if theme.parent == "light" {
        egui::Theme::Light
    } else {
        egui::Theme::Dark
    };
    let mut visuals = if theme.parent == "light" {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    if let Some(bg) = theme.palette.get("bg").and_then(|s| parse_hex(s)) {
        visuals.window_fill = bg;
        visuals.panel_fill = bg;
    }
    if let Some(panel) = theme.palette.get("panel").and_then(|s| parse_hex(s)) {
        visuals.extreme_bg_color = panel;
    }
    if let Some(text) = theme.palette.get("text").and_then(|s| parse_hex(s)) {
        visuals.override_text_color = Some(text);
    }
    if let Some(sel) = theme.palette.get("selection").and_then(|s| parse_hex(s)) {
        visuals.selection.bg_fill = sel;
    }
    if let Some(accent) = theme.palette.get("accent").and_then(|s| parse_hex(s)) {
        visuals.hyperlink_color = accent;
        visuals.selection.stroke.color = accent;
    }
    if let Some(muted) = theme.palette.get("muted").and_then(|s| parse_hex(s)) {
        visuals.weak_text_color = Some(muted);
    }
    // Shape is rounded, flat-until-hovered, soft shadows; colour stays the theme file's.
    let accent = theme
        .palette
        .get("accent")
        .and_then(|s| parse_hex(s))
        .unwrap_or(visuals.hyperlink_color);
    // "Surface" is the raised hover/separator fill. A theme can name it; otherwise nudge the
    // panel toward the text colour so it stays legible in light and dark themes.
    let surface = theme
        .palette
        .get("surface")
        .and_then(|s| parse_hex(s))
        .unwrap_or_else(|| {
            let base = visuals.panel_fill;
            if theme.parent == "light" {
                egui::Color32::from_rgb(
                    base.r().saturating_sub(18),
                    base.g().saturating_sub(18),
                    base.b().saturating_sub(18),
                )
            } else {
                egui::Color32::from_rgb(
                    base.r().saturating_add(22),
                    base.g().saturating_add(22),
                    base.b().saturating_add(22),
                )
            }
        });
    crate::ui::shape_visuals(&mut visuals, accent, surface);
    ctx.set_visuals_of(egui_theme, visuals);

    // Body/Button are proportional (the UI typeface); Monospace stays monospaced for code and
    // column-aligned chrome. Forcing Monospace here once made the whole app read as a terminal
    // and swallowed the UI font setting entirely.
    let size = theme.font.mono_size.unwrap_or(14.0);
    ctx.style_mut_of(egui_theme, |style| crate::ui::shape_style(style, size));
}
