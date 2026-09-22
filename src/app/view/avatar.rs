/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Avatar circles, initials and per-nick colours.

use crate::app::ThraceApp;
use crate::theme::{self};

impl ThraceApp {
    pub(in crate::app) fn nick_color(&self, nick: &str) -> egui::Color32 {
        let colors = &self.theme.timeline.nick_colors;
        if colors.is_empty() {
            return egui::Color32::LIGHT_BLUE;
        }
        let h: usize = nick.bytes().fold(5381usize, |a, b| {
            a.wrapping_mul(33).wrapping_add(b as usize)
        });
        theme::parse_hex(&colors[h % colors.len()]).unwrap_or(egui::Color32::LIGHT_BLUE)
    }

    pub(in crate::app) fn avatar_initial(name: &str) -> String {
        name.chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or("?".into())
    }

    /// Avatar at `size`: cached mxc texture, else coloured initial. Clickable.
    pub(in crate::app) fn render_avatar_sized(
        &mut self,
        ui: &mut egui::Ui,
        mxc: &Option<String>,
        name: &str,
        size: f32,
    ) -> egui::Response {
        // Request near drawn size to avoid upscaled thumbnails.
        let request = (size.ceil() as u32).max(32);
        if let Some(uri) = mxc {
            let kind = if request > 64 { "avatar-lg" } else { "avatar" };
            if let Some(handle) = self.media.texture_for(kind, uri, Some((request, request))) {
                return ui.add(
                    egui::Image::new(&handle)
                        .max_size(egui::vec2(size, size))
                        .sense(egui::Sense::click()),
                );
            }
        }
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
        let col = self.nick_color(name);
        ui.painter()
            .rect_filled(rect, size * 0.21, col.gamma_multiply(0.25));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            Self::avatar_initial(name),
            egui::FontId::monospace(size * 0.46),
            col,
        );
        resp
    }

    /// Circular avatar in an explicit rect; ring separates overlaps.
    pub(in crate::app) fn paint_avatar_circle(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        mxc: &Option<String>,
        name: &str,
        ring: egui::Color32,
    ) {
        let centre = rect.center();
        let radius = rect.width() * 0.5;
        let texture = mxc.as_ref().and_then(|uri| {
            let request = (rect.width().ceil() as u32).max(32);
            self.media
                .texture_for("avatar", uri, Some((request, request)))
        });
        match texture {
            Some(handle) => {
                let mut mesh = egui::Mesh::with_texture(handle.id());
                const SEGMENTS: usize = 24;
                mesh.vertices.push(egui::epaint::Vertex {
                    pos: centre,
                    uv: egui::pos2(0.5, 0.5),
                    color: egui::Color32::WHITE,
                });
                for i in 0..=SEGMENTS {
                    let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
                    let (sin, cos) = angle.sin_cos();
                    mesh.vertices.push(egui::epaint::Vertex {
                        pos: centre + egui::vec2(cos * radius, sin * radius),
                        uv: egui::pos2(0.5 + cos * 0.5, 0.5 + sin * 0.5),
                        color: egui::Color32::WHITE,
                    });
                }
                for i in 1..=SEGMENTS as u32 {
                    mesh.indices.extend_from_slice(&[0, i, i + 1]);
                }
                ui.painter().add(egui::Shape::mesh(mesh));
            }
            None => {
                let colour = self.nick_color(name);
                ui.painter()
                    .circle_filled(centre, radius, colour.gamma_multiply(0.35));
                ui.painter().text(
                    centre,
                    egui::Align2::CENTER_CENTER,
                    Self::avatar_initial(name),
                    egui::FontId::proportional(radius * 0.95),
                    colour,
                );
            }
        }
        // Ring last so it overlays neighbours.
        ui.painter()
            .circle_stroke(centre, radius, egui::Stroke::new(1.5, ring));
    }

    /// Avatar into an explicit rect; no layout. For fixed room-list geometry.
    pub(in crate::app) fn paint_avatar_at(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        mxc: &Option<String>,
        name: &str,
    ) {
        let size = rect.width();
        if let Some(uri) = mxc {
            let request = (size.ceil() as u32).max(32);
            if let Some(handle) = self
                .media
                .texture_for("avatar", uri, Some((request, request)))
            {
                let tint = egui::Color32::WHITE;
                ui.painter().image(
                    handle.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    tint,
                );
                return;
            }
        }
        let col = self.nick_color(name);
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(7), col.gamma_multiply(0.25));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            Self::avatar_initial(name),
            egui::FontId::proportional(size * 0.46),
            col,
        );
    }

    pub(in crate::app) fn render_avatar(
        &mut self,
        ui: &mut egui::Ui,
        mxc: &Option<String>,
        name: &str,
    ) -> egui::Response {
        self.render_avatar_sized(ui, mxc, name, 28.0)
    }
}
