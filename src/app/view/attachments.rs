/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Images, video, audio players and link cards in the timeline.

use crate::app::decode::timeline_still_source;
use crate::app::text::truncate_name;
use crate::app::{AudioAttachment, ImageAttachment, ThraceApp};

impl ThraceApp {
    /// Timeline image ≤320px wide; placeholder while downloading, error + retry.
    pub(in crate::app) fn render_image(&mut self, ui: &mut egui::Ui, img: &ImageAttachment) {
        let Some(source) = timeline_still_source(img) else {
            // Video with no still of its own: straight to the poster.
            self.render_video_poster(ui, img);
            return;
        };
        let cache_key = format!("thumbnail:{}", img.mxc);
        if let Some(handle) = self
            .media
            .texture_for_source(&cache_key, source, Some((320, 320)))
        {
            let size = handle.size_vec2();
            let w = size.x.min(320.0);
            let h = if size.x > 0.0 {
                size.y * (w / size.x)
            } else {
                180.0
            };
            let hint = if img.is_video {
                "Play video"
            } else {
                "Open full-size image"
            };
            let resp = ui
                .add(
                    egui::Image::new(&handle)
                        .max_size(egui::vec2(w, h.min(320.0)))
                        .sense(egui::Sense::click()),
                )
                .on_hover_text(hint);
            if img.is_video {
                // Play badge marks it as video, not a still.
                Self::paint_play_badge(ui, resp.rect, img.duration_ms);
            }
            if resp.clicked() {
                self.image_preview = Some(img.clone());
            }
            return;
        }
        match self.media.status(&cache_key) {
            Some(entry) if entry.error().is_some() => {
                // The clip itself may still play; only its still failed.
                if img.is_video {
                    self.render_video_poster(ui, img);
                    return;
                }
                let e = entry.error().unwrap_or_default().to_owned();
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        crate::ui::icon_label(
                            ui,
                            crate::ui::icons::BROKEN,
                            ui.visuals().error_fg_color,
                        );
                        ui.label(egui::RichText::new(truncate_name(&img.name, 28)).small());
                    });
                    ui.label(egui::RichText::new(&e).small().weak());
                    if crate::ui::icon_button(ui, crate::ui::icons::REFRESH, "Retry download")
                        .clicked()
                    {
                        self.media.retry(&cache_key);
                    }
                });
            }
            _ => {
                ui.horizontal(|ui| {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(180.0, 110.0), egui::Sense::click());
                    ui.painter()
                        .rect_filled(r, 4.0, egui::Color32::from_gray(30));
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        crate::ui::icons::IMAGE,
                        egui::FontId::proportional(28.0),
                        egui::Color32::from_gray(90),
                    );
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(truncate_name(&img.name, 28)).small());
                        ui.label(
                            egui::RichText::new(format!("{}×{}", img.w, img.h))
                                .small()
                                .weak(),
                        );
                    });
                });
            }
        }
    }

    /// Play badge and clip length, painted over a video's frame.
    pub(in crate::app) fn paint_play_badge(
        ui: &egui::Ui,
        rect: egui::Rect,
        duration_ms: Option<u64>,
    ) {
        let centre = rect.center();
        ui.painter()
            .circle_filled(centre, 22.0, egui::Color32::from_black_alpha(140));
        ui.painter().text(
            centre,
            egui::Align2::CENTER_CENTER,
            "\u{25B6}",
            egui::FontId::proportional(22.0),
            egui::Color32::WHITE,
        );
        if let Some(ms) = duration_ms {
            let secs = ms / 1000;
            ui.painter().text(
                rect.right_bottom() - egui::vec2(6.0, 6.0),
                egui::Align2::RIGHT_BOTTOM,
                format!("{}:{:02}", secs / 60, secs % 60),
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }
    }

    /// Stand-in for a video with no still: name, length and a play badge, clickable
    /// into the player. Nothing is downloaded until the click.
    pub(in crate::app) fn render_video_poster(&mut self, ui: &mut egui::Ui, img: &ImageAttachment) {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(220.0, 124.0), egui::Sense::click());
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_gray(30));
        Self::paint_play_badge(ui, rect, img.duration_ms);
        ui.painter().text(
            rect.left_bottom() + egui::vec2(6.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            truncate_name(&img.name, 24),
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(200),
        );
        if crate::ui::clickable(resp)
            .on_hover_text("Play video")
            .clicked()
        {
            self.image_preview = Some(img.clone());
        }
    }

    /// Voice note / music row: round play button, seekable waveform, time.
    pub(in crate::app) fn render_audio(
        &mut self,
        ui: &mut egui::Ui,
        event_id: &str,
        audio: &AudioAttachment,
    ) {
        if !audio.is_voice && !audio.name.trim().is_empty() {
            ui.label(egui::RichText::new(&audio.name).small().weak());
        }
        if let Some(error) = self.audio_errors.get(event_id).cloned() {
            let resp = crate::ui::clickable(
                ui.label(egui::RichText::new(format!("Audio failed: {error} — retry")).small())
                    .on_hover_text("Download and play again"),
            );
            if resp.clicked() {
                self.audio_errors.remove(event_id);
                self.fetch_audio(event_id, audio);
            }
            return;
        }
        let Some(path) = self.audio_paths.get(&audio.mxc).cloned() else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Loading audio…").small().weak());
            });
            self.fetch_audio(event_id, audio);
            return;
        };
        let duration = audio
            .duration_ms
            .map(|ms| ms as f64 / 1000.0)
            .or_else(|| self.audio_lengths.get(event_id).copied())
            .unwrap_or(0.0);
        let bars: Vec<f32> = if audio.waveform.is_empty() {
            self.audio_waves.get(event_id).cloned().unwrap_or_default()
        } else {
            audio.waveform.clone()
        };
        let now = ui.ctx().input(|i| i.time);
        let active = self.audio.as_ref().is_some_and(|(id, _)| id == event_id);
        let (position, paused, finished) = match self.audio.as_mut() {
            Some((id, player)) if id == event_id => {
                let position = player.position(now);
                (position, player.is_paused(), player.finished)
            }
            _ => (0.0, true, false),
        };
        let mut toggle = false;
        let mut seek_to: Option<f64> = None;
        ui.horizontal(|ui| {
            let (icon, tip) = if active && !paused && !finished {
                ("\u{23F8}", "Pause")
            } else {
                ("\u{25B6}", "Play")
            };
            if ui
                .add(egui::Button::new(egui::RichText::new(icon).size(15.0)))
                .on_hover_text(tip)
                .clicked()
            {
                toggle = true;
            }
            let width = (ui.available_width() - 64.0).max(80.0);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(width, 36.0), egui::Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            let played = self.theme.gold();
            let unplayed = ui.visuals().weak_text_color().gamma_multiply(0.5);
            let n = bars.len().max(1);
            let progress = if duration > 0.0 {
                (position / duration).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            for (i, bar) in bars
                .iter()
                .chain(std::iter::repeat(&0.0))
                .take(n)
                .enumerate()
            {
                let h = (bar.clamp(0.0, 1.0) * 32.0).max(2.0);
                let x0 = rect.left() + i as f32 * rect.width() / n as f32;
                let bar_rect = egui::Rect::from_min_max(
                    egui::pos2(x0 + 1.0, rect.center().y - h / 2.0),
                    egui::pos2(
                        x0 + rect.width() / n as f32 - 1.0,
                        rect.center().y + h / 2.0,
                    ),
                );
                painter.rect_filled(
                    bar_rect,
                    1.0,
                    if (i as f32 / n as f32) < progress {
                        played
                    } else {
                        unplayed
                    },
                );
            }
            let wave = ui.interact(
                rect,
                ui.id().with(("audio-seek", event_id)),
                egui::Sense::click_and_drag(),
            );
            if duration > 0.0 && (wave.clicked() || wave.drag_stopped()) {
                if let Some(pointer) = wave.interact_pointer_pos() {
                    let fraction =
                        ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
                    seek_to = Some(fraction * duration);
                }
            }
            let shown = if duration > 0.0 {
                format!(
                    "{} / {}",
                    crate::audio::format_time(position),
                    crate::audio::format_time(duration)
                )
            } else {
                crate::audio::format_time(position)
            };
            ui.label(egui::RichText::new(shown).small().weak());
        });
        if toggle {
            match self.audio.as_mut() {
                Some((id, player)) if id == event_id => {
                    if player.finished {
                        player.seek(now, 0.0);
                        if player.is_paused() {
                            player.toggle_pause(now);
                        }
                    } else {
                        player.toggle_pause(now);
                    }
                }
                _ => match crate::audio::AudioPlayer::start(&path, now, 0.0, duration) {
                    Ok(player) => self.audio = Some((event_id.to_owned(), player)),
                    Err(e) => {
                        self.audio_errors.insert(event_id.to_owned(), e);
                    }
                },
            }
        }
        if let Some(target) = seek_to {
            match self.audio.as_mut() {
                Some((id, player)) if id == event_id => player.seek(now, target),
                _ => match crate::audio::AudioPlayer::start(&path, now, target, duration) {
                    Ok(player) => self.audio = Some((event_id.to_owned(), player)),
                    Err(e) => {
                        self.audio_errors.insert(event_id.to_owned(), e);
                    }
                },
            }
        }
        if active && !paused {
            ui.ctx().request_repaint();
        }
    }

    pub(in crate::app) fn render_image_preview(&mut self, ctx: &egui::Context) {
        let Some(image) = self.image_preview.clone() else {
            return;
        };
        let mut open = true;
        crate::ui::popup(ctx, &truncate_name(&image.name, 40))
            .open(&mut open)
            .resizable(true)
            .default_size(egui::vec2(760.0, 600.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if crate::ui::icon_button(ui, crate::ui::icons::DOWNLOAD, "Save the original")
                        .clicked()
                    {
                        self.download_image(&image);
                    }
                    ui.label(
                        egui::RichText::new(format!("{}×{}", image.w, image.h))
                            .small()
                            .weak(),
                    );
                });
                ui.separator();
                if image.is_video {
                    self.render_video(ui, &image);
                    return;
                }
                let cache_key = format!("original:{}", image.mxc);
                if let Some(handle) = self
                    .media
                    .original_for_source(&cache_key, image.source.clone())
                {
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::Image::new(&handle).shrink_to_fit());
                    });
                } else if let Some(error) = self.media.status(&cache_key).and_then(|e| e.error()) {
                    ui.colored_label(egui::Color32::RED, error);
                    if crate::ui::icon_button(ui, crate::ui::icons::REFRESH, "Retry").clicked() {
                        self.media.retry(&cache_key);
                    }
                } else {
                    ui.spinner();
                    ui.monospace("loading original…");
                }
            });
        if !open {
            self.image_preview = None;
            // Dropping the player kills ffmpeg so audio stops.
            self.video = None;
        }
    }

    /// Play video in the preview window; downloads to temp file first.
    pub(in crate::app) fn render_video(&mut self, ui: &mut egui::Ui, image: &ImageAttachment) {
        let ready = self
            .video_file
            .as_ref()
            .filter(|(mxc, _)| *mxc == image.mxc)
            .map(|(_, path)| path.clone());

        let Some(path) = ready else {
            self.fetch_video(image);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Downloading video…").small().weak());
            });
            return;
        };

        let now = ui.ctx().input(|i| i.time);
        if self.video.is_none() {
            // Sender dims are display size (rotation accounted).
            let hint = (image.w > 0 && image.h > 0).then_some((image.w, image.h));
            match crate::video::VideoPlayer::start(&path, now, hint) {
                Ok(player) => self.video = Some(player),
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Cannot play this video: {e}"),
                    );
                    ui.label(
                        egui::RichText::new(
                            "ffmpeg is required for in-app playback — install it, \
                             or save the file and open it elsewhere.",
                        )
                        .small()
                        .weak(),
                    );
                    return;
                }
            }
        }

        let Some(player) = self.video.as_mut() else {
            return;
        };
        let frame = player.frame(ui.ctx(), now);
        let position = player.position(now);
        let finished = player.finished;
        let paused = player.is_paused();

        match frame {
            Some(handle) => {
                ui.add(egui::Image::new(&handle).shrink_to_fit());
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Decoding…").small().weak());
                });
            }
        }
        let mut toggle = false;
        let mut restart = false;
        ui.horizontal(|ui| {
            use crate::ui::icons;
            // Finished clips offer replay; nothing left to resume.
            if finished {
                if crate::ui::icon_button(ui, icons::REFRESH, "Replay").clicked() {
                    restart = true;
                }
            } else {
                let (icon, tip) = if paused {
                    ("\u{25B6}", "Play")
                } else {
                    ("\u{23F8}", "Pause")
                };
                if ui
                    .add(egui::Button::new(egui::RichText::new(icon).size(15.0)))
                    .on_hover_text(tip)
                    .clicked()
                {
                    toggle = true;
                }
                if crate::ui::icon_button(ui, icons::REFRESH, "Start again").clicked() {
                    restart = true;
                }
            }
            let total = image.duration_ms.map(|ms| ms as f64 / 1000.0);
            let shown = match total {
                Some(t) => format!(
                    "{}:{:02} / {}:{:02}",
                    position as u64 / 60,
                    position as u64 % 60,
                    t as u64 / 60,
                    t as u64 % 60
                ),
                None => format!("{}:{:02}", position as u64 / 60, position as u64 % 60),
            };
            ui.label(egui::RichText::new(shown).small().weak());
        });
        if toggle {
            if let Some(player) = self.video.as_mut() {
                player.toggle_pause(now);
            }
        }
        if restart {
            // No seek on a stream; restart is a fresh decode.
            self.video = None;
        }
        // Repaint while running; paused clips need no frames.
        if !paused {
            ui.ctx().request_repaint();
        }
    }

    /// Pick files via native dialog; stage above input.
    pub(in crate::app) fn render_embeds(&mut self, ui: &mut egui::Ui, body: &str) {
        // Bail out fast when there are no links.
        if !body.contains("http") {
            return;
        }
        let urls: Vec<String> = body
            .split_whitespace()
            .filter(|w| w.starts_with("http"))
            .map(|w| w.trim_end_matches(['.', ',', ')', ']']).to_owned())
            .collect();
        for url in urls {
            if crate::embed::card_rule(&self.embed_rules, &url).is_none() {
                continue;
            }
            match self.embeds.get(&url).cloned() {
                Some(embed) => self.render_embed_card(ui, &embed),
                None => {
                    self.fetch_embed(url);
                }
            }
        }
    }

    /// One link card: author, text and a thumbnail.
    pub(in crate::app) fn render_embed_card(
        &mut self,
        ui: &mut egui::Ui,
        embed: &crate::embed::Embed,
    ) {
        let accent = self.theme.gold();
        let surface = ui.visuals().widgets.hovered.bg_fill;
        ui.add_space(3.0);
        egui::Frame::new()
            .fill(surface)
            .corner_radius(egui::CornerRadius::same(9))
            .inner_margin(egui::Margin::symmetric(10, 9))
            .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.28)))
            .show(ui, |ui| {
                // Bound to available width so narrow windows don't overflow.
                ui.set_max_width(ui.available_width().min(430.0));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    // Author avatar, fetched like other card images.
                    let avatar = embed
                        .avatar
                        .as_ref()
                        .and_then(|src| self.embed_image(ui.ctx(), src));
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    match avatar {
                        Some(handle) => {
                            let mut mesh = egui::Mesh::with_texture(handle.id());
                            // Circular, like other avatars.
                            let (c, r) = (rect.center(), rect.width() * 0.5);
                            mesh.vertices.push(egui::epaint::Vertex {
                                pos: c,
                                uv: egui::pos2(0.5, 0.5),
                                color: egui::Color32::WHITE,
                            });
                            const SEG: usize = 20;
                            for i in 0..=SEG {
                                let a = i as f32 / SEG as f32 * std::f32::consts::TAU;
                                let (s, co) = a.sin_cos();
                                mesh.vertices.push(egui::epaint::Vertex {
                                    pos: c + egui::vec2(co * r, s * r),
                                    uv: egui::pos2(0.5 + co * 0.5, 0.5 + s * 0.5),
                                    color: egui::Color32::WHITE,
                                });
                            }
                            for i in 1..=SEG as u32 {
                                mesh.indices.extend_from_slice(&[0, i, i + 1]);
                            }
                            ui.painter().add(egui::Shape::mesh(mesh));
                        }
                        None => {
                            ui.painter().circle_filled(
                                rect.center(),
                                rect.width() * 0.5,
                                accent.gamma_multiply(0.25),
                            );
                        }
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(egui::RichText::new(&embed.author).strong().size(13.5));
                        // Handle uses theme accent.
                        ui.label(egui::RichText::new(&embed.handle).size(11.5).color(accent));
                    });
                });
                ui.add_space(5.0);
                let text = embed.text.clone();
                self.emoji_text(ui, &text, 13.0, false);
                if let Some(src) = embed.image.clone() {
                    if let Some(handle) = self.embed_image(ui.ctx(), &src) {
                        ui.add_space(6.0);
                        // Bound to card width for narrow layouts.
                        let w = ui.available_width().min(410.0);
                        ui.add(
                            egui::Image::new(&handle)
                                .max_size(egui::vec2(w, 280.0))
                                .corner_radius(egui::CornerRadius::same(6)),
                        );
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    crate::ui::icon_label(
                        ui,
                        crate::ui::icons::LINK,
                        ui.visuals().weak_text_color(),
                    );
                    ui.hyperlink_to(
                        egui::RichText::new("Open original").small().weak(),
                        embed.link.clone(),
                    );
                });
            });
    }
}
