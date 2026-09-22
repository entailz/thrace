/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! The eframe render pass.

use crate::app::media::clipboard_has_image;
use crate::app::text::{mention_prefix, short_room, snippet, truncate_name};
use crate::app::{
    Member, PickerTab, ProfileAction, RoomEntry, RoomMenuAction, RoomNotify, SettingsTab, ThraceApp,
};

mod attachments;
mod avatar;
mod pickers;
mod richtext;
mod settings;

impl eframe::App for ThraceApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Ctrl+V in two halves: a browser image copy puts bitmap + URL on the
        // clipboard, egui-winit emits `Paste`, and the composer would swallow
        // the URL. On the press frame drop `Paste` when an image is present.
        let text_paste = ui
            .ctx()
            .input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        if text_paste && clipboard_has_image() {
            ui.ctx()
                .input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Paste(_))));
        }
        // No `Key::V` press is ever emitted (egui-winit returns early), and an
        // image-only clipboard emits no `Paste`; the release triggers the attach.
        let v_released = ui.ctx().input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command
                )
            })
        });
        if v_released {
            // Quiet: a plain text paste must not overwrite the status bar.
            self.paste_clipboard(false);
        }
        if let Some(rx) = &self.ignored_rx {
            if let Ok(list) = rx.try_recv() {
                self.ignored_users = list;
            }
        }
        // Link cards and thumbnails.
        loop {
            let got = match &self.embed_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some((url, embed)) = got else { break };
            self.embeds.insert(url, embed);
        }
        loop {
            let got = match &self.embed_img_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some((url, image)) = got else { break };
            let handle = image.map(|img| {
                ui.ctx()
                    .load_texture(&url, img, egui::TextureOptions::LINEAR)
            });
            self.embed_images.insert(url, handle);
        }
        // Older pages arriving from scrollback.
        loop {
            let page = match &self.page_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some((room_id, result)) = page else { break };
            match result {
                Ok((rows, token)) => self.apply_page(room_id, rows, token),
                Err(error) => {
                    self.paginating.remove(&room_id);
                    self.status = error;
                }
            }
        }
        // Downloaded video files for ffmpeg.
        if let Some(rx) = &self.video_rx {
            if let Ok((mxc, result)) = rx.try_recv() {
                match result {
                    Ok(path) => {
                        self.video = None;
                        self.video_file = Some((mxc, path));
                    }
                    Err(e) => self.status = format!("video: {e}"),
                }
            }
        }
        // Ready audio files for ffplay.
        let ready: Vec<_> = self
            .audio_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for (event_id, result) in ready {
            self.audio_pending.remove(&event_id);
            match result {
                Ok(ready) => {
                    self.audio_errors.remove(&event_id);
                    self.audio_paths.insert(ready.mxc.clone(), ready.path);
                    if ready.duration_secs > 0.0 {
                        self.audio_lengths
                            .insert(event_id.clone(), ready.duration_secs);
                    }
                    if let Some(wave) = ready.waveform {
                        self.audio_waves.insert(event_id, wave);
                    }
                }
                Err(e) => {
                    self.audio_errors.insert(event_id, e);
                }
            }
        }
        self.poll_login(ui.ctx());
        self.poll_verify();
        self.poll_send();
        self.poll_dm();
        self.poll_media(ui.ctx());
        self.poll_live_sync();
        self.poll_relations();
        self.pump_relations();
        self.poll_history();
        self.pump_history();
        // Incoming verification sync; starts once per login.
        self.ensure_verify_watch(ui.ctx());
        self.ensure_live_sync(ui.ctx());
        if let Some(result) = self
            .session_save_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.session_save_rx = None;
            self.session_save_task = None;
            match result {
                Ok(()) => {
                    self.session_warning = None;
                    self.status = "Login saved in wallet".into();
                }
                Err(error) => self.session_warning = Some(error),
            }
        }
        if let Some(warning) = self.session_warning.clone() {
            egui::Panel::top("session_storage_warning").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                    if ui
                        .add_enabled(
                            self.session_save_rx.is_none(),
                            egui::Button::new("Retry saving login"),
                        )
                        .clicked()
                    {
                        self.retry_session_save();
                    }
                });
            });
        }
        if self.client.is_none() {
            egui::Panel::top("login").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.monospace("Login");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.login.homeserver)
                            .hint_text("https://matrix.org")
                            .desired_width(160.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.login.username)
                            .hint_text("@you:hs")
                            .desired_width(130.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.login.password)
                            .hint_text("password")
                            .password(true)
                            .desired_width(110.0),
                    );
                    if ui
                        .add_enabled(!self.login.busy, egui::Button::new("Connect"))
                        .clicked()
                    {
                        self.start_login();
                    }
                    if ui
                        .add_enabled(!self.login.busy, egui::Button::new("Sso"))
                        .clicked()
                    {
                        let ctx = ui.ctx().clone();
                        self.start_sso(&ctx);
                    }
                    if self.login.busy {
                        ui.monospace("… SSO: browser → auto back, no paste");
                    }
                });
                // Manual fallback: only when loopback failed.
                if self.login.show_sso_token {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace("fallback token:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.login.sso_token_input)
                                .hint_text("loginToken or callback URL")
                                .desired_width(300.0),
                        );
                        if ui
                            .add_enabled(!self.login.busy, egui::Button::new("Finish"))
                            .clicked()
                        {
                            self.finish_sso_manual();
                        }
                    });
                }
                if !self.login.error.is_empty() {
                    ui.colored_label(egui::Color32::RED, egui::RichText::new(&self.login.error));
                }
            });
        }

        // Fake titlebar + security + settings.
        egui::Panel::top("titlebar").show(ui, |ui| {
            use crate::ui::icons;
            ui.horizontal(|ui| {
                if crate::ui::icon_button(
                    ui,
                    if self.sidebar_collapsed {
                        icons::CHEVRON_RIGHT
                    } else {
                        icons::CHEVRON_LEFT
                    },
                    if self.sidebar_collapsed {
                        "Expand sidebar"
                    } else {
                        "Collapse sidebar"
                    },
                )
                .clicked()
                {
                    self.sidebar_collapsed = !self.sidebar_collapsed;
                }
                let room = self.rooms.get(self.current);
                let name = room.map(|r| r.name.clone()).unwrap_or_else(|| "—".into());
                let is_dm = room.is_some_and(|r| r.is_dm);
                let muted = ui.visuals().weak_text_color();
                crate::ui::icon_label(ui, if is_dm { icons::DM } else { icons::HASH }, muted);
                ui.label(egui::RichText::new(name).strong().size(15.0));
                if !self.members.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("{} members", self.members.len()))
                            .small()
                            .weak(),
                    );
                }
                // DM peer for one-tap verification, like Element/Cinny's DM shield.
                let dm_peer: Option<String> = if is_dm {
                    self.members
                        .iter()
                        .find(|m| m.mxid != self.own_user)
                        .map(|m| m.mxid.clone())
                } else {
                    None
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // No fake window buttons; the WM draws the real ones.
                    if crate::ui::icon_toggle(ui, icons::COG, "Settings", self.show_settings)
                        .clicked()
                    {
                        self.show_settings = !self.show_settings;
                        if self.show_settings {
                            // Seed editable fields from current state.
                            let me = self.own_user.clone();
                            self.settings_display_name = self.display_for(&me);
                            self.refresh_devices();
                            self.refresh_ignored();
                        }
                    }
                    if crate::ui::icon_toggle(
                        ui,
                        icons::SHIELD,
                        "Security and device verification",
                        self.show_security,
                    )
                    .clicked()
                    {
                        self.show_security = !self.show_security;
                        if self.show_security {
                            self.refresh_devices();
                        }
                    }
                    // DM shield: verify this conversation's peer without
                    // hunting their profile card or typing an mxid.
                    if let Some(peer) = dm_peer {
                        if crate::ui::icon_button(ui, icons::VERIFIED, &format!("Verify {peer}"))
                            .clicked()
                        {
                            self.verify_user_input = peer;
                            self.show_security = true;
                            self.refresh_devices();
                        }
                    }
                });
            });
        });

        // Verification panel (Matrix spec SAS).
        if self.show_security {
            egui::Panel::top("security").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.monospace("[security] SAS verify per Matrix spec");
                    ui.add(egui::TextEdit::singleline(&mut self.verify_user_input).hint_text("@user:hs (blank = me)").desired_width(180.0));
                    if ui.small_button("Load devices").clicked() {
                        self.refresh_devices();
                    }
                    if ui.small_button("× [hide]").clicked() {
                        self.show_security = false;
                    }
                });
                ui.monospace("1. [verify] on a device → 2. accept on OTHER device → 3. compare 7 emoji + 3 numbers BOTH sides → [match] / [no match]");
                let mut to_verify: Option<(String, String)> = None;
                for d in self.devices.clone() {
                    ui.horizontal_wrapped(|ui| {
                        let shield = if d.verified { "●" } else { "○" };
                        let own = if d.is_own { " (this account)" } else { "" };
                        ui.monospace(format!("{shield} {}:{}{} {}", d.user_id, d.device_id, own, d.display_name.unwrap_or_default()));
                        if !d.verified && ui.small_button("Verify").clicked() {
                            to_verify = Some((d.user_id.clone(), d.device_id.clone()));
                        }
                    });
                }
                if let Some((u, dev)) = to_verify {
                    self.start_verify_device(&u, &dev);
                }
                if let Some(sas) = &self.sas {
                    ui.separator();
                    ui.monospace("DO THEY MATCH on both devices? If yes [match], else [no match]:");
                    ui.horizontal_wrapped(|ui| {
                        for (sym, desc) in &sas.emojis {
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(sym).size(28.0));
                                ui.monospace(egui::RichText::new(desc).small().weak());
                            });
                        }
                    });
                    ui.monospace(format!("numbers: {} – {} – {}", sas.decimals.0, sas.decimals.1, sas.decimals.2));
                    ui.horizontal(|ui| {
                        if ui.button("Match").clicked() {
                            self.verify_confirm(true);
                        }
                        if ui.button("No match").clicked() {
                            self.verify_confirm(false);
                        }
                    });
                }
            });
        }
        // Settings popup: theme, font size, session.
        if self.show_settings {
            let ctx = ui.ctx().clone();
            let mut close = false;
            let mut logout = false;
            let mut theme_pick: Option<String> = None;
            let mut open = true;
            crate::ui::popup(&ctx, "Settings")
                .open(&mut open)
                .default_size(egui::vec2(580.0, 460.0))
                .show(&ctx, |ui| {
                    ui.horizontal_top(|ui| {
                        // Section list down the left.
                        ui.vertical(|ui| {
                            ui.set_width(150.0);
                            for tab in SettingsTab::ALL {
                                let selected = self.settings_tab == tab;
                                let row = ui.horizontal(|ui| {
                                    crate::ui::icon_label(
                                        ui,
                                        tab.icon(),
                                        if selected {
                                            ui.visuals().selection.stroke.color
                                        } else {
                                            ui.visuals().weak_text_color()
                                        },
                                    );
                                    ui.add(
                                        egui::Label::new(if selected {
                                            egui::RichText::new(tab.label()).strong()
                                        } else {
                                            egui::RichText::new(tab.label())
                                        })
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                    )
                                    .clicked()
                                });
                                // Key by tab: auto ids from layout position can collide.
                                let whole_row = crate::ui::clickable(ui.interact(
                                    row.response.rect,
                                    ui.id().with(("settings-tab", tab.label())),
                                    egui::Sense::click(),
                                ));
                                if row.inner || whole_row.clicked() {
                                    self.settings_tab = tab;
                                }
                            }
                            ui.add_space(8.0);
                            ui.separator();
                            let danger = ui.visuals().error_fg_color;
                            if ui
                                .add(
                                    egui::Label::new(egui::RichText::new("Sign out").color(danger))
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                )
                                .clicked()
                            {
                                logout = true;
                            }
                        });
                        ui.separator();
                        ui.vertical(|ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_min_width(360.0);
                                    match self.settings_tab {
                                        SettingsTab::Account => self.settings_account(ui),
                                        SettingsTab::Appearance => {
                                            theme_pick = self.settings_appearance(ui)
                                        }
                                        SettingsTab::Notifications => {
                                            self.settings_notifications(ui)
                                        }
                                        SettingsTab::Sessions => self.settings_sessions(ui),
                                        SettingsTab::Privacy => self.settings_privacy(ui),
                                        SettingsTab::Emoji => self.settings_emoji(ui),
                                        SettingsTab::Links => self.settings_links(ui),
                                    }
                                });
                        });
                    });
                });
            if !open {
                close = true;
            }
            if let Some(name) = theme_pick {
                self.apply_settings_theme(&name, ui.ctx());
            }
            // Font slider applies live.
            self.apply_settings_font(ui.ctx());
            self.save_preferences();
            if logout {
                self.logout();
                close = true;
            }
            if close {
                self.show_settings = false;
            }
        }
        // Incoming verification: accept compares SAS emoji in security; decline cancels.
        if self.incoming.is_some() {
            let ctx = ui.ctx().clone();
            let mut accept = false;
            let mut decline = false;
            let (user, flow) = self
                .incoming
                .as_ref()
                .map(|r| (r.user_id.clone(), r.flow_id.clone()))
                .unwrap();
            let mut open = true;
            crate::ui::popup(&ctx, "Verify device")
                .open(&mut open)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(egui::RichText::new(format!("{user} wants to verify")).strong());
                    ui.label(
                        egui::RichText::new(
                            "Accept, then compare the seven emoji and three numbers on \
                             both devices.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.label(
                        egui::RichText::new(format!("Session {flow}"))
                            .small()
                            .weak(),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Accept").clicked() {
                            accept = true;
                        }
                        if ui.button("Decline").clicked() {
                            decline = true;
                        }
                    });
                });
            if accept {
                self.accept_incoming();
            } else if decline || !open {
                // Closing the window declines: leaving a verification request
                // half-open on the other device is worse than a clear no.
                self.decline_incoming();
            }
        }

        // Compact room sidebar (rooms + DMs).
        if !self.sidebar_collapsed {
            egui::Panel::left("rooms")
                .default_size(200.0)
                .min_size(140.0)
                .max_size(320.0)
                .show(ui, |ui| {
                    let mut clicked: Option<usize> = None;
                    let mut menu_for: Option<String> = None;
                    let accent = self.theme.gold();
                    let rooms = self.rooms.clone();
                    let cur = self.current;

                    // One closure for both lists, so Rooms and Direct stay in sync.
                    let previews = self.show_previews;
                    // Two lines need more height.
                    let row_h = if previews { 44.0 } else { 32.0 };
                    let mut avatar_jobs: Vec<(egui::Rect, Option<String>, String)> = Vec::new();
                    // Previews paint after the closure; the row closure lacks `&mut self`.
                    let mut previews_to_paint: Vec<(egui::Pos2, String, f32)> = Vec::new();

                    let mut section = |ui: &mut egui::Ui,
                                   title: &str,
                                   dm: bool,
                                   jobs: &mut Vec<(egui::Rect, Option<String>, String)>| {
                    let items: Vec<(usize, &RoomEntry)> = rooms
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.is_dm == dm)
                        .collect();
                    if items.is_empty() {
                        return;
                    }
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(title).small().weak().strong());
                    ui.add_space(2.0);
                    for (i, room) in items {
                        let selected = i == cur;
                        let resp = ui.add(
                            egui::Button::new("")
                                .frame(false)
                                .min_size(egui::vec2(ui.available_width(), row_h)),
                        );
                        let rect = resp.rect;
                        if selected || resp.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                egui::CornerRadius::same(7),
                                if selected {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    ui.visuals().widgets.hovered.bg_fill
                                },
                            );
                        }
                        // Unread/mentioned rows read louder than idle.
                        let fg = if room.mentioned {
                            accent
                        } else if selected || room.unread > 0 {
                            ui.visuals().text_color()
                        } else {
                            ui.visuals().text_color().gamma_multiply(0.82)
                        };

                        // Queue rects; avatars need `&mut self` the closure lacks.
                        let av = 26.0;
                        let av_rect = egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 8.0 + av / 2.0, rect.center().y),
                            egui::vec2(av, av),
                        );
                        jobs.push((av_rect, room.avatar_mxc.clone(), room.name.clone()));

                        let text_x = av_rect.right() + 9.0;
                        let badge_w = if room.unread > 0 { 30.0 } else { 6.0 };
                        let avail = (rect.right() - text_x - badge_w).max(20.0);
                        let name_y = if previews && room.preview.is_some() {
                            rect.center().y - 9.0
                        } else {
                            rect.center().y
                        };
                        ui.painter().text(
                            egui::pos2(text_x, name_y),
                            egui::Align2::LEFT_CENTER,
                            truncate_name(
                                room.name.trim_start_matches('@'),
                                (avail / 7.2).max(4.0) as usize,
                            ),
                            egui::FontId::proportional(13.5),
                            fg,
                        );
                        if previews {
                            if let Some(preview) = room.preview.clone() {
                                previews_to_paint.push((
                                    egui::pos2(text_x, rect.center().y + 10.0),
                                    truncate_name(&preview, (avail / 6.0).max(4.0) as usize),
                                    avail,
                                ));
                            }
                        }
                        if room.unread > 0 {
                            // Capped, so a busy room cannot widen the row.
                            let text = if room.unread > 99 {
                                "99+".to_owned()
                            } else {
                                room.unread.to_string()
                            };
                            let c = egui::pos2(rect.right() - 18.0, rect.center().y);
                            let w = 9.0 + text.len() as f32 * 3.0;
                            ui.painter().rect_filled(
                                egui::Rect::from_center_size(c, egui::vec2(w * 2.0, 17.0)),
                                egui::CornerRadius::same(9),
                                if room.mentioned {
                                    accent
                                } else {
                                    ui.visuals().widgets.hovered.bg_fill
                                },
                            );
                            ui.painter().text(
                                c,
                                egui::Align2::CENTER_CENTER,
                                text,
                                egui::FontId::proportional(10.5),
                                if room.mentioned {
                                    egui::Color32::BLACK
                                } else {
                                    ui.visuals().text_color()
                                },
                            );
                        }
                        // Test the pointer against the rect: no widget that
                        // could swallow the selecting left-click.
                        let secondary = ui.ctx().input(|inp| {
                            inp.pointer.secondary_clicked()
                                && inp
                                    .pointer
                                    .interact_pos()
                                    .is_some_and(|p| rect.contains(p))
                        });
                        if secondary {
                            menu_for = Some(room.room_id.clone());
                        }
                        if resp
                            .on_hover_text(format!("{}\nright-click for options", room.room_id))
                            .clicked()
                        {
                            clicked = Some(i);
                        }
                    }
                };

                    egui::ScrollArea::vertical().show(ui, |ui| {
                        section(ui, "ROOMS", false, &mut avatar_jobs);
                        section(ui, "DIRECT", true, &mut avatar_jobs);
                    });
                    // `self` is free again; paint the queued avatars.
                    for (rect, mxc, name) in avatar_jobs {
                        self.paint_avatar_at(ui, rect, &mxc, &name);
                    }
                    let muted = ui.visuals().weak_text_color();
                    for (pos, text, width) in previews_to_paint {
                        self.paint_text_with_emoji(ui, pos, &text, 11.0, muted, width);
                    }
                    if let Some(i) = clicked {
                        self.switch_room(i);
                    }
                    if let Some(id) = menu_for {
                        self.room_menu = Some((
                            id,
                            ui.ctx().pointer_latest_pos().unwrap_or_default(),
                            ui.ctx().cumulative_pass_nr(),
                        ));
                    }
                });
        }

        // Members (click → DM). Clamped width + truncated names keep the
        // panel from widening every frame on long display names.
        egui::Panel::right("nicks")
            .default_size(170.0)
            .min_size(120.0)
            .max_size(260.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("MEMBERS — {}", self.members.len()))
                        .small()
                        .weak()
                        .strong(),
                )
                .on_hover_text("click a member for their profile");
                ui.add_space(4.0);
                let mut dm_target: Option<(String, String)> = None;
                let mut profile_open: Option<(String, egui::Pos2)> = None;
                // Cloned once; the loop calls `&mut self` avatar methods.
                let members = std::mem::take(&mut self.members);
                for m in &members {
                    // Allocated rect, not a right-aligned button that overlays long names.
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 30.0),
                        egui::Sense::click(),
                    );
                    let hovered = ui.rect_contains_pointer(rect);
                    if hovered {
                        ui.painter().rect_filled(
                            rect,
                            egui::CornerRadius::same(7),
                            ui.visuals().widgets.hovered.bg_fill,
                        );
                    }
                    let av = 22.0;
                    let av_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 6.0 + av / 2.0, rect.center().y),
                        egui::vec2(av, av),
                    );
                    self.paint_avatar_at(ui, av_rect, &m.avatar_mxc, &m.display);
                    // Presence dot against the avatar.
                    ui.painter().circle_filled(
                        egui::pos2(av_rect.right() - 2.0, av_rect.bottom() - 2.0),
                        3.5,
                        if m.online {
                            self.theme.gold()
                        } else {
                            ui.visuals().weak_text_color()
                        },
                    );
                    // Reserve the DM button width only while hovered.
                    let text_x = av_rect.right() + 9.0;
                    let reserve = if hovered { 32.0 } else { 6.0 };
                    let avail = (rect.right() - text_x - reserve).max(20.0);
                    ui.painter().text(
                        egui::pos2(text_x, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        truncate_name(&m.display, (avail / 7.0).max(3.0) as usize),
                        egui::FontId::proportional(13.0),
                        self.nick_color(&m.display),
                    );
                    if hovered {
                        let btn = egui::Rect::from_center_size(
                            egui::pos2(rect.right() - 17.0, rect.center().y),
                            egui::vec2(26.0, 26.0),
                        );
                        if ui
                            .put(
                                btn,
                                egui::Button::new(
                                    egui::RichText::new(crate::ui::icons::CHAT).size(15.0),
                                ),
                            )
                            .on_hover_text(format!("Message {}", m.display))
                            .clicked()
                        {
                            dm_target = Some((m.mxid.clone(), m.display.clone()));
                        }
                    }
                    if resp
                        .on_hover_text(format!("{} ({}) — view profile", m.display, m.mxid))
                        .clicked()
                    {
                        let anchor = ui
                            .ctx()
                            .pointer_latest_pos()
                            .unwrap_or_else(|| rect.right_top());
                        profile_open = Some((m.mxid.clone(), anchor));
                    }
                }
                // Restore before `open_dm` / `profile_of` can see an empty list.
                self.members = members;
                if let Some((mxid, pos)) = profile_open {
                    self.profile_target = Some((mxid, pos, ui.ctx().cumulative_pass_nr()));
                }
                if let Some((mxid, display)) = dm_target {
                    self.open_dm(&mxid, &display);
                }
            });

        // Status bar.
        egui::Panel::bottom("status").show(ui, |ui| {
            use crate::ui::icons;
            ui.horizontal(|ui| {
                // Lead with a state dot.
                let (dot, tint, label) = if self.client.is_none() {
                    (icons::CIRCLE_SM, ui.visuals().weak_text_color(), "offline")
                } else if self.sync_running {
                    (icons::DOT, self.theme.gold(), "connected")
                } else {
                    (icons::LOADING, ui.visuals().weak_text_color(), "connecting")
                };
                crate::ui::icon_label(ui, dot, tint);
                ui.label(egui::RichText::new(&self.status).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Real numbers only; no decoration that measures nothing.
                    ui.label(
                        egui::RichText::new(format!(
                            "{label} · {} cached images",
                            self.media.resident()
                        ))
                        .small()
                        .weak(),
                    );
                });
            });
        });

        // Input with reply bar + slash palette.
        egui::Panel::bottom("input").show(ui, |ui| {
            if let Some(id) = self.replying_to.clone() {
                // Who and what, not the raw event id. Name and quote stay
                // apart so the quote can carry real emoji.
                let (who, quote) = self
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| (r.display_name.clone(), snippet(&r.body, 48)))
                    .unwrap_or_else(|| ("a message".into(), String::new()));
                let accent = self.theme.gold();
                let reply_size = self.reply_text_size();
                egui::Frame::new()
                    // Same ground as the timeline; not a raised surface.
                    .fill(ui.visuals().panel_fill)
                    .corner_radius(egui::CornerRadius::same(7))
                    .inner_margin(egui::Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // Coloured bar matches the timeline reply line.
                            let (bar, _) =
                                ui.allocate_exact_size(egui::vec2(2.0, 15.0), egui::Sense::hover());
                            ui.painter()
                                .rect_filled(bar, egui::CornerRadius::same(1), accent);
                            ui.label(egui::RichText::new("Replying to").size(reply_size).weak());
                            ui.label(egui::RichText::new(&who).size(reply_size).strong());
                            if !quote.trim().is_empty() {
                                self.reply_preview(ui, &quote);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if crate::ui::icon_button(
                                        ui,
                                        crate::ui::icons::CLOSE,
                                        "cancel reply",
                                    )
                                    .clicked()
                                    {
                                        self.replying_to = None;
                                    }
                                },
                            );
                        });
                    });
            }
            // Icons with tooltips; hints live in the placeholder and status bar.
            // Staged uploads: name + size + remove, sent with the message.
            if !self.pending_uploads.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    let mut remove: Option<usize> = None;
                    let muted = ui.visuals().weak_text_color();
                    // Borrowed: each staged file owns up to 25 MiB, copied every frame if cloned.
                    let staged = std::mem::take(&mut self.pending_uploads);
                    for (i, up) in staged.iter().enumerate() {
                        egui::Frame::new()
                            .fill(ui.visuals().widgets.hovered.bg_fill)
                            .corner_radius(egui::CornerRadius::same(7))
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    match self.upload_thumb(ui.ctx(), up) {
                                        Some(handle) => {
                                            ui.add(
                                                egui::Image::new(&handle)
                                                    .fit_to_exact_size(egui::vec2(26.0, 26.0))
                                                    .corner_radius(egui::CornerRadius::same(4)),
                                            );
                                        }
                                        None => {
                                            let icon = if up.is_image() {
                                                crate::ui::icons::IMAGE
                                            } else {
                                                crate::ui::icons::FILE
                                            };
                                            crate::ui::icon_label(ui, icon, muted);
                                        }
                                    }
                                    ui.label(
                                        egui::RichText::new(truncate_name(&up.name, 22)).small(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} KB",
                                            up.bytes.len() / 1024
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                    if crate::ui::icon_button(
                                        ui,
                                        crate::ui::icons::CLOSE,
                                        &format!("remove {}", up.name),
                                    )
                                    .clicked()
                                    {
                                        remove = Some(i);
                                    }
                                });
                            });
                    }
                    self.pending_uploads = staged;
                    if let Some(i) = remove {
                        // Drop the thumbnail too, or its texture stays resident.
                        let gone = self.pending_uploads.remove(i);
                        self.upload_thumbs.remove(&gone.name);
                    }
                });
            }
            // Drag-and-drop (egui reports drops per-frame).
            let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
            if !dropped.is_empty() {
                self.ingest_dropped(dropped);
            }
            if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
                ui.horizontal(|ui| {
                    crate::ui::icon_label(
                        ui,
                        crate::ui::icons::ATTACH,
                        ui.visuals().selection.stroke.color,
                    );
                    ui.label(egui::RichText::new("Drop to attach").small());
                });
            }
            ui.horizontal(|ui| {
                use crate::ui::icons;
                // Tooltip carries the caveat: Wayland/winit delivers no file drops.
                let attach_tip = format!("Attach a file\n{}", self.drop_hint);
                if crate::ui::icon_button(ui, icons::ATTACH, &attach_tip).clicked() {
                    self.pick_files();
                }
                if crate::ui::icon_button(ui, icons::PASTE, "Paste image from clipboard (Ctrl+V)")
                    .clicked()
                {
                    self.paste_clipboard(true);
                }

                let can_send = !self.input.trim().is_empty() || !self.pending_uploads.is_empty();
                // Reserve room so the field never squeezes off the trailing controls.
                let trailing = crate::ui::ICON_BUTTON * 3.0 + 40.0;
                // Up arrow on an empty composer edits your last message.
                if self.input.is_empty()
                    && self.editing.is_none()
                    && ui.input(|i| i.key_pressed(egui::Key::ArrowUp))
                {
                    self.start_edit_last();
                }
                if self.editing.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.editing = None;
                    self.input.clear();
                }
                // Name the room; the placeholder says where you type, not a manual.
                let hint = if self.editing.is_some() {
                    "Editing — Enter to save, Esc to cancel".to_owned()
                } else {
                    match self.rooms.get(self.current) {
                        Some(room) if room.is_dm => {
                            format!("Message {}", room.name.trim_start_matches('@'))
                        }
                        Some(room) => format!("Message {}", room.name),
                        None => "Message".to_owned(),
                    }
                };
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.input)
                        .id(Self::composer_id())
                        .hint_text(hint)
                        .desired_width((ui.available_width() - trailing).max(120.0))
                        .margin(egui::Margin::symmetric(10, 7)),
                );
                // Snapshot the @-mention state first: while the dropdown is
                // open Enter/Tab accept the highlight and hand focus back,
                // they never send a bare "@user" with an empty message.
                let mention_state: Option<(String, Vec<Member>)> = mention_prefix(&self.input)
                    .map(str::to_owned)
                    .map(|prefix| {
                        let hits = self.mention_hits(&prefix);
                        (prefix, hits)
                    });
                let mention_open = mention_state
                    .as_ref()
                    .is_some_and(|(_, hits)| !hits.is_empty());
                let enter_pressed =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                // Gated on composer focus: Tab belongs to other fields when
                // they hold it.
                let tab_pressed = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Tab));
                if mention_open {
                    let (prefix, hits) = mention_state.expect("open means state");
                    self.mention_selected %= hits.len();
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                        self.mention_selected = (self.mention_selected + 1) % hits.len();
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                        self.mention_selected =
                            (self.mention_selected + hits.len() - 1) % hits.len();
                    }
                    if enter_pressed || tab_pressed {
                        let m = hits[self.mention_selected % hits.len()].clone();
                        self.accept_mention(&m, &prefix);
                        Self::focus_composer(ui.ctx());
                    }
                } else if enter_pressed {
                    // A partial command completes; an exact verb runs straight away.
                    match self.slash_matches() {
                        Some((verb, matches)) if !matches.iter().any(|(c, _)| c[1..] == verb) => {
                            let idx = self.slash_selected % matches.len();
                            let cmd = matches[idx].0;
                            self.complete_slash(cmd);
                            resp.request_focus();
                        }
                        _ => self.send_current_input(),
                    }
                } else if tab_pressed {
                    // Tab completes a slash highlight without running it.
                    if let Some((_, matches)) = self.slash_matches() {
                        let selected = self.slash_selected % matches.len();
                        let cmd = matches[selected].0;
                        self.complete_slash(cmd);
                        resp.request_focus();
                    }
                }

                let picker_open = self.show_picker;
                if crate::ui::icon_toggle(
                    ui,
                    icons::EMOJI,
                    "Emoji, custom emoji and stickers",
                    picker_open,
                )
                .clicked()
                {
                    self.show_picker = !picker_open;
                    self.picker_opened_frame = ui.ctx().cumulative_pass_nr();
                    self.picker_tab = PickerTab::Emoji;
                }
                if crate::ui::icon_button(ui, icons::STICKER, "Stickers").clicked() {
                    self.show_picker = true;
                    self.picker_opened_frame = ui.ctx().cumulative_pass_nr();
                    self.picker_tab = PickerTab::Stickers;
                }
                // Send is the accent action; greys out when there is nothing to send.
                let tint = if can_send {
                    self.theme.gold()
                } else {
                    ui.visuals().weak_text_color()
                };
                if crate::ui::icon_button_tinted(ui, icons::SEND, "Send (Enter)", tint).clicked()
                    && can_send
                {
                    self.send_current_input();
                }
            });
            // @-mention autocomplete: word under the cursor starting with '@'.
            // Keys are handled beside the composer above (it owns `resp`);
            // this block only draws the list and takes clicks.
            if let Some(prefix) = mention_prefix(&self.input).map(str::to_owned) {
                // Owned: the `self.input` borrow would span the mutable uses below.
                let hits: Vec<Member> = self.mention_hits(&prefix);
                if !hits.is_empty() {
                    self.mention_selected %= hits.len();
                    let mut chosen: Option<Member> = None;
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Enter or Tab to mention")
                                .small()
                                .weak(),
                        );
                        for (i, m) in hits.iter().enumerate() {
                            let sel = i == self.mention_selected;
                            let row = ui.horizontal(|ui| {
                                let avatar = m.avatar_mxc.clone();
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(18.0, 18.0),
                                    egui::Sense::hover(),
                                );
                                self.paint_avatar_at(ui, rect, &avatar, &m.display);
                                let _ = ui.selectable_label(
                                    sel,
                                    egui::RichText::new(&m.display)
                                        .color(self.nick_color(&m.display)),
                                );
                                ui.label(egui::RichText::new(&m.mxid).small().weak());
                            });
                            let whole_row = crate::ui::clickable(ui.interact(
                                row.response.rect,
                                ui.id().with(("mention", m.mxid.as_str())),
                                egui::Sense::click(),
                            ));
                            if whole_row.clicked() {
                                chosen = Some(m.clone());
                            }
                        }
                    });
                    if let Some(m) = chosen {
                        // Full mxid, not the display name: that is what
                        // notifies the user and renders as a pill.
                        self.accept_mention(&m, &prefix);
                        Self::focus_composer(ui.ctx());
                    }
                }
            }
            if let Some((_, matches)) = self.slash_matches() {
                let selected = self.slash_selected % matches.len();
                // Tab is completed beside the composer (it owns `resp`);
                // arrows only move the highlight here.
                if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    self.slash_selected = (selected + 1) % matches.len();
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    self.slash_selected = (selected + matches.len() - 1) % matches.len();
                }
                let mut chosen: Option<&str> = None;
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Tab to complete · Enter to run")
                            .small()
                            .weak(),
                    );
                    for (i, (cmd, hint)) in matches.iter().enumerate() {
                        let sel = i == selected;
                        if ui
                            .selectable_label(sel, egui::RichText::new(format!("{cmd}  {hint}")))
                            .clicked()
                        {
                            chosen = Some(cmd);
                        }
                    }
                });
                if let Some(cmd) = chosen {
                    self.complete_slash(cmd);
                    Self::focus_composer(ui.ctx());
                }
            }
        });

        // Compact timeline. Logged out / syncing: spinner, not fake demo rooms.
        if self.client.is_none() || (self.rows.is_empty() && self.login.busy) {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.3);
                    let frame = (ui.ctx().cumulative_pass_nr() / 8) % 4;
                    let spinner = ["◐", "◓", "◑", "◒"][frame as usize];
                    ui.monospace(egui::RichText::new(spinner).size(48.0));
                    ui.monospace(if self.login.busy {
                        "syncing rooms…"
                    } else {
                        "log in to load your rooms"
                    });
                    if !self.login.busy {
                        ui.monospace("password or SSO above");
                    }
                });
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                if let Some(room_id) = self.current_room_id() {
                    if self.history_queue.has_failed(&room_id) {
                        ui.horizontal(|ui| {
                            ui.label("Could not load messages.");
                            if ui.button("Retry").clicked() {
                                self.history_queue.select(&room_id);
                                self.pump_history();
                            }
                        });
                    } else if !self.history_queue.is_loaded(&room_id) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Loading messages...");
                        });
                    }
                }
                let scroll = egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    // Fill the panel; a shrunk scroll area clips rows and strands the scrollbar.
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Scrollback indicator at the very top, for this room only.
                        if self
                            .current_room_id()
                            .is_some_and(|id| self.paginating.contains(&id))
                        {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    egui::RichText::new("Loading older messages…")
                                        .small()
                                        .weak(),
                                );
                            });
                        } else if self.current_room_id().is_some_and(|id| {
                            self.history_queue.is_loaded(&id) && !self.back_tokens.contains_key(&id)
                        }) {
                            ui.label(egui::RichText::new("Beginning of the room").small().weak());
                        }
                        let mut toggle: Option<(usize, String)> = None;
                        let mut reply_to: Option<String> = None;
                        let mut react_open: Option<(String, egui::Pos2)> = None;
                        let mut profile_open: Option<(String, egui::Pos2)> = None;
                        let mut jump_to: Option<String> = None;
                        // Real events on screen this frame, in timeline order. Drives
                        // reaction backfill and the read receipt.
                        let mut visible: Vec<String> = Vec::new();
                        // Consumed by whichever row matches it this frame.
                        let scroll_target = self.scroll_to_event.take();
                        let now = ui.ctx().input(|i| i.time);
                        let highlight = self
                            .highlight_event
                            .as_ref()
                            .filter(|(_, until)| *until > now)
                            .map(|(id, _)| id.clone());
                        let frame_nr = ui.ctx().cumulative_pass_nr();
                        // Spelled out so a method-resolution change cannot silently deep-copy again.
                        let rows = std::rc::Rc::clone(&self.rows);
                        for (ri, row) in rows.iter().enumerate() {
                            let row_id = row.id.clone();

                            // Live target row first; plain-text quote fallback when not loaded.
                            let reply_view: Option<(String, String, Option<String>)> = row
                                .reply_to_id
                                .as_ref()
                                .and_then(|id| self.rows.iter().find(|r| &r.id == id))
                                .map(|t| {
                                    (
                                        t.display_name.clone(),
                                        snippet(&t.body, 80),
                                        t.avatar_mxc.clone(),
                                    )
                                })
                                .or_else(|| {
                                    row.reply_to.clone().map(|(who, snip)| (who, snip, None))
                                });
                            // Filter our own receipt; it is not news.
                            let me = self.own_user.clone();
                            let seen: Vec<String> =
                                row.seen_by.iter().filter(|u| **u != me).cloned().collect();
                            // Gutter only with receipts, so ordinary messages keep full width.
                            let gutter = if seen.is_empty() { 0.0 } else { 86.0 };
                            let row_resp = ui
                                .horizontal_top(|ui| {
                                    // Avatar: cached mxc texture, else colored initial.
                                    let amxc = row.avatar_mxc.clone();
                                    let dname = row.display_name.clone();
                                    let sender = row.sender.clone();
                                    let avatar_resp = self
                                        .render_avatar(ui, &amxc, &dname)
                                        .on_hover_text(format!("{} — view profile", row.sender));
                                    if avatar_resp.clicked() {
                                        profile_open = Some((
                                            sender.clone(),
                                            ui.ctx()
                                                .pointer_latest_pos()
                                                .unwrap_or_else(|| avatar_resp.rect.right_top()),
                                        ));
                                    }
                                    let content_w = (ui.available_width() - gutter).max(140.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(content_w, 0.0),
                                        egui::Layout::top_down(egui::Align::LEFT),
                                        |ui| {
                                            // Tight gap; the default left messages double-spaced.
                                            ui.spacing_mut().item_spacing.y = 1.0;
                                            // Header: name + time + actions.
                                            ui.horizontal_wrapped(|ui| {
                                                let name_resp = ui
                                                    .add(
                                                        egui::Label::new(
                                                            egui::RichText::new(&row.display_name)
                                                                .strong()
                                                                .color(
                                                                    self.nick_color(
                                                                        &row.display_name,
                                                                    ),
                                                                ),
                                                        )
                                                        .selectable(false)
                                                        .sense(egui::Sense::click()),
                                                    )
                                                    .on_hover_text(format!(
                                                        "{sender} — view profile"
                                                    ));
                                                if name_resp.clicked() {
                                                    profile_open = Some((
                                                        sender.clone(),
                                                        ui.ctx()
                                                            .pointer_latest_pos()
                                                            .unwrap_or_else(|| {
                                                                name_resp.rect.right_top()
                                                            }),
                                                    ));
                                                }
                                                if !row.ts.is_empty() {
                                                    ui.label(
                                                        egui::RichText::new(&row.ts).weak().small(),
                                                    );
                                                }
                                                if row.edited {
                                                    ui.label(
                                                        egui::RichText::new("(edited)")
                                                            .weak()
                                                            .small(),
                                                    )
                                                    .on_hover_text("this message was edited");
                                                }
                                                if row.is_sticker {
                                                    crate::ui::icon_label(
                                                        ui,
                                                        crate::ui::icons::STICKER,
                                                        ui.visuals().weak_text_color(),
                                                    )
                                                    .on_hover_text("sticker");
                                                }
                                                // No header buttons; actions live in right-click, reply on double-click.
                                            });
                                            // Reply chain.
                                            if let Some((who, snip, avatar)) = reply_view.clone() {
                                                // Accent bar + "In reply to" + avatar + name.
                                                let colour = self.nick_color(&who);
                                                let reply_size = self.reply_text_size();
                                                let line = ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 4.0;
                                                    let (bar, _) = ui.allocate_exact_size(
                                                        egui::vec2(2.0, 15.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    ui.painter().rect_filled(
                                                        bar,
                                                        egui::CornerRadius::same(1),
                                                        colour,
                                                    );
                                                    ui.label(
                                                        egui::RichText::new("In reply to")
                                                            .size(reply_size)
                                                            .weak(),
                                                    );
                                                    let (av, _) = ui.allocate_exact_size(
                                                        egui::vec2(14.0, 14.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    let ring = ui.visuals().panel_fill;
                                                    self.paint_avatar_circle(
                                                        ui, av, &avatar, &who, ring,
                                                    );
                                                    ui.label(
                                                        egui::RichText::new(&who)
                                                            .size(reply_size)
                                                            .color(colour)
                                                            .strong(),
                                                    );
                                                    if !snip.trim().is_empty() {
                                                        let text = snippet(&snip, 70);
                                                        self.reply_preview(ui, &text);
                                                    }
                                                });
                                                // Hover-only labels inside, so this steals no clicks.
                                                // Explicit per-event id: auto ids from layout position collide.
                                                let hit = crate::ui::clickable(ui.interact(
                                                    line.response.rect,
                                                    ui.id().with(("reply-jump", row_id.as_str())),
                                                    egui::Sense::click(),
                                                ))
                                                .on_hover_text("Go to the original message");
                                                if hit.clicked() {
                                                    jump_to = row.reply_to_id.clone();
                                                }
                                            }
                                            // Body / image (cached textures; rich markdown).
                                            if let Some(img) = row.image.clone() {
                                                self.render_image(ui, &img);
                                                if !row.body.is_empty() {
                                                    let b = row.body.clone();
                                                    let f = row.formatted.clone();
                                                    self.render_body(ui, &b, f.as_deref());
                                                }
                                            } else if row.is_sticker {
                                                match row.image.clone() {
                                                    Some(img) => {
                                                        self.render_image(ui, &img);
                                                        if !row.body.is_empty() {
                                                            let b = row.body.clone();
                                                            let f = row.formatted.clone();
                                                            self.render_body(ui, &b, f.as_deref());
                                                        }
                                                    }
                                                    None => {
                                                        let b = row.body.clone();
                                                        ui.colored_label(
                                                            self.theme.gold(),
                                                            egui::RichText::new(&b),
                                                        );
                                                    }
                                                }
                                            } else if let Some(audio) = row.audio.clone() {
                                                self.render_audio(ui, &row_id, &audio);
                                            } else {
                                                let b = row.body.clone();
                                                let f = row.formatted.clone();
                                                self.render_body(ui, &b, f.as_deref());
                                                self.render_embeds(ui, &b);
                                            }
                                            // Reactions grouped by key; click toggles ours.
                                            if !row.reactions.is_empty() {
                                                let own_user = self
                                                    .client
                                                    .as_ref()
                                                    .and_then(|c| c.user_id())
                                                    .map(|u| u.to_string())
                                                    .unwrap_or_default();
                                                ui.horizontal_wrapped(|ui| {
                                                    for r in &row.reactions {
                                                        let owned = r.owns(&own_user);
                                                        // Resolved here so the hover closure avoids a second `self` borrow.
                                                        let reactors: Vec<(
                                                            String,
                                                            Option<String>,
                                                        )> = r
                                                            .senders
                                                            .iter()
                                                            .map(|s| {
                                                                let (name, avatar, _) =
                                                                    self.profile_of(&s.user);
                                                                (name, avatar)
                                                            })
                                                            .collect();
                                                        // mxc key: MSC4027 custom emoji as image;
                                                        // `:shortcode:` is the legacy bridge form.
                                                        let custom = if r.key.starts_with("mxc://")
                                                        {
                                                            Some(r.key.clone())
                                                        } else if r.key.starts_with(':') {
                                                            self.packs
                                                                .resolve(&r.key)
                                                                .map(|i| i.mxc_url.clone())
                                                        } else {
                                                            None
                                                        };
                                                        let texture = custom
                                                            .and_then(|mxc| {
                                                                self.media.texture_for(
                                                                    "emoji",
                                                                    &mxc,
                                                                    Some((24, 24)),
                                                                )
                                                            })
                                                            .or_else(|| {
                                                                self.emoji_font.texture_at_size(
                                                                    ui.ctx(),
                                                                    &r.key,
                                                                    18.0,
                                                                )
                                                            });
                                                        // Owned state rides on the chip fill/outline, not extra glyphs.
                                                        // A plain frame, not a Button: buttons restyle on
                                                        // hover, which resized the chip and nudged the
                                                        // timeline up/down. This frame is identical
                                                        // hovered or not, so rows never shift.
                                                        let accent = self.theme.gold();
                                                        // The old light squircle becomes the 1px
                                                        // border; the fill goes darker behind it.
                                                        let light = ui
                                                            .visuals()
                                                            .widgets
                                                            .hovered
                                                            .bg_fill;
                                                        let (fill, stroke) = if owned {
                                                            (
                                                                accent.gamma_multiply(0.22),
                                                                egui::Stroke::new(1.0, accent),
                                                            )
                                                        } else {
                                                            (
                                                                ui.visuals()
                                                                    .widgets
                                                                    .inactive
                                                                    .bg_fill
                                                                    .gamma_multiply(0.55),
                                                                egui::Stroke::new(1.0, light),
                                                            )
                                                        };
                                                        // Bare count; "x5" wastes characters.
                                                        let count = r.count().to_string();
                                                        let key = r.key.clone();
                                                        let frame = egui::Frame::new()
                                                            .fill(fill)
                                                            .stroke(stroke)
                                                            .corner_radius(
                                                                egui::CornerRadius::same(9),
                                                            )
                                                            .inner_margin(
                                                                egui::Margin::symmetric(8, 4),
                                                            )
                                                            .show(ui, |ui| {
                                                                ui.horizontal(|ui| {
                                                                    ui.spacing_mut().item_spacing.x =
                                                                        4.0;
                                                                    match &texture {
                                                                        Some(h) => {
                                                                            ui.add(
                                                                                egui::Image::new(h)
                                                                                    .fit_to_exact_size(
                                                                                        egui::vec2(
                                                                                            18.0,
                                                                                            18.0,
                                                                                        ),
                                                                                    ),
                                                                            );
                                                                        }
                                                                        // No bitmap: raw key (unicode or shortcode).
                                                                        None => {
                                                                            ui.label(
                                                                                egui::RichText::new(
                                                                                    &key,
                                                                                ),
                                                                            );
                                                                        }
                                                                    }
                                                                    ui.label(
                                                                        egui::RichText::new(&count),
                                                                    );
                                                                });
                                                            });
                                                        let resp = crate::ui::clickable(ui.interact(
                                                            frame.response.rect,
                                                            ui.id().with((
                                                                "reaction",
                                                                ri,
                                                                r.key.as_str(),
                                                            )),
                                                            egui::Sense::click(),
                                                        ))
                                                            .on_hover_ui(|ui| {
                                                                ui.spacing_mut().item_spacing.y =
                                                                    1.0;
                                                                let unknown = r
                                                                    .bundled
                                                                    .saturating_sub(
                                                                        r.senders.len(),
                                                                    );
                                                                if reactors.is_empty()
                                                                    && unknown == 0
                                                                {
                                                                    ui.label(
                                                                        egui::RichText::new(
                                                                            "nobody yet",
                                                                        )
                                                                        .small()
                                                                        .weak(),
                                                                    );
                                                                }
                                                                let ring = ui.visuals().panel_fill;
                                                                for (name, avatar) in &reactors {
                                                                    ui.horizontal(|ui| {
                                                                        let (rect, _) = ui
                                                                            .allocate_exact_size(
                                                                                egui::vec2(
                                                                                    16.0, 16.0,
                                                                                ),
                                                                                egui::Sense::hover(
                                                                                ),
                                                                            );
                                                                        self.paint_avatar_circle(
                                                                            ui, rect, avatar, name,
                                                                            ring,
                                                                        );
                                                                        ui.label(
                                                                            egui::RichText::new(
                                                                                name,
                                                                            )
                                                                            .small(),
                                                                        );
                                                                    });
                                                                }
                                                                if unknown > 0 {
                                                                    ui.label(
                                                                        egui::RichText::new(
                                                                            format!(
                                                                                "+{unknown} more"
                                                                            ),
                                                                        )
                                                                        .small()
                                                                        .weak(),
                                                                    );
                                                                }
                                                            });
                                                        if resp.clicked() {
                                                            toggle = Some((ri, r.key.clone()));
                                                        }
                                                    }
                                                });
                                            }
                                            // Threads unimplemented (nothing sets `thread_count`); seen-by dots below.
                                        },
                                    );
                                    // Receipts share a right gutter; their own row stretched every message.
                                    if !seen.is_empty() {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::TOP),
                                            |ui| self.render_seen_by(ui, &seen),
                                        );
                                    }
                                })
                                .response;

                            // Right-click should also work in the empty space beside narrow content.
                            // Only the hit-test extends; child widgets keep their left-clicks.
                            let row_hit_rect = egui::Rect::from_min_max(
                                row_resp.rect.min,
                                egui::pos2(ui.max_rect().right(), row_resp.rect.max.y),
                            );

                            // Right-click opens the react picker at the pointer. Never a
                            // click-sensing row rect: last-registered-wins would swallow
                            // child link/reaction clicks. Same for double-click-to-reply.
                            let double = ui.ctx().input(|i| {
                                i.pointer
                                    .button_double_clicked(egui::PointerButton::Primary)
                                    && i.pointer
                                        .interact_pos()
                                        .is_some_and(|p| row_hit_rect.contains(p))
                            });
                            if double {
                                reply_to = Some(row_id.clone());
                            }
                            let hit_row = ui.ctx().input(|i| {
                                i.pointer.secondary_clicked()
                                    && i.pointer
                                        .interact_pos()
                                        .is_some_and(|p| row_hit_rect.contains(p))
                            });
                            if hit_row {
                                react_open = Some((
                                    row_id.clone(),
                                    ui.ctx()
                                        .pointer_latest_pos()
                                        .unwrap_or_else(|| row_resp.rect.left_top()),
                                ));
                            }
                            // Landed from a reply jump: scroll into view and flash.
                            if scroll_target.as_deref() == Some(row_id.as_str()) {
                                row_resp.scroll_to_me(Some(egui::Align::Center));
                            }
                            if highlight.as_deref() == Some(row_id.as_str()) {
                                ui.painter().rect_filled(
                                    row_resp.rect.expand2(egui::vec2(4.0, 2.0)),
                                    egui::CornerRadius::same(6),
                                    self.theme.gold().gamma_multiply(0.16),
                                );
                            }
                            // Local echoes and system notices carry no relations and
                            // cannot hold a receipt.
                            if row.sender != "system"
                                && row_id.starts_with('$')
                                && ui.is_rect_visible(row_resp.rect)
                            {
                                visible.push(row_id.clone());
                            }
                            ui.add_space(6.0);
                        }
                        if let Some(target) = jump_to {
                            // Only when the target is loaded.
                            if self.rows.iter().any(|r| r.id == target) {
                                self.highlight_event = Some((target.clone(), now + 1.6));
                                self.scroll_to_event = Some(target);
                            } else if self
                                .current_room_id()
                                .is_some_and(|id| self.back_tokens.contains_key(&id))
                            {
                                // Not loaded yet: fetch the next page and say so.
                                self.paginate_current();
                                self.status = "loading older messages to find it…".into();
                            } else {
                                self.status = "that message is no longer available".into();
                            }
                        }
                        // `scroll_to_me` animates over frames; keep repainting or the scroll stalls mid-way.
                        if self.scroll_to_event.is_some()
                            || self
                                .highlight_event
                                .as_ref()
                                .is_some_and(|(_, until)| *until > now)
                        {
                            ui.ctx().request_repaint();
                        }
                        if let Some((ri, key)) = toggle {
                            self.send_reaction(ri, key);
                        }
                        if let Some(id) = reply_to {
                            self.replying_to = Some(id);
                        }
                        if let Some((event_id, pos)) = react_open {
                            self.react_target = Some((event_id, pos, frame_nr));
                        }
                        if let Some((mxid, pos)) = profile_open {
                            self.profile_target = Some((mxid, pos, frame_nr));
                        }
                        // Scroll to content end; egui animates rather than jumping.
                        if std::mem::take(&mut self.jump_to_latest) {
                            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                        }
                        visible
                    });

                let content_h = scroll.content_size.y;
                // Prepended pages shift the offset by the added height, or the reader jumps.
                if let Some(before) = self.pending_scroll_fix.take() {
                    let delta = content_h - before;
                    if delta > 0.0 {
                        let mut state = scroll.state;
                        state.offset.y += delta;
                        state.store(ui.ctx(), scroll.id);
                    }
                }
                self.last_content_height = content_h;
                // Near the top with more to fetch: one request via the paginate guard.
                if scroll.state.offset.y < 120.0 && content_h > 0.0 {
                    self.paginate_current();
                }
                // Mark read only what the reader has actually reached. Needs this
                // frame's visible set, so it runs here rather than off a sync batch.
                self.send_current_read_receipt(scroll.inner.last().map(String::as_str));

                // Distance from newest, in rows: images and one-liners differ in pixels.
                let view_h = scroll.inner_rect.height();
                let below = (content_h - scroll.state.offset.y - view_h).max(0.0);
                let row_h = if self.rows.is_empty() {
                    0.0
                } else {
                    content_h / self.rows.len() as f32
                };
                const ROWS_BEFORE_JUMP: f32 = 30.0;
                let hidden_rows = if row_h > 0.0 { below / row_h } else { 0.0 };
                if hidden_rows >= ROWS_BEFORE_JUMP {
                    let area = scroll.inner_rect;
                    let size = 38.0;
                    let btn = egui::Rect::from_min_size(
                        egui::pos2(area.right() - size - 18.0, area.bottom() - size - 14.0),
                        egui::vec2(size, size),
                    );
                    // Round, so it reads as floating action, not timeline.
                    ui.painter().circle_filled(
                        btn.center(),
                        size * 0.5,
                        ui.visuals().widgets.hovered.bg_fill,
                    );
                    ui.painter().circle_stroke(
                        btn.center(),
                        size * 0.5,
                        egui::Stroke::new(1.0, self.theme.gold().gamma_multiply(0.5)),
                    );
                    let resp = ui.put(
                        btn,
                        egui::Button::new(
                            egui::RichText::new(crate::ui::icons::CHEVRON_DOWN)
                                .size(20.0)
                                .color(self.theme.gold()),
                        )
                        .frame(false),
                    );
                    if resp
                        .on_hover_text(format!(
                            "Jump to latest — {} messages below",
                            hidden_rows as usize
                        ))
                        .clicked()
                    {
                        self.jump_to_latest = true;
                        // Repaint only covers the animation, not the whole scrolled-up stay.
                        self.jump_until = ui.ctx().input(|i| i.time) + 0.8;
                    }
                    if ui.ctx().input(|i| i.time) < self.jump_until {
                        ui.ctx().request_repaint();
                    }
                }
            });
        }
        // Full react picker. Anchored by `fixed_pos` near the click, clamped on-screen.
        if let Some((target_id, anchor, opened)) = self.react_target.clone() {
            let ctx = ui.ctx().clone();
            let mut close = false;
            let mut pick: Option<String> = None;
            let mut reply = false;
            let mut delete = false;
            let mut copy = false;
            // Re-resolve every frame; a stored index drifts under sync batches.
            let ri = self.rows.iter().position(|r| r.id == target_id);
            let preview = ri
                .and_then(|i| self.rows.get(i))
                .map(|r| format!("{} — {}", r.display_name, snippet(&r.body, 40)))
                .unwrap_or_default();
            // Escape closes.
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let viewport = ctx.viewport_rect();
            let pos = egui::pos2(
                (anchor.x + 16.0).clamp(
                    viewport.min.x + 8.0,
                    (viewport.max.x - 320.0).max(viewport.min.x + 8.0),
                ),
                (anchor.y - 40.0).clamp(
                    viewport.min.y + 8.0,
                    (viewport.max.y - 300.0).max(viewport.min.y + 8.0),
                ),
            );
            let mut open = true;
            let window = crate::ui::popup(&ctx, "React")
                .open(&mut open)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    if !preview.is_empty() {
                        ui.monospace(format!("reacting to {preview}"));
                    }
                    ui.horizontal(|ui| {
                        use crate::ui::icons;
                        if crate::ui::icon_button(ui, icons::REPLY, "Reply").clicked() {
                            reply = true;
                            close = true;
                        }
                        if crate::ui::icon_button(ui, icons::COPY, "Copy text").clicked() {
                            copy = true;
                            close = true;
                        }
                        if ri.is_some() {
                            // Tinted and apart: destructive.
                            let danger = ui.visuals().error_fg_color;
                            if crate::ui::icon_button_tinted(
                                ui,
                                icons::DELETE,
                                "Delete this message",
                                danger,
                            )
                            .clicked()
                            {
                                delete = true;
                                close = true;
                            }
                        }
                    });
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for e in [
                            "\u{1F44D}",
                            "\u{2764}\u{FE0F}",
                            "\u{1F602}",
                            "\u{1F389}",
                            "\u{1F440}",
                            "\u{1F622}",
                        ] {
                            if self.emoji_widget(ui, e, 22.0).clicked() {
                                self.note_recent_emoji(e);
                                pick = Some(e.into());
                                close = true;
                            }
                        }
                    });
                    ui.separator();
                    // Same order as the composer picker: emoji, custom, stickers.
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        for (tab, name) in [
                            (PickerTab::Emoji, "Emoji"),
                            (PickerTab::Custom, "Custom"),
                            (PickerTab::Stickers, "Stickers"),
                        ] {
                            if ui.selectable_label(self.picker_tab == tab, name).clicked() {
                                self.picker_tab = tab;
                            }
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.picker_query)
                                .hint_text("search")
                                .desired_width(110.0),
                        );
                    });
                    let query = self.picker_query.trim().to_lowercase();
                    match self.picker_tab {
                        // The grid scrolls itself; an outer scroll area would nest two of them.
                        PickerTab::Emoji => {
                            if let Some(e) = self.emoji_grid(ui, 240.0) {
                                pick = Some(e);
                                close = true;
                            }
                        }
                        PickerTab::Stickers => {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    let stickers: Vec<(String, String)> = self
                                        .packs
                                        .stickers()
                                        .map(|(_, i)| (i.shortcode.clone(), i.mxc_url.clone()))
                                        .collect();
                                    ui.horizontal_wrapped(|ui| {
                                        for (sc, mxc) in stickers {
                                            if let Some(h) = self.media.texture_for(
                                                "sticker",
                                                &mxc,
                                                Some((64, 64)),
                                            ) {
                                                if ui
                                                    .add(
                                                        egui::Image::new(&h)
                                                            .max_height(44.0)
                                                            .sense(egui::Sense::click()),
                                                    )
                                                    .on_hover_text(&sc)
                                                    .clicked()
                                                {
                                                    pick = Some(format!(":{sc}:"));
                                                    close = true;
                                                }
                                            }
                                        }
                                    });
                                });
                        }
                        PickerTab::Custom => {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    for pack in self.packs.packs().to_vec() {
                                        let hits: Vec<_> = pack
                                            .images
                                            .iter()
                                            .filter(|i| {
                                                query.is_empty()
                                                    || i.shortcode.to_lowercase().contains(&query)
                                            })
                                            .cloned()
                                            .collect();
                                        if hits.is_empty() {
                                            continue;
                                        }
                                        ui.label(
                                            egui::RichText::new(&pack.display_name)
                                                .small()
                                                .weak()
                                                .strong(),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            for img in hits {
                                                let hit = match self.media.texture_for(
                                                    "emoji",
                                                    &img.mxc_url,
                                                    Some((24, 24)),
                                                ) {
                                                    Some(h) => ui
                                                        .add(
                                                            egui::Image::new(&h)
                                                                .max_height(22.0)
                                                                .sense(egui::Sense::click()),
                                                        )
                                                        .on_hover_text(format!(
                                                            ":{}:",
                                                            img.shortcode
                                                        ))
                                                        .clicked(),
                                                    None => ui
                                                        .button(format!(":{}:", img.shortcode))
                                                        .clicked(),
                                                };
                                                if hit {
                                                    pick = Some(format!(":{}:", img.shortcode));
                                                    close = true;
                                                }
                                            }
                                        });
                                    }
                                });
                        }
                    }
                })
                .map(|w| w.response.rect);
            if !open {
                close = true;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    close = true;
                }
            }
            if let (Some(key), Some(i)) = (pick, ri) {
                self.send_reaction(i, key);
                close = true;
            }
            if reply {
                self.replying_to = Some(target_id.clone());
            }
            if copy {
                let text = ri
                    .and_then(|i| self.rows.get(i))
                    .map(|r| r.body.clone())
                    .unwrap_or_default();
                self.status = match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                    Ok(()) => "copied".into(),
                    Err(e) => format!("copy failed: {e}"),
                };
            }
            if delete {
                self.delete_message(target_id.clone());
            }
            // Target scrolled out: nothing to react to.
            if close || ri.is_none() {
                self.react_target = None;
            }
        }

        // Room menu (sidebar right-click).
        if let Some((room_id, anchor, opened)) = self.room_menu.clone() {
            let ctx = ui.ctx().clone();
            let mut close = false;
            let mut chosen: Option<RoomMenuAction> = None;
            let room = self.rooms.iter().find(|r| r.room_id == room_id).cloned();
            let name = room
                .as_ref()
                .map(|r| r.name.clone())
                .unwrap_or_else(|| short_room(&room_id));
            let unread = room.as_ref().is_some_and(|r| r.unread > 0);
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let viewport = ctx.viewport_rect();
            let pos = egui::pos2(
                (anchor.x + 8.0).clamp(
                    viewport.min.x + 8.0,
                    (viewport.max.x - 220.0).max(viewport.min.x + 8.0),
                ),
                (anchor.y).clamp(
                    viewport.min.y + 8.0,
                    (viewport.max.y - 300.0).max(viewport.min.y + 8.0),
                ),
            );
            let mut open = true;
            let window = crate::ui::popup(&ctx, "Room")
                .open(&mut open)
                .resizable(false)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    use crate::ui::icons;
                    ui.set_min_width(190.0);
                    ui.label(egui::RichText::new(truncate_name(&name, 26)).strong());
                    ui.separator();

                    let item = |ui: &mut egui::Ui, icon: &str, text: &str| {
                        ui.horizontal(|ui| {
                            crate::ui::icon_label(ui, icon, ui.visuals().weak_text_color());
                            ui.add(
                                egui::Label::new(text)
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .clicked()
                        })
                        .inner
                    };

                    if unread {
                        if item(ui, icons::CHECK, "Mark as read") {
                            chosen = Some(RoomMenuAction::MarkRead);
                        }
                    } else if item(ui, icons::DOT, "Mark as unread") {
                        chosen = Some(RoomMenuAction::MarkUnread);
                    }
                    if item(ui, icons::CHECK, "Toggle favourite") {
                        chosen = Some(RoomMenuAction::Favourite(true));
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Notifications").small().weak());
                    for mode in [RoomNotify::All, RoomNotify::MentionsOnly, RoomNotify::Mute] {
                        let icon = match mode {
                            RoomNotify::Mute => icons::CIRCLE_SM,
                            _ => icons::DOT,
                        };
                        if item(ui, icon, mode.label()) {
                            chosen = Some(RoomMenuAction::Notify(mode));
                        }
                    }
                    ui.separator();
                    if item(ui, icons::AT, "Invite someone") {
                        chosen = Some(RoomMenuAction::Invite);
                    }
                    if item(ui, icons::COPY, "Copy room link") {
                        chosen = Some(RoomMenuAction::CopyLink);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        let danger = ui.visuals().error_fg_color;
                        crate::ui::icon_label(ui, icons::LOGOUT, danger);
                        if ui
                            .add(
                                egui::Label::new(egui::RichText::new("Leave room").color(danger))
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .clicked()
                        {
                            chosen = Some(RoomMenuAction::Leave);
                        }
                    });
                })
                .map(|w| w.response.rect);
            if !open {
                close = true;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    close = true;
                }
            }
            if let Some(action) = chosen {
                // Favourite toggles; resolve current state here, not in the menu.
                let action = match action {
                    RoomMenuAction::Favourite(_) => {
                        let on = !self.favourites.contains(&room_id);
                        if on {
                            self.favourites.insert(room_id.clone());
                        } else {
                            self.favourites.remove(&room_id);
                        }
                        RoomMenuAction::Favourite(on)
                    }
                    other => other,
                };
                self.room_menu_action(room_id.clone(), action);
                close = true;
            }
            if close {
                self.room_menu = None;
            }
        }

        // Profile card. Anchored at the click like the picker, clamped on-screen.
        if let Some((mxid, anchor, opened)) = self.profile_target.clone() {
            let ctx = ui.ctx().clone();
            let (display, avatar, online) = self.profile_of(&mxid);
            let shared = self.shared_rooms(&mxid);
            let is_self = mxid == self.own_user;
            let mut close = false;
            let mut action: Option<ProfileAction> = None;
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let viewport = ctx.viewport_rect();
            let pos = egui::pos2(
                (anchor.x + 16.0).clamp(
                    viewport.min.x + 8.0,
                    (viewport.max.x - 300.0).max(viewport.min.x + 8.0),
                ),
                (anchor.y - 20.0).clamp(
                    viewport.min.y + 8.0,
                    (viewport.max.y - 260.0).max(viewport.min.y + 8.0),
                ),
            );
            let mut open = true;
            let window = crate::ui::popup(&ctx, "Profile")
                .open(&mut open)
                .resizable(false)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        self.render_avatar_sized(ui, &avatar, &display, 72.0);
                        ui.vertical(|ui| {
                            ui.colored_label(
                                self.nick_color(&display),
                                egui::RichText::new(&display).heading().strong(),
                            );
                            ui.monospace(egui::RichText::new(&mxid).small().weak());
                            ui.monospace(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    if online { "●" } else { "○" },
                                    if online { "online" } else { "offline" }
                                ))
                                .small()
                                .weak(),
                            );
                            if shared > 0 {
                                ui.monospace(
                                    egui::RichText::new(format!(
                                        "{shared} shared room{}",
                                        if shared == 1 { "" } else { "s" }
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                        });
                    });
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        // No DM/verify actions for yourself.
                        use crate::ui::icons;
                        if !is_self {
                            if crate::ui::icon_button(ui, icons::CHAT, "Message").clicked() {
                                action = Some(ProfileAction::Message);
                            }
                            if crate::ui::icon_button(ui, icons::VERIFIED, "Verify devices")
                                .clicked()
                            {
                                action = Some(ProfileAction::Verify);
                            }
                        }
                        if crate::ui::icon_button(ui, icons::AT, "Mention in the composer")
                            .clicked()
                        {
                            action = Some(ProfileAction::Mention);
                        }
                        if crate::ui::icon_button(ui, icons::COPY, "Copy the full mxid").clicked() {
                            action = Some(ProfileAction::CopyId);
                        }
                    });
                })
                .map(|w| w.response.rect);
            if !open {
                close = true;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    close = true;
                }
            }
            match action {
                Some(ProfileAction::Message) => {
                    self.open_dm(&mxid, &display);
                    close = true;
                }
                Some(ProfileAction::Mention) => {
                    if !self.input.is_empty() && !self.input.ends_with(' ') {
                        self.input.push(' ');
                    }
                    // Full mxid: display names don't notify and don't pill-render.
                    self.input.push_str(&format!("{mxid} "));
                    Self::focus_composer(ui.ctx());
                    close = true;
                }
                Some(ProfileAction::Verify) => {
                    self.verify_user_input = mxid.clone();
                    self.show_security = true;
                    self.refresh_devices();
                    close = true;
                }
                Some(ProfileAction::CopyId) => {
                    self.status = match arboard::Clipboard::new()
                        .and_then(|mut c| c.set_text(mxid.clone()))
                    {
                        Ok(()) => format!("copied {mxid}"),
                        Err(e) => format!("copy failed: {e}"),
                    };
                    close = true;
                }
                None => {}
            }
            if close {
                self.profile_target = None;
            }
        }

        // Emoji picker.
        if self.show_picker {
            let ctx = ui.ctx().clone();
            let opened = self.picker_opened_frame;
            let mut picker_open = true;
            let window = crate::ui::popup(&ctx, "Emoji")
                .open(&mut picker_open)
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        // Tabs sit apart so the three destinations read as separate targets.
                        ui.spacing_mut().item_spacing.x = 12.0;
                        for (tab, name) in [
                            (PickerTab::Emoji, "Emoji"),
                            (PickerTab::Custom, "Custom"),
                            (PickerTab::Stickers, "Stickers"),
                        ] {
                            if ui.selectable_label(self.picker_tab == tab, name).clicked() {
                                self.picker_tab = tab;
                            }
                        }
                    });
                    // The window owns the only search field; the emoji grid adds none of its own.
                    let hint = match self.picker_tab {
                        PickerTab::Emoji => "Search emoji",
                        PickerTab::Custom => "Search custom emoji",
                        PickerTab::Stickers => "Search stickers",
                    };
                    ui.add(
                        egui::TextEdit::singleline(&mut self.picker_query)
                            .hint_text(hint)
                            .desired_width(f32::INFINITY),
                    );
                    let query = self.picker_query.trim().to_lowercase();
                    match self.picker_tab {
                        // The grid scrolls itself; an outer scroll area would nest two of them.
                        PickerTab::Emoji => {
                            if let Some(e) = self.emoji_grid(ui, 300.0) {
                                self.input.push_str(&e);
                            }
                        }
                        PickerTab::Custom => {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for pack in self.packs.packs() {
                                        let hits: Vec<_> = pack
                                            .images
                                            .iter()
                                            .filter(|i| {
                                                query.is_empty()
                                                    || i.shortcode.to_lowercase().contains(&query)
                                            })
                                            .cloned()
                                            .collect();
                                        // A pack with nothing matching keeps its header out of the way.
                                        if hits.is_empty() {
                                            continue;
                                        }
                                        ui.label(
                                            egui::RichText::new(&pack.display_name)
                                                .small()
                                                .weak()
                                                .strong(),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            for img in hits {
                                                let mxc = img.mxc_url.clone();
                                                let sc = img.shortcode.clone();
                                                // Ready / pending / failed red shortcode; click retries.
                                                let key = format!("emoji:{mxc}");
                                                let hit = match (
                                                    self.media.texture_for(
                                                        "emoji",
                                                        &mxc,
                                                        Some((24, 24)),
                                                    ),
                                                    self.media.status(&key),
                                                ) {
                                                    (Some(h), _) => ui
                                                        .add(
                                                            egui::Image::new(&h)
                                                                .max_height(20.0)
                                                                .sense(egui::Sense::click()),
                                                        )
                                                        .on_hover_text(format!(":{sc}:"))
                                                        .clicked(),
                                                    (None, Some(entry))
                                                        if entry.error().is_some() =>
                                                    {
                                                        // Click retries; never auto-requeue per frame.
                                                        if ui
                                                            .button(format!(":{sc}:"))
                                                            .on_hover_text(
                                                                "failed — click to retry",
                                                            )
                                                            .clicked()
                                                        {
                                                            self.media.retry(&key);
                                                        }
                                                        false
                                                    }
                                                    (None, _) => ui
                                                        .button(crate::ui::icons::IMAGE)
                                                        .on_hover_text(format!("loading :{sc}:…"))
                                                        .clicked(),
                                                };
                                                if hit {
                                                    self.input.push_str(&format!(":{sc}: "));
                                                }
                                            }
                                        });
                                    }
                                });
                        }
                        PickerTab::Stickers => {
                            let stickers: Vec<(String, String)> = self
                                .packs
                                .stickers()
                                .filter(|(_, i)| {
                                    query.is_empty() || i.shortcode.to_lowercase().contains(&query)
                                })
                                .map(|(_, i)| (i.shortcode.clone(), i.mxc_url.clone()))
                                .collect();
                            if stickers.is_empty() {
                                ui.monospace(if query.is_empty() {
                                    "no sticker packs — stickers live in packs with sticker usage"
                                } else {
                                    "no stickers match"
                                });
                            }
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        for (sc, mxc) in stickers {
                                            let hit = if let Some(h) = self.media.texture_for(
                                                "sticker",
                                                &mxc,
                                                Some((64, 64)),
                                            ) {
                                                ui.add(
                                                    egui::Image::new(&h)
                                                        .max_height(48.0)
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(format!(":{sc}:"))
                                                .clicked()
                                            } else {
                                                ui.button(format!("[{sc}]")).clicked()
                                            };
                                            if hit {
                                                self.input = format!("/sticker {sc}");
                                                self.send_current_input();
                                                self.show_picker = false;
                                            }
                                        }
                                    });
                                });
                        }
                    }
                })
                .map(|w| w.response.rect);
            if !picker_open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.show_picker = false;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    self.show_picker = false;
                }
            }
        }
        self.render_image_preview(ui.ctx());
    }
}
