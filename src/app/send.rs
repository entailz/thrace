/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Outgoing messages: the composer, slash commands, reactions, moderation and uploads.

use crate::app::decode::plain_media_source;
use crate::app::media::encode_clipboard_png;
use crate::app::rows::redact_in;
use crate::app::text::{
    format_ts, html_escape, mentioned_ids_in, now_millis, rich_bodies_in, snippet,
};
use crate::app::{
    ImageAttachment, Member, Moderation, PendingUpload, RoomAction, RoomMenuAction, SendResult,
    ThraceApp, TimelineRow, SLASH_COMMANDS,
};

impl ThraceApp {
    /// Load our most recent real message into the composer for editing.
    pub(in crate::app) fn start_edit_last(&mut self) {
        let own = self.own_user.clone();
        let Some(row) = self.rows.iter().rev().find(|r| {
            r.sender == own && r.id.starts_with('$') && r.image.is_none() && !r.is_sticker
        }) else {
            self.status = "nothing of yours to edit yet".into();
            return;
        };
        self.input = row.body.clone();
        self.editing = Some(row.id.clone());
        self.replying_to = None;
    }

    /// Send an `m.replace` for one of our messages.
    pub(in crate::app) fn send_edit(&mut self, target: String, text: String) {
        // Local echo; sync confirms.
        if let Some(row) = std::rc::Rc::make_mut(&mut self.rows)
            .iter_mut()
            .find(|r| r.id == target)
        {
            row.body = text.clone();
            row.formatted = None;
            row.edited = true;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(target) else {
                return SendResult::Failed("bad event id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            // `body` is the fallback; `m.new_content` is the replacement.
            let content = serde_json::json!({
                "msgtype": "m.text",
                "body": format!("* {text}"),
                "m.new_content": { "msgtype": "m.text", "body": text },
                "m.relates_to": { "rel_type": "m.replace", "event_id": eid },
            });
            match room.send_raw("m.room.message", content).await {
                Ok(_) => SendResult::Done("edited".into()),
                Err(e) => SendResult::Failed(format!("edit: {e}")),
            }
        });
    }

    pub(in crate::app) fn send_current_input(&mut self) {
        // In-progress edit consumes Enter; rewrites instead of sending.
        if let Some(target) = self.editing.take() {
            let text = self.input.trim().to_owned();
            self.input.clear();
            if !text.is_empty() {
                self.send_edit(target, text);
            }
            return;
        }
        // Uploads send first, then text; one Enter sends all.
        if !self.pending_uploads.is_empty() {
            let uploads = std::mem::take(&mut self.pending_uploads);
            // Sent: drop staging thumbnails.
            self.upload_thumbs.clear();
            for up in uploads {
                self.send_upload(up);
            }
        }
        let text = self.input.trim().to_owned();
        if text.is_empty() {
            self.input.clear();
            self.slash_selected = 0;
            return;
        }
        // Slash commands echo effects only, never raw text.
        if let Some(cmd) = text.strip_prefix('/') {
            let mut parts = cmd.splitn(2, ' ');
            let verb = parts.next().unwrap_or("").to_lowercase();
            let args = parts.next().unwrap_or("").to_owned();
            match verb.as_str() {
                "me" => self.send_emote(args),
                "react" => {
                    let key = if args.is_empty() { "+1".into() } else { args };
                    self.send_reaction_to_last(key);
                }
                "reply" => {
                    let target = self.rows.last().map(|r| r.id.clone());
                    if let Some(id) = target {
                        self.replying_to = Some(id);
                        if !args.is_empty() {
                            self.send_text(args);
                        }
                    }
                }
                "sticker" => self.send_sticker(args),
                "shrug" => self.send_text(format!("{} ¯\\_(ツ)_/¯", args)),
                "join" => self.room_action(args, RoomAction::Join),
                "invite" => self.room_action(args, RoomAction::Invite),
                "topic" => self.room_action(args, RoomAction::Topic),
                "nick" => self.set_nick(args),
                "leave" | "part" => self.room_action(String::new(), RoomAction::Leave),
                "roomname" => self.set_room_name(args),
                "plain" => self.send_plain(args),
                "spoiler" => self.send_spoiler(args),
                "kick" => self.moderate(args, Moderation::Kick),
                "ban" => self.moderate(args, Moderation::Ban),
                "unban" => self.moderate(args, Moderation::Unban),
                "op" => self.moderate(args, Moderation::Power),
                "deop" => self.moderate(format!("{args} 0"), Moderation::Power),
                "ignore" => self.moderate(args, Moderation::Ignore),
                "unignore" => self.moderate(args, Moderation::Unignore),
                "help" => {
                    let n = self.rows.len();
                    std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
                        id: format!("local-{n}"),
                        ts: format_ts(now_millis()),
                        origin_server_ts: 0,
                        sender: "system".into(),
                        display_name: "system".into(),
                        body: SLASH_COMMANDS
                            .iter()
                            .map(|(c, h)| format!("{c} — {h}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        formatted: None,
                        avatar_mxc: None,
                        reply_to: None,
                        reply_to_id: None,
                        thread_count: 0,
                        edited: false,
                        seen_by: Vec::new(),
                        image: None,
                        audio: None,
                        reactions: vec![],
                        is_sticker: false,
                        txn_id: None,
                    });
                }
                _ => {
                    let n = self.rows.len();
                    std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
                        id: format!("local-{n}"),
                        ts: format_ts(now_millis()),
                        origin_server_ts: 0,
                        sender: "system".into(),
                        display_name: "system".into(),
                        body: format!("Unknown command /{verb} — try /help"),
                        formatted: None,
                        avatar_mxc: None,
                        reply_to: None,
                        reply_to_id: None,
                        thread_count: 0,
                        edited: false,
                        seen_by: Vec::new(),
                        image: None,
                        audio: None,
                        reactions: vec![],
                        is_sticker: false,
                        txn_id: None,
                    });
                }
            }
        } else {
            self.send_text(text);
        }
        self.input.clear();
        self.slash_selected = 0;
    }

    /// Staged-upload thumbnail, decoded once and cached.
    pub(in crate::app) fn upload_thumb(
        &mut self,
        ctx: &egui::Context,
        up: &PendingUpload,
    ) -> Option<egui::TextureHandle> {
        if let Some(cached) = self.upload_thumbs.get(&up.name) {
            return cached.clone();
        }
        let decoded = up.is_image().then(|| {
            // Small budget: 28px chip from possibly huge source.
            crate::media_cache::decode_image(&up.bytes, 160_000).map(|img| {
                ctx.load_texture(
                    format!("upload:{}", up.name),
                    img,
                    egui::TextureOptions::LINEAR,
                )
            })
        });
        let handle = decoded.flatten();
        self.upload_thumbs.insert(up.name.clone(), handle.clone());
        handle
    }

    /// Stage clipboard image/files as uploads. Reads clipboard directly (egui paste is text-only).
    pub(in crate::app) fn paste_clipboard(&mut self, announce_empty: bool) {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                if announce_empty {
                    self.status = format!("clipboard unavailable: {e}");
                }
                return;
            }
        };
        // Raw bitmap first.
        if let Ok(img) = clipboard.get_image() {
            let (w, h) = (img.width, img.height);
            match encode_clipboard_png(&img) {
                Ok((name, bytes)) => match PendingUpload::from_bytes(name, bytes) {
                    Ok(up) => {
                        self.status = format!("pasted {} ({w}×{h})", up.name);
                        self.pending_uploads.push(up);
                    }
                    Err(e) => self.status = e.to_string(),
                },
                Err(e) => self.status = format!("paste: {e}"),
            }
            return;
        }
        // Else `file://` URIs / paths as text (file-manager copies).
        if let Ok(text) = clipboard.get_text() {
            if self.stage_clipboard_paths(&text) > 0 {
                return;
            }
        }
        if announce_empty {
            self.status = "nothing to paste — clipboard has no image or file".into();
        }
    }

    /// Stage `file://` URIs / absolute paths from clipboard text.
    pub(in crate::app) fn stage_clipboard_paths(&mut self, text: &str) -> usize {
        let mut staged = 0;
        for line in text.lines() {
            let line = line.trim();
            let path = if line.starts_with("file://") {
                match url::Url::parse(line)
                    .ok()
                    .and_then(|u| u.to_file_path().ok())
                {
                    Some(p) => p,
                    None => continue,
                }
            } else if line.starts_with('/') {
                std::path::PathBuf::from(line)
            } else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_owned();
            match std::fs::read(&path) {
                Ok(bytes) => match PendingUpload::from_bytes(name.clone(), bytes) {
                    Ok(up) => {
                        self.pending_uploads.push(up);
                        staged += 1;
                    }
                    Err(e) => self.status = e.to_string(),
                },
                Err(e) => self.status = format!("cannot read {name}: {e}"),
            }
        }
        if staged > 0 {
            self.status = format!("pasted {staged} file(s)");
        }
        staged
    }

    pub(in crate::app) fn pick_files(&mut self) {
        let paths = rfd::FileDialog::new().pick_files().unwrap_or_default();
        for path in paths {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_owned();
            match std::fs::read(&path) {
                Ok(bytes) => match PendingUpload::from_bytes(name.clone(), bytes) {
                    Ok(up) => self.pending_uploads.push(up),
                    Err(e) => self.status = e.to_string(),
                },
                Err(e) => self.status = format!("cannot read {name}: {e}"),
            }
        }
    }

    /// OS drag-and-drop files; same cap + staging as picker.
    pub(in crate::app) fn ingest_dropped(&mut self, files: Vec<egui::DroppedFile>) {
        for f in files {
            let name = f
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if f.name.is_empty() {
                        "dropped-file".into()
                    } else {
                        f.name.clone()
                    }
                });
            let bytes = if let Some(path) = &f.path {
                match std::fs::read(path) {
                    Ok(b) => b,
                    Err(e) => {
                        self.status = format!("cannot read {name}: {e}");
                        continue;
                    }
                }
            } else if let Some(bytes) = f.bytes.as_ref() {
                bytes.to_vec()
            } else {
                self.status = format!("cannot read {name}: no data");
                continue;
            };
            match PendingUpload::from_bytes(name.clone(), bytes) {
                Ok(up) => self.pending_uploads.push(up),
                Err(e) => self.status = e.to_string(),
            }
        }
    }

    /// Upload one staged file via `send_attachment`; images as `m.image`, rest as `m.file`.
    pub(in crate::app) fn send_upload(&mut self, up: PendingUpload) {
        // Txn-stamped echo; sync arrival replaces it.
        let txn = format!("rs{}", Self::uuid_txn());
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: txn.clone(),
            ts: format_ts(now_millis()),
            origin_server_ts: 0,
            sender: "@you:hs".into(),
            display_name: "you".into(),
            body: format!("▲ {} ({} KB) …", up.name, up.bytes.len() / 1024),
            formatted: None,
            avatar_mxc: None,
            reply_to: None,
            reply_to_id: None,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: None,
            audio: None,
            reactions: vec![],
            is_sticker: false,
            txn_id: Some(txn.clone()),
        });
        let Some(client) = self.client.clone() else {
            self.status = "log in to upload".into();
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            self.status = "log in to upload".into();
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedRoomId, UInt};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let mime: mime::Mime = up.mime.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM);
            let config = if up.is_image() {
                // Probe dims for `info`; failure still sends.
                match image::load_from_memory(&up.bytes) {
                    Ok(img) => {
                        let (w, h) = (img.width(), img.height());
                        matrix_sdk::attachment::AttachmentConfig::new().info(
                            matrix_sdk::attachment::AttachmentInfo::Image(
                                matrix_sdk::attachment::BaseImageInfo {
                                    width: Some(UInt::new(w as u64).unwrap_or(UInt::MAX)),
                                    height: Some(UInt::new(h as u64).unwrap_or(UInt::MAX)),
                                    size: Some(
                                        UInt::new(up.bytes.len() as u64).unwrap_or(UInt::MAX),
                                    ),
                                    ..Default::default()
                                },
                            ),
                        )
                    }
                    Err(_) => matrix_sdk::attachment::AttachmentConfig::new(),
                }
            } else {
                matrix_sdk::attachment::AttachmentConfig::new()
            };
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.clone().into();
            match room
                .send_attachment(&up.name, &mime, up.bytes, config.txn_id(txn_id))
                .await
            {
                Ok(_) => SendResult::Done(format!("sent {}", up.name)),
                Err(e) => SendResult::Failed(format!("upload {}: {e}", up.name)),
            }
        });
    }

    /// Plain message (or reply): local echo + `room.send` on a worker.
    pub(in crate::app) fn send_text(&mut self, text: String) {
        // The DM room doesn't exist server-side yet; queue nothing.
        if self
            .current_room_id()
            .as_deref()
            .is_some_and(|id| id.starts_with("dm:"))
        {
            self.status = "creating DM — send again in a moment".into();
            return;
        }
        let reply = self.replying_to.take().map(|id| ("you".into(), id));
        // Mentions + custom emoji in one pass; `m.mentions` is what notifies.
        let (plain, html) = self.rich_bodies(&text);
        let mentioned = self.mentioned_ids(&text);
        let formatted = html.clone();
        // Txn id on echo and send; sync echo replaces this row.
        let txn = format!("rs{}", Self::uuid_txn());
        self.push_local(
            "@you:hs",
            "you",
            text,
            reply.clone(),
            formatted.clone(),
            Some(txn.clone()),
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            self.status = "log in to send".into();
            return;
        };
        let reply_id = reply.map(|(_, id)| id);
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
            let mut content = match html {
                Some(h) => RoomMessageEventContent::text_html(plain, h),
                None => RoomMessageEventContent::text_plain(plain),
            };
            if let Some(target) = reply_id {
                if let (Ok(target_id), Some(own)) = (
                    OwnedEventId::try_from(target),
                    client.user_id().map(|u| u.to_owned()),
                ) {
                    content = content.make_reply_to(
                        matrix_sdk::ruma::events::room::message::ReplyMetadata::new(
                            &target_id, &own, None,
                        ),
                        matrix_sdk::ruma::events::room::message::ForwardThread::Yes,
                        matrix_sdk::ruma::events::room::message::AddMentions::No,
                    );
                }
            }
            // Always set: an empty `m.mentions` opts out of legacy push rules.
            content =
                content.add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(mentioned));
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.clone().into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    pub(in crate::app) fn send_emote(&mut self, text: String) {
        if self
            .current_room_id()
            .as_deref()
            .is_some_and(|id| id.starts_with("dm:"))
        {
            self.status = "creating DM — send again in a moment".into();
            return;
        }
        let (plain, html) = self.rich_bodies(&text);
        let mentioned = self.mentioned_ids(&text);
        let txn = format!("rs{}", Self::uuid_txn());
        self.push_local(
            "* you",
            "you",
            plain.clone(),
            None,
            html.clone(),
            Some(txn.clone()),
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            let Ok(rid) = matrix_sdk::ruma::OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
            let mut content = match html {
                Some(h) => RoomMessageEventContent::emote_html(plain, h),
                None => RoomMessageEventContent::emote_plain(plain),
            };
            content =
                content.add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(mentioned));
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.clone().into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    pub(in crate::app) fn send_reaction_to_last(&mut self, key: String) {
        let Some(target) = self.rows.last().map(|r| r.id.clone()) else {
            return;
        };
        let own = self
            .client
            .as_ref()
            .and_then(|c| c.user_id())
            .map(|u| u.to_string())
            .unwrap_or("you".into());
        if let Some(last) = std::rc::Rc::make_mut(&mut self.rows).last_mut() {
            Self::bump_reaction(last, key.clone(), &own);
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(target) else {
                return SendResult::Done("reaction sent".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::events::reaction::ReactionEventContent;
            use matrix_sdk::ruma::events::relation::Annotation;
            let content = ReactionEventContent::new(Annotation::new(eid, key));
            match room.send(content).await {
                Ok(_) => SendResult::Done("reaction sent".into()),
                Err(e) => SendResult::Failed(format!("reaction: {e}")),
            }
        });
    }

    /// Reaction toggle from the timeline.
    pub(in crate::app) fn send_reaction(&mut self, ri: usize, key: String) {
        let Some(target) = self.rows.get(ri).map(|r| r.id.clone()) else {
            return;
        };
        // MSC4027: custom-emoji reactions travel as mxc URI + shortcode label.
        let custom = self
            .packs
            .resolve(&key)
            .map(|i| (i.mxc_url.clone(), key.trim_matches(':').to_owned()));
        let (key, shortcode) = match custom {
            Some((mxc, sc)) => (mxc, Some(sc)),
            None => (key, None),
        };
        let own = self
            .client
            .as_ref()
            .and_then(|c| c.user_id())
            .map(|u| u.to_string())
            .unwrap_or("you".into());
        // Already reacted: this click takes it back (redact own event).
        let mine = self
            .rows
            .get(ri)
            .and_then(|r| r.reactions.iter().find(|g| g.key == key))
            .and_then(|g| g.event_of(&own).map(str::to_owned));
        let owns_pending = self
            .rows
            .get(ri)
            .and_then(|r| r.reactions.iter().find(|g| g.key == key))
            .is_some_and(|g| g.owns(&own));
        if owns_pending && mine.is_none() {
            self.status = "reaction still sending — try again in a moment".into();
            return;
        }
        if let Some(reaction_event) = mine {
            self.unreact(ri, &key, &own, reaction_event);
            return;
        }
        if let Some(row) = std::rc::Rc::make_mut(&mut self.rows).get_mut(ri) {
            Self::bump_reaction(row, key.clone(), &own);
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(target) else {
                return SendResult::Done("reaction sent".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            // Raw send: ruma has no room for the MSC4027 `shortcode` field.
            let mut content = serde_json::json!({
                "m.relates_to": {
                    "rel_type": "m.annotation",
                    "event_id": eid,
                    "key": key,
                }
            });
            if let Some(sc) = shortcode {
                content["shortcode"] = serde_json::Value::String(sc);
            }
            match room.send_raw("m.reaction", content).await {
                Ok(_) => SendResult::Done("reaction sent".into()),
                Err(e) => SendResult::Failed(format!("reaction: {e}")),
            }
        });
    }

    pub(in crate::app) fn send_sticker(&mut self, shortcode: String) {
        let sc = if shortcode.is_empty() {
            "party".into()
        } else {
            shortcode
        };
        let mxc = self
            .packs
            .resolve(&format!(":{sc}:"))
            .map(|i| i.mxc_url.clone());
        // Keep target id only; renderer resolves sender + snippet from live row.
        let reply_to_id = self.replying_to.take();
        let n = self.rows.len();
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: format!("local-{n}"),
            ts: format_ts(now_millis()),
            origin_server_ts: 0,
            sender: "@you:hs".into(),
            display_name: "you".into(),
            body: format!("Sticker :{sc}:"),
            formatted: None,
            avatar_mxc: None,
            reply_to: None,
            reply_to_id,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: mxc.clone().map(|m| ImageAttachment {
                source: plain_media_source(&m),
                thumbnail_source: None,
                is_video: false,
                duration_ms: None,
                mxc: m,
                name: sc.clone(),
                w: 0,
                h: 0,
            }),
            audio: None,
            reactions: vec![],
            is_sticker: true,
            txn_id: None,
        });
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        let Some(mxc) = mxc else {
            self.status = format!("no sticker :{sc}: in any pack");
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedMxcUri, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let url: OwnedMxcUri = mxc.clone().into();
            use matrix_sdk::ruma::events::sticker::StickerEventContent;
            let content = StickerEventContent::new(sc.clone(), Default::default(), url);
            match room.send(content).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(format!("sticker: {e}")),
            }
        });
    }

    pub(in crate::app) fn room_action(&mut self, args: String, action: RoomAction) {
        let Some(client) = self.client.clone() else {
            self.status = "log in first".into();
            return;
        };
        let cur = self.current_room_id();
        self.status = "working …".into();
        self.spawn_send(async move {
            let res: Result<String, String> = async {
                match action {
                    RoomAction::Join => {
                        use matrix_sdk::ruma::OwnedRoomOrAliasId;
                        let Ok(id) = OwnedRoomOrAliasId::try_from(args.clone()) else {
                            return Err("usage: /join #alias:hs or !room:hs".into());
                        };
                        client
                            .join_room_by_id_or_alias(&id, &[])
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok("joined".into())
                    }
                    RoomAction::Invite => {
                        let Some(rid_s) = cur else {
                            return Err("no room".into());
                        };
                        use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
                        let rid = OwnedRoomId::try_from(rid_s).map_err(|e| e.to_string())?;
                        let uid = OwnedUserId::try_from(args.clone())
                            .map_err(|_| "usage: /invite @u:hs".to_string())?;
                        let Some(room) = client.get_room(&rid) else {
                            return Err("room not found".into());
                        };
                        room.invite_user_by_id(&uid)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok("invited".into())
                    }
                    RoomAction::Topic => {
                        let Some(rid_s) = cur else {
                            return Err("no room".into());
                        };
                        use matrix_sdk::ruma::OwnedRoomId;
                        let rid = OwnedRoomId::try_from(rid_s).map_err(|e| e.to_string())?;
                        let Some(room) = client.get_room(&rid) else {
                            return Err("room not found".into());
                        };
                        room.set_room_topic(&args)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok("topic set".into())
                    }
                    RoomAction::Leave => {
                        let Some(rid_s) = cur else {
                            return Err("no room".into());
                        };
                        use matrix_sdk::ruma::OwnedRoomId;
                        let rid = OwnedRoomId::try_from(rid_s).map_err(|e| e.to_string())?;
                        let Some(room) = client.get_room(&rid) else {
                            return Err("room not found".into());
                        };
                        room.leave().await.map_err(|e| e.to_string())?;
                        Ok("left".into())
                    }
                }
            }
            .await;
            match res {
                Ok(t) => SendResult::Done(t),
                Err(e) => SendResult::Failed(e),
            }
        });
    }

    /// Run a moderation command (real server call).
    pub(in crate::app) fn moderate(&mut self, args: String, what: Moderation) {
        let mut parts = args.trim().splitn(2, ' ');
        let target = parts.next().unwrap_or("").to_owned();
        let extra = parts.next().unwrap_or("").trim().to_owned();
        if target.is_empty() {
            self.status = "usage: /<command> @user:server [reason or level]".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.status = format!("working on {target} …");
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
            let Ok(user) = OwnedUserId::try_from(target.clone()) else {
                return SendResult::Failed(format!("{target} is not a user id"));
            };
            let reason = (!extra.is_empty()).then_some(extra.as_str());
            if let Moderation::Ignore | Moderation::Unignore = what {
                let res = match what {
                    Moderation::Ignore => client.account().ignore_user(&user).await,
                    _ => client.account().unignore_user(&user).await,
                };
                return match res {
                    Ok(()) => SendResult::Done(format!("done — {target}")),
                    Err(e) => SendResult::Failed(format!("{e}")),
                };
            }
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let res = match what {
                Moderation::Kick => room.kick_user(&user, reason).await,
                Moderation::Ban => room.ban_user(&user, reason).await,
                Moderation::Unban => room.unban_user(&user, reason).await,
                Moderation::Power => {
                    // `/op` with no level means moderator.
                    let level: i64 = extra.parse().unwrap_or(50);
                    // `Int` converts fallibly, not via `From`.
                    let Ok(level) = matrix_sdk::ruma::Int::try_from(level) else {
                        return SendResult::Failed("power level out of range".into());
                    };
                    room.update_power_levels(vec![(&user, level)])
                        .await
                        .map(|_| ())
                }
                _ => unreachable!("ignore handled above"),
            };
            match res {
                Ok(()) => SendResult::Done(format!("done — {target}")),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// `/roomname` — rename the current room.
    pub(in crate::app) fn set_room_name(&mut self, name: String) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.status = "usage: /roomname New name".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            match room.set_name(name).await {
                Ok(_) => SendResult::Done("room renamed".into()),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// `/plain` — send text with markdown left literal.
    pub(in crate::app) fn send_plain(&mut self, text: String) {
        if text.trim().is_empty() {
            self.status = "usage: /plain some *literal* text".into();
            return;
        }
        self.send_text(text);
    }

    /// `/spoiler` — send hidden behind a spoiler.
    pub(in crate::app) fn send_spoiler(&mut self, text: String) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            self.status = "usage: /spoiler the secret".into();
            return;
        }
        let html = format!("<span data-mx-spoiler>{}</span>", html_escape(&text));
        let txn = format!("rs{}", Self::uuid_txn());
        self.push_local(
            "@you:hs",
            "you",
            text.clone(),
            None,
            Some(html.clone()),
            Some(txn.clone()),
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
            use matrix_sdk::ruma::OwnedRoomId;
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let content = RoomMessageEventContent::text_html(text, html);
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    pub(in crate::app) fn set_nick(&mut self, nick: String) {
        if nick.trim().is_empty() {
            self.status = "usage: /nick NewName".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.status = "setting display name …".into();
        self.spawn_send(async move {
            match client.account().set_display_name(Some(&nick)).await {
                Ok(_) => SendResult::Done("display name set".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    pub(in crate::app) fn push_local(
        &mut self,
        sender: &str,
        display: &str,
        body: String,
        reply: Option<(String, String)>,
        formatted: Option<String>,
        txn_id: Option<String>,
    ) {
        // Resolve reply id to snippet.
        let reply_to_id = reply.as_ref().map(|(_, id)| id.clone());
        let reply_to = reply
            .as_ref()
            .and_then(|(_, id)| {
                self.rows
                    .iter()
                    .find(|r| r.id == *id)
                    .map(|r| (r.display_name.clone(), snippet(&r.body, 60)))
            })
            .or(reply);
        let n = self.rows.len();
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: txn_id.clone().unwrap_or_else(|| format!("local-{n}")),
            ts: format_ts(now_millis()),
            origin_server_ts: 0,
            sender: sender.into(),
            display_name: display.into(),
            body,
            formatted,
            avatar_mxc: None,
            reply_to,
            reply_to_id,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: None,
            audio: None,
            reactions: vec![],
            is_sticker: false,
            txn_id,
        });
    }

    /// Take back our reaction by redacting its event; echoes locally.
    pub(in crate::app) fn unreact(
        &mut self,
        ri: usize,
        key: &str,
        own: &str,
        reaction_event: String,
    ) {
        if let Some(row) = std::rc::Rc::make_mut(&mut self.rows).get_mut(ri) {
            if let Some(group) = row.reactions.iter_mut().find(|g| g.key == key) {
                group.senders.retain(|s| s.user != own);
            }
            row.reactions.retain(|g| !g.senders.is_empty());
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(reaction_event) else {
                return SendResult::Failed("bad reaction id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            match room.redact(&eid, None, None).await {
                Ok(_) => SendResult::Done("reaction removed".into()),
                Err(e) => SendResult::Failed(format!("un-react: {e}")),
            }
        });
    }

    /// Slash commands matching the typed verb. `None` when the palette stays hidden.
    pub(in crate::app) fn slash_matches(
        &self,
    ) -> Option<(String, Vec<(&'static str, &'static str)>)> {
        let rest = self.input.strip_prefix('/')?;
        let verb = rest.split_whitespace().next().unwrap_or("").to_lowercase();
        let matches: Vec<(&str, &str)> = SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd[1..].starts_with(&verb))
            .take(8)
            .map(|(a, b)| (*a, *b))
            .collect();
        (!matches.is_empty()).then_some((verb, matches))
    }

    /// Replace the typed verb with `cmd`, keeping any arguments.
    ///
    /// Enter must honor the highlight, not send the raw text.
    pub(in crate::app) fn complete_slash(&mut self, cmd: &str) {
        let args = self
            .input
            .split_once(' ')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default();
        self.input = if args.is_empty() {
            format!("{cmd} ")
        } else {
            format!("{cmd} {args}")
        };
        self.slash_selected = 0;
    }

    /// Stable id for the composer field, so accepting a completion can hand
    /// focus straight back to it from anywhere in the frame.
    pub(in crate::app) fn composer_id() -> egui::Id {
        egui::Id::new("composer")
    }

    pub(in crate::app) fn focus_composer(ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(Self::composer_id()));
    }

    /// Members matching an @-mention prefix, for the composer dropdown.
    pub(in crate::app) fn mention_hits(&self, prefix: &str) -> Vec<Member> {
        let q = prefix.trim_start_matches('@').to_lowercase();
        self.members
            .iter()
            .filter(|m| {
                q.is_empty()
                    || m.display.to_lowercase().contains(&q)
                    || m.mxid.to_lowercase().contains(&q)
            })
            .take(8)
            .cloned()
            .collect()
    }

    /// Accept a highlighted @-mention: swap the partial @word for the full
    /// mxid and leave the composer open for the rest of the message.
    /// The mxid (not the display name) is what notifies and pill-renders.
    pub(in crate::app) fn accept_mention(&mut self, m: &Member, prefix: &str) {
        let cut = self.input.len().saturating_sub(prefix.len());
        self.input.truncate(cut);
        self.input.push_str(&m.mxid);
        self.input.push(' ');
        self.mention_selected = 0;
    }

    /// Members named by `text`: full mxids, plus `@display` spellings for
    /// names typed by hand. Matched against the current room's members.
    pub(in crate::app) fn mentioned_ids(&self, text: &str) -> Vec<matrix_sdk::ruma::OwnedUserId> {
        mentioned_ids_in(&self.members, text)
    }

    /// Build `(plain, html)` for the composer text, resolving custom-emoji
    /// `:shortcode:` and @-mention pills in one pass. One pass matters:
    /// mxids contain colons, so an emoji-only scan would split `@user:hs`
    /// apart and swallow a following `:shortcode:`.
    pub(in crate::app) fn rich_bodies(&self, raw: &str) -> (String, Option<String>) {
        rich_bodies_in(&self.members, &self.packs, raw)
    }

    /// Apply a sidebar room action.
    pub(in crate::app) fn room_menu_action(&mut self, room_id: String, action: RoomMenuAction) {
        // Local echo so the sidebar reacts instantly.
        match action {
            RoomMenuAction::MarkRead => {
                if let Some(r) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
                    r.unread = 0;
                    r.mentioned = false;
                }
            }
            RoomMenuAction::MarkUnread => {
                if let Some(r) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
                    r.unread = r.unread.max(1);
                }
            }
            RoomMenuAction::CopyLink => {
                // matrix.to is the portable form every client understands.
                let link = format!("https://matrix.to/#/{room_id}");
                self.status =
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(link.clone())) {
                        Ok(()) => format!("copied {link}"),
                        Err(e) => format!("copy failed: {e}"),
                    };
                return;
            }
            RoomMenuAction::Invite => {
                // Needs a user id; route through the composer instead of guessing.
                self.input = "/invite ".into();
                self.status = "type the user id to invite, then Enter".into();
                return;
            }
            RoomMenuAction::Leave => {
                self.rooms.retain(|r| r.room_id != room_id);
                self.timelines.remove(&room_id);
                self.members_by_room.remove(&room_id);
                if self.current >= self.rooms.len() {
                    self.current = self.rooms.len().saturating_sub(1);
                }
                std::rc::Rc::make_mut(&mut self.rows).clear();
                self.members.clear();
            }
            RoomMenuAction::Notify(mode) => {
                if let Some(r) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
                    r.notify = Some(mode);
                }
            }
            _ => {}
        }

        let Some(client) = self.client.clone() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let result = match action {
                RoomMenuAction::MarkRead => room.set_unread_flag(false).await,
                RoomMenuAction::MarkUnread => room.set_unread_flag(true).await,
                RoomMenuAction::Favourite(on) => room.set_is_favourite(on, None).await,
                RoomMenuAction::Leave => room.leave().await,
                RoomMenuAction::Notify(mode) => {
                    return match client
                        .notification_settings()
                        .await
                        .set_room_notification_mode(&rid, mode.sdk())
                        .await
                    {
                        Ok(()) => SendResult::Done(format!("notifications: {}", mode.label())),
                        Err(e) => SendResult::Failed(format!("{e}")),
                    };
                }
                // Handled locally above.
                RoomMenuAction::Invite | RoomMenuAction::CopyLink => {
                    return SendResult::Done(String::new())
                }
            };
            match result {
                Ok(()) => SendResult::Done(String::new()),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// Delete a message by redacting it; a refused redact surfaces instead of failing silently.
    pub(in crate::app) fn delete_message(&mut self, event_id: String) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        // Local echo; the sync redaction confirms it.
        redact_in(
            std::rc::Rc::make_mut(&mut self.rows).as_mut_slice(),
            &event_id,
        );
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(event_id) else {
                return SendResult::Failed("bad event id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            match room.redact(&eid, None, None).await {
                Ok(_) => SendResult::Done("message deleted".into()),
                Err(e) => SendResult::Failed(format!("delete: {e}")),
            }
        });
    }
}
