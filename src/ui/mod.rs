/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Shared UI vocabulary: icon glyphs and the widgets built from them.
//!
//! Replaces the old terminal-style chrome (box-drawing structure, shape glyphs for state,
//! `[bracket]` buttons) with Material icons, spacing, and rounding.

pub mod icons;

use egui::{Color32, CornerRadius, RichText, Stroke, Vec2};

/// Height of the square icon buttons in the toolbar and composer.
pub const ICON_BUTTON: f32 = 30.0;
/// Point size for icon glyphs. Material icons sit on a 24px grid, so they need a touch more
/// than surrounding text to look the same weight.
pub const ICON_SIZE: f32 = 17.0;

/// A square, borderless icon button with a tooltip.
///
/// The tooltip is not optional: an icon-only toolbar is usable only if every control names
/// itself on hover.
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> egui::Response {
    icon_button_tinted(ui, icon, tip, ui.visuals().widgets.inactive.fg_stroke.color)
}

/// Icon button in a specific colour — for destructive or accent actions.
pub fn icon_button_tinted(
    ui: &mut egui::Ui,
    icon: &str,
    tip: &str,
    tint: Color32,
) -> egui::Response {
    let button = egui::Button::new(RichText::new(icon).size(ICON_SIZE).color(tint))
        .min_size(Vec2::splat(ICON_BUTTON));
    ui.add(button).on_hover_text(tip)
}

/// Icon button that reads as "on" when `active` — for toggles like the emoji picker.
pub fn icon_toggle(ui: &mut egui::Ui, icon: &str, tip: &str, active: bool) -> egui::Response {
    let visuals = ui.visuals();
    let (fg, bg) = if active {
        (visuals.selection.stroke.color, visuals.selection.bg_fill)
    } else {
        (
            visuals.widgets.inactive.fg_stroke.color,
            Color32::TRANSPARENT,
        )
    };
    let button = egui::Button::new(RichText::new(icon).size(ICON_SIZE).color(fg))
        .fill(bg)
        .corner_radius(CornerRadius::same(7))
        .min_size(Vec2::splat(ICON_BUTTON));
    ui.add(button).on_hover_text(tip)
}

/// An icon glyph as a plain label, for state indicators that aren't buttons.
pub fn icon_label(ui: &mut egui::Ui, icon: &str, colour: Color32) -> egui::Response {
    ui.label(RichText::new(icon).size(ICON_SIZE).color(colour))
}

/// A small pill used for counts — unread badges, reaction counts.
pub fn badge(ui: &mut egui::Ui, text: &str, fill: Color32, fg: Color32) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(11.0), fg);
    let pad = Vec2::new(6.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().galley(rect.min + pad, galley, fg);
    resp
}

/// A popup window with one consistent shape: title bar with close X, no collapse arrow,
/// height always fitting the viewport (an oversized window couldn't be resized from the top
/// edge, which once made Settings impossible to close).
pub fn popup(ctx: &egui::Context, title: &str) -> egui::Window<'static> {
    let viewport = ctx.viewport_rect();
    egui::Window::new(title.to_owned())
        .collapsible(false)
        .resizable(true)
        // Leave room for the title bar and margin, so the close X stays reachable.
        .max_height((viewport.height() - 64.0).max(200.0))
        .max_width((viewport.width() - 64.0).max(320.0))
        // Content panels scroll themselves; a second scrollbar here would fight them.
        .vscroll(false)
}

/// Mark a response as clickable: hovering shows the pointing hand.
///
/// egui leaves the cursor alone for hand-rolled clickables, and a `Label` shows the text
/// I-beam (labels are selectable) — so clickable text gave no hint it could be clicked.
pub fn clickable(resp: egui::Response) -> egui::Response {
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Did a click land outside `rect` this frame?
///
/// Dismissal gesture for transient popups. `opened_frame` guards the opening frame: the
/// opening click is still queued on first paint, and without this the popup would close on
/// the click that spawned it.
pub fn clicked_outside(ctx: &egui::Context, rect: egui::Rect, opened_frame: u64) -> bool {
    if ctx.cumulative_pass_nr() <= opened_frame {
        return false;
    }
    ctx.input(|i| {
        i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| !rect.contains(p))
    })
}

/// Shape the widget visuals: rounding, flat-until-hovered fills, shadows.
///
/// Colour comes from the theme file; this owns *form*. Plain mutator (not touching the
/// context) so it can't race `theme::apply_theme`, which calls this while building visuals.
pub fn shape_visuals(v: &mut egui::Visuals, accent: Color32, surface: Color32) {
    // egui paints the window title strip with the active/open widget fill. Keep it darker
    // than the content surface without changing the window body.
    v.widgets.open.weak_bg_fill = v.window_fill.gamma_multiply(0.72);
    v.widgets.open.bg_fill = v.window_fill.gamma_multiply(0.72);
    let r = CornerRadius::same(7);
    v.window_corner_radius = CornerRadius::same(11);
    v.menu_corner_radius = CornerRadius::same(9);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }

    // Flat by default, lifting only on hover.
    v.widgets.inactive.bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    // Hover lifts to `surface`; pressing goes brighter still, so a click reads as a press.
    v.widgets.hovered.bg_fill = surface;
    v.widgets.hovered.weak_bg_fill = surface;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, surface);
    v.widgets.hovered.expansion = 0.0;
    let pressed = accent.gamma_multiply(0.30);
    v.widgets.active.bg_fill = pressed;
    v.widgets.active.weak_bg_fill = pressed;
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent.gamma_multiply(0.6));
    v.widgets.active.expansion = 0.0;

    v.selection.bg_fill = accent.gamma_multiply(0.25);
    v.selection.stroke = Stroke::new(1.0, accent);
    v.hyperlink_color = accent;

    // Hairlines for structure.
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, surface);

    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(70),
    };
    v.popup_shadow = v.window_shadow;
}

/// Spacing, hit targets and the text-style ramp.
///
/// `ui_size` is the user's font-size setting. Body/Button are proportional (the point of a UI
/// typeface); Monospace stays monospaced for code, timestamps, column-aligned text.
pub fn shape_style(style: &mut egui::Style, ui_size: f32) {
    use egui::{FontFamily, FontId, TextStyle};

    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.indent = 18.0;
    style.spacing.interact_size.y = 26.0;
    style.spacing.scroll.bar_width = 9.0;
    style.spacing.scroll.floating = true;

    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(ui_size, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(ui_size, FontFamily::Proportional),
    );
    // Popup chrome: compact, readable title bars.
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new((ui_size - 1.0).max(10.0), FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new((ui_size - 3.0).max(9.0), FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(ui_size - 1.0, FontFamily::Monospace),
    );
}
