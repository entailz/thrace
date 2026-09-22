/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! The settings panes.

use crate::app::{Moderation, RoomNotify, ThraceApp};

impl ThraceApp {
    // Settings sections.

    /// Profile and credentials.
    pub(in crate::app) fn settings_account(&mut self, ui: &mut egui::Ui) {
        ui.heading("Account");
        let mxid = self
            .client
            .as_ref()
            .and_then(|c| c.user_id())
            .map(|u| u.to_string());
        let Some(mxid) = mxid else {
            ui.label("Not signed in.");
            return;
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
            let ring = ui.visuals().panel_fill;
            // Our own avatar, taken from whichever room has us in its
            // member list — no extra request needed.
            let avatar = self.profile_of(&mxid).1;
            let name = self.display_for(&mxid);
            self.paint_avatar_circle(ui, rect, &avatar, &name, ring);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(&mxid).strong());
                let homeserver = mxid.split(':').nth(1).unwrap_or("?").to_owned();
                ui.label(egui::RichText::new(homeserver).small().weak());
            });
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Display name").small().weak());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.settings_display_name).desired_width(220.0),
            );
            if ui.button("Save").clicked() {
                let name = self.settings_display_name.clone();
                self.set_nick(name);
            }
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Changing your password signs out your other sessions on some servers — \
                                      do it from your homeserver's account page.",
            )
            .small()
            .weak(),
        );
    }

    /// Theme, font size, message previews.
    pub(in crate::app) fn settings_appearance(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let mut pick = None;
        ui.heading("Appearance");
        ui.label(egui::RichText::new("Theme").small().weak());
        ui.horizontal_wrapped(|ui| {
            for name in ["dark", "midnight", "bbs-amber", "win98"] {
                if ui
                    .selectable_label(self.settings_theme == name, name)
                    .clicked()
                {
                    pick = Some(name.to_owned());
                }
            }
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!("Font size — {:.0}pt", self.settings_font_size))
                .small()
                .weak(),
        );
        ui.add(egui::Slider::new(&mut self.settings_font_size, 10.0..=24.0).step_by(1.0));
        ui.add_space(8.0);
        ui.checkbox(
            &mut self.show_previews,
            "Show the newest message under each room",
        );
        ui.add_space(10.0);
        ui.separator();
        // CC-BY 4.0 attribution for bundled emoji artwork.
        ui.label(
            egui::RichText::new(
                "Emoji: Twemoji © Twitter/X, CC-BY 4.0 — font build by \
                 mozilla/twemoji-colr (Apache-2.0)",
            )
            .small()
            .weak(),
        );
        pick
    }

    /// Default notification behaviour.
    pub(in crate::app) fn settings_notifications(&mut self, ui: &mut egui::Ui) {
        ui.heading("Notifications");
        ui.label(
            egui::RichText::new(
                "Defaults for new rooms. Per-room settings live in the room's \
                 right-click menu and override these.",
            )
            .small()
            .weak(),
        );
        ui.add_space(8.0);
        let mut chosen: Option<(bool, RoomNotify)> = None;
        for (one_to_one, heading) in [(false, "Group rooms"), (true, "Direct messages")] {
            ui.label(egui::RichText::new(heading).strong());
            ui.horizontal_wrapped(|ui| {
                for mode in [RoomNotify::All, RoomNotify::MentionsOnly, RoomNotify::Mute] {
                    if ui.button(mode.label()).clicked() {
                        chosen = Some((one_to_one, mode));
                    }
                }
            });
            ui.add_space(6.0);
        }
        if let Some((one_to_one, mode)) = chosen {
            self.set_default_notifications(one_to_one, mode);
        }
    }

    /// Signed-in devices.
    pub(in crate::app) fn settings_sessions(&mut self, ui: &mut egui::Ui) {
        ui.heading("Sessions");
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Verified sessions can read your encrypted history.")
                    .small()
                    .weak(),
            );
            if crate::ui::icon_button(ui, crate::ui::icons::REFRESH, "Refresh").clicked() {
                self.refresh_devices();
            }
        });
        ui.add_space(6.0);
        if self.devices.is_empty() {
            ui.label(
                egui::RichText::new("No sessions loaded yet.")
                    .small()
                    .weak(),
            );
        }
        let devices = self.devices.clone();
        for d in devices {
            ui.horizontal(|ui| {
                let (icon, tint) = if d.verified {
                    (crate::ui::icons::VERIFIED, self.theme.gold())
                } else {
                    (crate::ui::icons::ALERT, ui.visuals().error_fg_color)
                };
                crate::ui::icon_label(ui, icon, tint);
                ui.vertical(|ui| {
                    let name = d
                        .display_name
                        .clone()
                        .unwrap_or_else(|| d.device_id.clone());
                    ui.label(if d.is_own {
                        egui::RichText::new(format!("{name} (this session)")).strong()
                    } else {
                        egui::RichText::new(name)
                    });
                    ui.label(egui::RichText::new(&d.device_id).small().weak());
                });
                if !d.is_own && !d.verified && ui.button("Verify").clicked() {
                    self.start_verify_device(&d.user_id, &d.device_id);
                }
            });
            ui.separator();
        }
    }

    /// Ignored users and encryption state.
    pub(in crate::app) fn settings_privacy(&mut self, ui: &mut egui::Ui) {
        ui.heading("Security & Privacy");
        ui.label(egui::RichText::new("Ignored users").strong());
        ui.label(
            egui::RichText::new("You will not see messages from these people.")
                .small()
                .weak(),
        );
        if self.ignored_users.is_empty() {
            ui.label(egui::RichText::new("Nobody is ignored.").small().weak());
        }
        let ignored = self.ignored_users.clone();
        for mxid in ignored {
            ui.horizontal(|ui| {
                ui.label(&mxid);
                if ui.button("Un-ignore").clicked() {
                    self.moderate(mxid.clone(), Moderation::Unignore);
                    self.ignored_users.retain(|u| *u != mxid);
                }
            });
        }
        ui.add_space(10.0);
        ui.separator();
        ui.label(egui::RichText::new("Encryption").strong());
        ui.label(
            egui::RichText::new(
                "Messages in encrypted rooms are decrypted on this device. Verify \
                 your other sessions so they can read history too.",
            )
            .small()
            .weak(),
        );
        if ui.button("Open device verification").clicked() {
            self.show_security = true;
            self.refresh_devices();
        }
    }

    /// Custom emoji and sticker packs.
    pub(in crate::app) fn settings_emoji(&mut self, ui: &mut egui::Ui) {
        ui.heading("Emoji & Stickers");
        let packs = self.packs.packs().to_vec();
        if packs.is_empty() {
            ui.label(
                egui::RichText::new(
                    "No packs. Packs come from your account (im.ponies.user_emotes) \
                     and from rooms you are in.",
                )
                .small()
                .weak(),
            );
            return;
        }
        ui.label(
            egui::RichText::new(format!(
                "{} pack(s), {} images",
                packs.len(),
                packs.iter().map(|p| p.images.len()).sum::<usize>()
            ))
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        for pack in packs {
            egui::CollapsingHeader::new(&pack.display_name)
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for img in &pack.images {
                            let tip = format!(":{}:", img.shortcode);
                            match self
                                .media
                                .texture_for("emoji", &img.mxc_url, Some((32, 32)))
                            {
                                Some(h) => {
                                    ui.add(egui::Image::new(&h).max_height(28.0))
                                        .on_hover_text(&tip);
                                }
                                None => {
                                    ui.label(egui::RichText::new(&tip).small().weak());
                                }
                            }
                        }
                    });
                });
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Packs are read-only here for now: adding and removing images writes \
                 account data and room state, which is not wired up yet.",
            )
            .small()
            .weak(),
        );
    }

    /// Per-platform link embedding.
    pub(in crate::app) fn settings_links(&mut self, ui: &mut egui::Ui) {
        ui.heading("Link previews");
        ui.label(
            egui::RichText::new(
                "Links to sites that block previews can be opened through a \
                 front end instead, and shown inline.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);
        // Privacy trade-off; kept visible on purpose.
        ui.label(
            egui::RichText::new(
                "Fetching a preview asks that front end for a link someone \
                 sent you, which tells its operator your IP address and what \
                 you are reading. Rewriting alone sends nothing until you \
                 click. Both are off until you turn them on.",
            )
            .small()
            .color(ui.visuals().warn_fg_color),
        );
        ui.add_space(8.0);

        // Seed edit buffers on first use / rule added.
        if self.embed_host_bufs.len() != self.embed_rules.len() {
            self.embed_host_bufs = self
                .embed_rules
                .iter()
                .map(|r| r.hosts.join(", "))
                .collect();
        }
        let mut changed = false;
        for i in 0..self.embed_rules.len() {
            let name = self.embed_rules[i].name.clone();
            egui::CollapsingHeader::new(&name)
                .default_open(true)
                .show(ui, |ui| {
                    let rule = &mut self.embed_rules[i];
                    changed |= ui
                        .checkbox(&mut rule.enabled, "Enabled")
                        .on_hover_text(
                            "rewrite links, and show cards when the front end has an API",
                        )
                        .changed();
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Open links with").small().weak());
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut rule.open_with)
                                .hint_text("fxtwitter.com")
                                .desired_width(220.0),
                        )
                        .changed();
                    ui.label(
                        egui::RichText::new("Preview API (blank for rewrite only)")
                            .small()
                            .weak(),
                    );
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut rule.api)
                                .hint_text("https://api.fxtwitter.com")
                                .desired_width(220.0),
                        )
                        .changed();
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Hostnames to match, comma separated — add the \
                             front ends people post from, since those are the \
                             same links.",
                        )
                        .small()
                        .weak(),
                    );
                    // Buffer persists in state so typed separators survive re-parse.
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.embed_host_bufs[i])
                                .desired_width(280.0),
                        )
                        .changed()
                    {
                        rule.hosts = self.embed_host_bufs[i]
                            .split(',')
                            .map(|h| h.trim().to_lowercase())
                            .filter(|h| !h.is_empty())
                            .collect();
                        changed = true;
                    }
                });
        }
        if changed {
            // Cards depend on the front end; refetch after rule changes.
            self.embeds.clear();
            self.embed_images.clear();
        }
    }
}
