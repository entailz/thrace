/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Message bodies: markdown spans, inline emoji, replies and read receipts.

use crate::app::text::mention_mxid;
use crate::app::ThraceApp;

impl ThraceApp {
    /// Read receipts as overlapping circular avatars; overflow collapses to a count.
    pub(in crate::app) fn render_seen_by(&mut self, ui: &mut egui::Ui, seen: &[String]) {
        /// Avatars shown before overflow count.
        const MAX_SHOWN: usize = 5;
        let size = 16.0;
        // Avatars overlap by ~40%.
        let step = size * 0.62;
        let shown = seen.len().min(MAX_SHOWN);
        let overflow = seen.len() - shown;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let width = step * shown.saturating_sub(1) as f32 + size;
            let (area, _) = ui.allocate_exact_size(egui::vec2(width, size), egui::Sense::hover());
            let ring = ui.visuals().panel_fill;
            for (i, mxid) in seen.iter().take(shown).enumerate() {
                let (name, avatar, _) = self.profile_of(mxid);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(area.left() + step * i as f32, area.top()),
                    egui::vec2(size, size),
                );
                self.paint_avatar_circle(ui, rect, &avatar, &name, ring);
                // Hover names each reader.
                ui.interact(
                    rect,
                    ui.id().with(("seen", i, mxid.as_str())),
                    egui::Sense::hover(),
                )
                .on_hover_text(&name);
            }
            if overflow > 0 {
                let rest: Vec<String> = seen
                    .iter()
                    .skip(shown)
                    .map(|m| self.display_for(m))
                    .collect();
                ui.label(egui::RichText::new(format!("+{overflow}")).small().weak())
                    .on_hover_text(rest.join(", "));
            }
        });
    }

    /// One emoji at `size`: colour bitmap, else text glyph. Returns response for clicks.
    pub(in crate::app) fn emoji_widget(
        &mut self,
        ui: &mut egui::Ui,
        emoji: &str,
        size: f32,
    ) -> egui::Response {
        if let Some(handle) = self.emoji_font.texture_at_size(ui.ctx(), emoji, size) {
            return ui.add(
                egui::Image::new(&handle)
                    .fit_to_exact_size(egui::vec2(size, size))
                    .sense(egui::Sense::click()),
            );
        }
        // Same size as the image so a missing bitmap keeps line height.
        crate::ui::clickable(
            ui.add(
                egui::Label::new(egui::RichText::new(emoji).size(size * 0.9))
                    .selectable(false)
                    .sense(egui::Sense::click()),
            ),
        )
    }

    /// Text with colour emoji at an explicit position, for painted sidebar rows.
    pub(in crate::app) fn paint_text_with_emoji(
        &mut self,
        ui: &mut egui::Ui,
        pos: egui::Pos2,
        text: &str,
        size: f32,
        colour: egui::Color32,
        max_width: f32,
    ) {
        let font = egui::FontId::proportional(size);
        let mut x = pos.x;
        let limit = pos.x + max_width;
        let mut buf = String::new();
        // Flush pending text; returns its width.
        let flush = |ui: &mut egui::Ui, buf: &mut String, x: &mut f32| {
            if buf.is_empty() {
                return;
            }
            let galley = ui
                .painter()
                .layout_no_wrap(std::mem::take(buf), font.clone(), colour);
            let w = galley.size().x;
            ui.painter().galley(
                egui::pos2(*x, pos.y - galley.size().y * 0.5),
                galley,
                colour,
            );
            *x += w;
        };
        let mut rest = text;
        while !rest.is_empty() {
            if x > limit {
                break;
            }
            let Some(len) = crate::emoji::emoji_cluster_len(rest) else {
                let ch = rest.chars().next().expect("non-empty");
                buf.push(ch);
                rest = &rest[ch.len_utf8()..];
                continue;
            };
            let cluster = rest[..len].to_owned();
            rest = &rest[len..];
            flush(ui, &mut buf, &mut x);
            if x > limit {
                break;
            }
            let side = size * 1.15;
            let rect = egui::Rect::from_center_size(
                egui::pos2(x + side * 0.5, pos.y),
                egui::vec2(side, side),
            );
            match self.emoji_font.texture_at_size(ui.ctx(), &cluster, side) {
                Some(handle) => {
                    ui.painter().image(
                        handle.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    // No bitmap: use the glyph.
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &cluster,
                        font.clone(),
                        colour,
                    );
                }
            }
            x += side;
        }
        flush(ui, &mut buf, &mut x);
    }

    /// Short text with colour emoji, for previews and banners.
    pub(in crate::app) fn reply_text_size(&self) -> f32 {
        self.settings_font_size.clamp(10.0, 24.0)
    }

    pub(in crate::app) fn reply_preview(&mut self, ui: &mut egui::Ui, text: &str) {
        let size = self.reply_text_size();
        ui.scope(|ui| {
            let style = ui.style_mut();
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(size));
            style
                .text_styles
                .insert(egui::TextStyle::Monospace, egui::FontId::monospace(size));
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(size));
            self.render_body(ui, text, None);
        });
    }

    pub(in crate::app) fn emoji_text(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        size: f32,
        weak: bool,
    ) {
        let style = |rt: egui::RichText| if weak { rt.weak() } else { rt };
        // Wrapped so long text can't run past the window.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut buf = String::new();
            let mut rest = text;
            while !rest.is_empty() {
                match crate::emoji::emoji_cluster_len(rest) {
                    Some(len) => {
                        if !buf.is_empty() {
                            let pending = std::mem::take(&mut buf);
                            ui.label(style(egui::RichText::new(pending).size(size)));
                        }
                        self.emoji_widget(ui, &rest[..len], size * 1.15);
                        rest = &rest[len..];
                    }
                    None => {
                        let ch = rest.chars().next().expect("non-empty");
                        buf.push(ch);
                        rest = &rest[ch.len_utf8()..];
                    }
                }
            }
            if !buf.is_empty() {
                ui.label(style(egui::RichText::new(buf).size(size)));
            }
        });
    }

    /// Text run split into colour-emoji images and plain text.
    pub(in crate::app) fn render_text_with_emoji(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        span: &crate::markdown::Span,
        size: f32,
    ) {
        if !self.emoji_font.available() {
            self.render_plain_run(ui, text, span);
            return;
        }
        // Walk clusters so modifiers/ZWJ stay attached.
        let mut buf = String::new();
        let mut rest = text;
        while !rest.is_empty() {
            match crate::emoji::emoji_cluster_len(rest) {
                Some(len) => {
                    if !buf.is_empty() {
                        let pending = std::mem::take(&mut buf);
                        self.render_plain_run(ui, &pending, span);
                    }
                    let cluster = &rest[..len];
                    self.emoji_widget(ui, cluster, size)
                        .on_hover_text(crate::emoji::name_of(cluster));
                    rest = &rest[len..];
                }
                None => {
                    let ch = rest.chars().next().expect("non-empty");
                    buf.push(ch);
                    rest = &rest[ch.len_utf8()..];
                }
            }
        }
        if !buf.is_empty() {
            self.render_plain_run(ui, &buf, span);
        }
    }

    /// Rich body: markdown/HTML spans with custom emoji resolved.
    pub(in crate::app) fn render_body(
        &mut self,
        ui: &mut egui::Ui,
        body: &str,
        formatted: Option<&str>,
    ) {
        let msg = crate::markdown::render_message(body, formatted);
        for block in &msg.blocks {
            match block {
                crate::markdown::Block::Spans(spans) => {
                    ui.horizontal_wrapped(|ui| {
                        // No inter-widget gap; runs carry their own spaces.
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for span in spans {
                            self.render_span(ui, span);
                        }
                    });
                }
                crate::markdown::Block::Code(cb) => {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        if !cb.lang.is_empty() {
                            ui.monospace(egui::RichText::new(cb.lang.clone()).small().weak());
                        }
                        ui.monospace(&cb.code);
                    });
                }
            }
        }
    }

    /// One styled span: links, accent code, inline custom emoji.
    pub(in crate::app) fn render_span(&mut self, ui: &mut egui::Ui, span: &crate::markdown::Span) {
        // Links/code are never `:shortcode:`; don't split them.
        if span.link.is_some() || span.code {
            self.render_text_run(ui, &span.text, span);
            return;
        }
        // Split `:shortcode:` runs for inline emoji images.
        let mut rest = span.text.as_str();
        let mut first = true;
        while let Some(start) = rest.find(':') {
            let (before, after_start) = rest.split_at(start);
            if !before.is_empty() || first {
                if !before.is_empty() {
                    self.render_text_run(ui, before, span);
                }
                first = false;
            }
            if let Some(end) = after_start[1..].find(':') {
                let sc = &after_start[..end + 2];
                if self.packs.resolve(sc).is_some() {
                    self.render_custom_emoji(ui, sc);
                } else {
                    self.render_text_run(ui, sc, span);
                }
                rest = &after_start[end + 2..];
            } else {
                self.render_text_run(ui, after_start, span);
                rest = "";
                break;
            }
        }
        if !rest.is_empty() {
            self.render_text_run(ui, rest, span);
        }
    }

    pub(in crate::app) fn render_text_run(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        span: &crate::markdown::Span,
    ) {
        // Only prose gets emoji lifted into colour images.
        if span.link.is_some() || span.code {
            self.render_plain_run(ui, text, span);
            return;
        }
        // Emoji render slightly larger than body text.
        let size = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
        self.render_text_with_emoji(ui, text, span, size);
    }

    /// Render one run as text, with no emoji substitution.
    pub(in crate::app) fn render_plain_run(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        span: &crate::markdown::Span,
    ) {
        if text == "\n" {
            ui.end_row();
            return;
        }
        // Prose is proportional; code and chrome stay monospace.
        let mut rt = if span.code {
            egui::RichText::new(text).monospace()
        } else {
            egui::RichText::new(text)
        };
        if span.bold {
            rt = rt.strong();
        }
        if span.italic {
            rt = rt.italics();
        }
        if span.code {
            rt = rt.background_color(egui::Color32::from_gray(40));
        }
        if let Some(link) = &span.link {
            let link = link.clone();
            // Mention pills open the profile card, not a browser.
            if let Some(mxid) = mention_mxid(&link) {
                let display = self.display_for(&mxid);
                // Resolve bare linkified `@user:server` to a display name.
                let label = if text == mxid {
                    format!("@{display}")
                } else {
                    text.to_owned()
                };
                let colour = self.nick_color(&display);
                let resp = crate::ui::clickable(
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(label)
                                .color(colour)
                                .strong()
                                .background_color(colour.gamma_multiply(0.18)),
                        )
                        .selectable(false)
                        .sense(egui::Sense::click()),
                    ),
                )
                .on_hover_text(format!("{mxid} — view profile"));
                if resp.clicked() {
                    let anchor = ui
                        .ctx()
                        .pointer_latest_pos()
                        .unwrap_or_else(|| resp.rect.right_top());
                    self.profile_target = Some((mxid, anchor, ui.ctx().cumulative_pass_nr()));
                }
                return;
            }
            // Rewrite through the claiming front end; leaks nothing until clicked.
            let target = crate::embed::rewrite(&self.embed_rules, &link).unwrap_or(link);
            ui.hyperlink_to(rt.underline(), target);
        } else if span.code {
            ui.colored_label(egui::Color32::from_rgb(255, 203, 107), rt);
        } else {
            ui.label(rt);
        }
    }

    /// Custom `:shortcode:` emoji as inline image; fetches on first sight.
    pub(in crate::app) fn render_custom_emoji(&mut self, ui: &mut egui::Ui, shortcode: &str) {
        let mxc = self
            .packs
            .resolve(shortcode)
            .map(|i| i.mxc_url.clone())
            .unwrap_or_default();
        if mxc.is_empty() {
            ui.label(egui::RichText::new(shortcode));
            return;
        }
        // Namespaced: same mxc at different sizes stays distinct.
        let key = format!("emoji:{mxc}");
        match (
            self.media.texture_for("emoji", &mxc, Some((24, 24))),
            self.media.status(&key),
        ) {
            (Some(handle), _) => {
                // Hover shows shortcode for discoverability.
                ui.add(egui::Image::new(&handle).max_height(20.0))
                    .on_hover_text(shortcode);
            }
            (None, Some(entry)) if entry.error().is_some() => {
                ui.colored_label(egui::Color32::RED, egui::RichText::new(shortcode).small())
                    .on_hover_text("emoji download failed — click to retry");
            }
            (None, _) => {
                crate::ui::icon_label(ui, crate::ui::icons::IMAGE, self.theme.gold())
                    .on_hover_text(format!("loading {shortcode}…"));
            }
        }
    }
}
