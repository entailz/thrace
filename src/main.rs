/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

// matrix-sdk crypto futures nest deeply; proving `Send` on the shared
// runtime overflows the default recursion limit.
#![recursion_limit = "512"]

//! Thrace — desktop Matrix client.
//!
//! Thin eframe shell around [`app::ThraceApp`]. Tokio lives on a background
//! runtime; the UI thread only pumps channels and paints egui.

pub mod app;
pub mod config;
pub mod embed;
pub mod emoji;
pub mod emoji_font;
pub mod history_queue;
pub mod markdown;
pub mod matrix;
pub mod media_cache;
pub mod recent_emoji;
pub mod session_store;
pub mod theme;
pub mod ui;
pub mod verify;
pub mod video;

use anyhow::Result;

/// Font strategy.
///
/// egui 0.35 rasterises glyph outlines only (no COLR/CBDT bitmaps), so
/// bitmap-only `NotoColorEmoji.ttf` draws nothing here; emoji come from images.
/// Symbola has monochrome outlines covering the chrome symbols plus most emoji,
/// so it is the fallback for both families.
///
/// To change the UI typeface, add a path to `UI_FONTS`; first readable file
/// wins. Same for `MONO_FONTS` and `SYMBOL_FONTS`.
fn install_fonts(ctx: &egui::Context) {
    const UI_FONTS: &[&str] = &[
        "/usr/share/fonts/ttf-readex-pro/ReadexPro-Regular.ttf",
        "~/.local/share/fonts/ReadexPro-Regular.ttf",
        // Variable font: skrifa renders its default instance.
        "/usr/share/fonts/TTF/Rubik%5Bwght%5D.ttf",
        "/usr/share/fonts/TTF/Rubik.ttf",
    ];
    // Monospace, used by the `ui.monospace` chrome.
    const MONO_FONTS: &[&str] = &[
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/FiraMono-Regular.ttf",
    ];
    // Fallback only; fills glyphs the primary lacks.
    const SYMBOL_FONTS: &[&str] = &[
        "/usr/share/fonts/TTF/Symbola.ttf",
        "/usr/share/fonts/OTF/Symbola.otf",
        "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
    ];
    // Outline glyphs, so egui can rasterise them — see `ui::icons`.
    const ICON_FONTS: &[&str] = &[
        "/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf",
        "/usr/share/fonts/SpaceMono/SymbolsNerdFont-Regular.ttf",
        "~/.local/share/fonts/SymbolsNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/MaterialSymbolsOutlined.ttf",
    ];

    fn read_first(paths: &[&str]) -> Option<Vec<u8>> {
        for path in paths {
            let resolved = match path.strip_prefix("~/") {
                Some(rest) => std::env::var("HOME")
                    .ok()
                    .map(|h| std::path::PathBuf::from(h).join(rest))?,
                None => std::path::PathBuf::from(path),
            };
            if let Ok(bytes) = std::fs::read(&resolved) {
                return Some(bytes);
            }
        }
        None
    }

    let mut fonts = egui::FontDefinitions::default();

    // Primary faces first to win over egui defaults; symbol font last, gaps only.
    if let Some(bytes) = read_first(UI_FONTS) {
        fonts
            .font_data
            .insert("ui".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "ui".into());
    }
    if let Some(bytes) = read_first(MONO_FONTS) {
        fonts
            .font_data
            .insert("mono".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "mono".into());
    }
    // Icons before symbols: same codepoints, Material wins.
    if let Some(bytes) = read_first(ICON_FONTS) {
        fonts
            .font_data
            .insert("icons".into(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("icons".into());
        }
    } else {
        eprintln!(
            "thrace: no icon font found — toolbar icons will be blank boxes.\n\
             install `ttf-nerd-fonts-symbols` (Symbols Nerd Font) to fix."
        );
    }
    if let Some(bytes) = read_first(SYMBOL_FONTS) {
        fonts
            .font_data
            .insert("symbols".into(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("symbols".into());
        }
    }
    ctx.set_fonts(fonts);
}

/// Pick the windowing backend, returning `(name, file-drops-work)`.
///
/// winit 0.30 has no file-drop support on Wayland; XWayland is the only way
/// to get drops. Since 0.29 removed `WINIT_UNIX_BACKEND`, hiding
/// `WAYLAND_DISPLAY` before the event loop selects X11. Default is native
/// Wayland; pass `--x11` to get drops back.
fn select_backend(force_x11: bool) -> (&'static str, bool) {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    if !wayland {
        return ("x11", x11);
    }
    if force_x11 && x11 {
        std::env::remove_var("WAYLAND_DISPLAY");
        return ("x11/xwayland", true);
    }
    ("wayland", false)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (backend, drops) = select_backend(args.iter().any(|a| a == "--x11"));
    eprintln!(
        "thrace: {backend} backend, file drag-and-drop {}",
        if drops {
            "enabled"
        } else {
            "unavailable (winit has no Wayland file drops — run with --x11, or paste with Ctrl+V)"
        }
    );

    let config_file = config::ConfigFile::open(config::config_path()?)?;
    let cli_theme = std::env::args().skip_while(|a| a != "--theme").nth(1);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("Thrace"),
        ..Default::default()
    };

    eframe::run_native(
        "thrace",
        native_options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            let mut app = app::ThraceApp::new(cc, config_file, cli_theme);
            app.set_backend(backend, drops);
            app.try_restore_cached();
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))?;
    Ok(())
}
