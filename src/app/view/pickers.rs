/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Emoji grid and skin-tone selection.

use crate::app::{ThraceApp, SKIN_TONES};

impl ThraceApp {
    /// Emoji grid: a row of category jumps over the scrolled sections. Returns this frame's
    /// pick. Search comes from the caller's field, so the grid adds no second box.
    pub(in crate::app) fn emoji_grid(&mut self, ui: &mut egui::Ui, height: f32) -> Option<String> {
        let mut picked: Option<String> = None;
        let recents = self.recent_emoji.clone();

        ui.horizontal_wrapped(|ui| {
            // Category jumps read as separate buttons only with room between them.
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 6.0);
            if !recents.is_empty() {
                // Clock icon jumps to recents.
                if self
                    .emoji_widget(ui, "\u{1F551}", 17.0)
                    .on_hover_text("Frequently used")
                    .clicked()
                {
                    self.scroll_to_category = Some(usize::MAX);
                }
            }
            for (i, cat) in crate::emoji::CATEGORIES.iter().enumerate() {
                if self
                    .emoji_widget(ui, cat.icon, 17.0)
                    .on_hover_text(cat.name)
                    .clicked()
                {
                    self.scroll_to_category = Some(i);
                }
            }
            ui.add_space(4.0);
            self.skin_tone_menu(ui);
        });
        ui.separator();

        let query = self.picker_query.trim().to_lowercase();
        let jump = self.scroll_to_category.take();
        egui::ScrollArea::vertical()
            .max_height(height)
            .show(ui, |ui| {
                // Search collapses categories into one ranked list.
                if !query.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for (_, e, skin) in crate::emoji::search(&query) {
                            let label = self.emoji_with_skin(e, skin);
                            if self
                                .emoji_widget(ui, &label, 22.0)
                                .on_hover_text(crate::emoji::name_of(e))
                                .clicked()
                            {
                                picked = Some(label);
                            }
                        }
                    });
                    return;
                }

                if !recents.is_empty() {
                    let header = ui.label(
                        egui::RichText::new("Frequently used")
                            .small()
                            .weak()
                            .strong(),
                    );
                    if jump == Some(usize::MAX) {
                        header.scroll_to_me(Some(egui::Align::TOP));
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for e in &recents {
                            if self
                                .emoji_widget(ui, e, 22.0)
                                .on_hover_text(crate::emoji::name_of(e))
                                .clicked()
                            {
                                picked = Some(e.clone());
                            }
                        }
                    });
                }

                for (i, cat) in crate::emoji::CATEGORIES.iter().enumerate() {
                    let header = ui.label(egui::RichText::new(cat.name).small().weak().strong());
                    if jump == Some(i) {
                        header.scroll_to_me(Some(egui::Align::TOP));
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for (e, skin) in cat.emojis.iter() {
                            let label = self.emoji_with_skin(e, *skin);
                            if self
                                .emoji_widget(ui, &label, 22.0)
                                .on_hover_text(crate::emoji::name_of(e))
                                .clicked()
                            {
                                picked = Some(label);
                            }
                        }
                    });
                }
            });

        if let Some(e) = &picked {
            self.note_recent_emoji(e);
        }
        picked
    }

    /// Skin-tone chooser, collapsed into a dropdown. The button is the hand in the current
    /// tone and the menu is the same hand in all six, so the colour carries the choice and
    /// the tone names are left to hover text.
    pub(in crate::app) fn skin_tone_menu(&mut self, ui: &mut egui::Ui) {
        const SWATCH: f32 = 18.0;
        let current = format!("\u{270B}{}", self.skin_tone);
        let chevron = egui::RichText::new(crate::ui::icons::CHEVRON_DOWN).size(12.0);
        let button = match self.emoji_font.texture_at_size(ui.ctx(), &current, SWATCH) {
            Some(handle) => ui.add(egui::Button::new((
                egui::Image::new(&handle).fit_to_exact_size(egui::Vec2::splat(SWATCH)),
                chevron,
            ))),
            // No colour bitmap: the text glyph still shows the tone.
            None => ui.add(egui::Button::new((
                egui::RichText::new(&current).size(SWATCH * 0.9),
                chevron,
            ))),
        }
        .on_hover_text("Skin tone");
        egui::Popup::menu(&button).show(|ui| {
            ui.horizontal(|ui| {
                for (modifier, label) in SKIN_TONES {
                    let swatch = format!("\u{270B}{modifier}");
                    let resp = self.emoji_widget(ui, &swatch, 22.0).on_hover_text(*label);
                    if self.skin_tone == *modifier {
                        // Underline the active tone; image widgets carry no selection state.
                        let r = resp.rect;
                        ui.painter().line_segment(
                            [
                                egui::pos2(r.left(), r.bottom() + 1.0),
                                egui::pos2(r.right(), r.bottom() + 1.0),
                            ],
                            egui::Stroke::new(2.0, self.theme.gold()),
                        );
                    }
                    if resp.clicked() {
                        self.skin_tone = (*modifier).to_owned();
                        ui.close();
                    }
                }
            });
        });
    }
}
