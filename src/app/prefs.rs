/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Saved preferences: theme, font, emoji history, notification and privacy defaults.

use crate::app::session::{restore_blocking, session_path};
use crate::app::{LoginMsg, RoomNotify, SendResult, ThraceApp};
use crate::theme::ThemeFile;

impl ThraceApp {
    /// Save client preferences without runtime state.
    pub(in crate::app) fn save_preferences(&mut self) {
        let config = crate::config::Config {
            theme: self.config_theme.clone(),
            font_size: self.settings_font_size,
            show_previews: self.show_previews,
            embeds: crate::config::Embeds {
                rules: self.embed_rules.clone(),
            },
        };
        if let Err(error) = self.config_file.save(config) {
            self.status = format!("Could not save settings: {error:#}");
        }
    }

    /// Remember a pick for the "frequently used" row.
    pub(in crate::app) fn note_recent_emoji(&mut self, emoji: &str) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.emoji_usage.record(emoji);
        self.recent_emoji = self.emoji_usage.frequent(18);
        if self.emoji_save_tx.is_none() {
            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::recent_emoji::RecentEmoji>();
            let errors = self.send_channel();
            let ctx = self.ctx.clone();
            self.emoji_save_task = Some(self.rt.spawn(async move {
                while let Some(mut usage) = rx.recv().await {
                    // Serialize writes and coalesce rapid picks so an older save cannot win.
                    while let Ok(newer) = rx.try_recv() {
                        usage = newer;
                    }
                    if let Err(error) = usage.save(&client).await {
                        let _ = errors.send(SendResult::Failed(format!(
                            "Could not save emoji usage: {error}"
                        )));
                        ctx.request_repaint();
                    }
                }
            }));
            self.emoji_save_tx = Some(tx);
        }
        let _ = self
            .emoji_save_tx
            .as_ref()
            .unwrap()
            .send(self.emoji_usage.clone());
    }

    pub(in crate::app) fn stop_emoji_saving(&mut self) {
        self.emoji_save_tx = None;
        if let Some(task) = self.emoji_save_task.take() {
            task.abort();
        }
        self.emoji_usage = Default::default();
        self.recent_emoji.clear();
    }

    /// Load ignored users from account data.
    pub(in crate::app) fn refresh_ignored(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.ignored_tx();
        self.rt.spawn(async move {
            use matrix_sdk::ruma::events::ignored_user_list::IgnoredUserListEventContent;
            let list = client
                .account()
                .account_data::<IgnoredUserListEventContent>()
                .await
                .ok()
                .flatten()
                .and_then(|raw| raw.deserialize().ok())
                .map(|c| c.ignored_users.keys().map(|u| u.to_string()).collect())
                .unwrap_or_default();
            let _ = tx.send(list);
        });
    }

    /// Lazily create the ignored-list channel.
    pub(in crate::app) fn ignored_tx(&mut self) -> std::sync::mpsc::Sender<Vec<String>> {
        if self.ignored_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.ignored_tx = Some(tx);
            self.ignored_rx = Some(rx);
        }
        self.ignored_tx.clone().unwrap()
    }

    /// Set the default notification mode for group rooms or DMs.
    pub(in crate::app) fn set_default_notifications(&mut self, one_to_one: bool, mode: RoomNotify) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::notification_settings::{IsEncrypted, IsOneToOne};
            let settings = client.notification_settings().await;
            let one = if one_to_one {
                IsOneToOne::Yes
            } else {
                IsOneToOne::No
            };
            // Covers encrypted and unencrypted rooms alike.
            let mut last = Ok(());
            for enc in [IsEncrypted::Yes, IsEncrypted::No] {
                last = settings
                    .set_default_room_notification_mode(enc, one, mode.sdk())
                    .await;
                if last.is_err() {
                    break;
                }
            }
            match last {
                Ok(()) => SendResult::Done(format!("default: {}", mode.label())),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// Apply selected skin tone on insert.
    pub(in crate::app) fn emoji_with_skin(&self, base: &str, supports_skin: bool) -> String {
        if supports_skin && !self.skin_tone.is_empty() {
            format!("{base}{}", self.skin_tone)
        } else {
            base.to_owned()
        }
    }

    /// Retry remembering an in-memory login after the user starts or unlocks a wallet.
    pub(in crate::app) fn retry_session_save(&mut self) {
        if self.session_save_rx.is_some() {
            return;
        }
        let Some(metadata) = self.session_metadata.clone() else {
            return;
        };
        let Some(session) = self
            .client
            .as_ref()
            .and_then(|client| client.matrix_auth().session())
        else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.session_save_rx = Some(rx);
        let ctx = self.ctx.clone();
        self.session_save_task = Some(self.rt.spawn(async move {
            let result = crate::session_store::save(&session_path(), &metadata, &session)
                .await
                .map_err(|error| format!("Login is not saved: {error:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        }));
    }

    /// Restore cached session on startup; reports via PasswordDone.
    pub fn try_restore_cached(&mut self) {
        if self.client.is_some() || self.login_rx.is_some() {
            return;
        }
        if !session_path().exists() {
            return;
        }
        self.login.busy = true;
        self.status = "restoring cached session …".into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        self.spawn_blocking_task(move || {
            let res = restore_blocking();
            let _ = tx.send(LoginMsg::PasswordDone(res));
        });
    }

    /// Apply theme: reload builtin TOML, apply live.
    pub(in crate::app) fn apply_settings_theme(&mut self, name: &str, ctx: &egui::Context) {
        match ThemeFile::load_builtin(name) {
            Ok(theme) => {
                crate::theme::apply_theme(ctx, &theme);
                self.theme = theme;
                self.settings_theme = name.to_owned();
                self.config_theme = name.to_owned();
                self.status = format!("theme → {name}");
            }
            Err(e) => {
                self.status = format!("theme {name} failed: {e}");
            }
        }
    }

    /// Apply font size via `theme.font.mono_size` + re-apply.
    pub(in crate::app) fn apply_settings_font(&mut self, ctx: &egui::Context) {
        let size = self.settings_font_size.clamp(10.0, 24.0);
        if (self.theme.font.mono_size.unwrap_or(14.0) - size).abs() > f32::EPSILON {
            self.theme.font.mono_size = Some(size);
            crate::theme::apply_theme(ctx, &self.theme);
        }
    }
}
