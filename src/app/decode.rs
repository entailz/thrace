/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Wire format to `TimelineRow`: history pages, sync batches and attachments.

use crate::app::rows::{
    bundled_annotations, edit_in, edit_replacement, merge_reactions, seen_in, split_reply_fallback,
};
use crate::app::text::{format_ts, localpart, now_millis};
use crate::app::{AudioAttachment, ImageAttachment, Member, SyncBatch, SyncReaction, TimelineRow};

/// Load all image packs: per-room `im.ponies.room_emotes` state + MSC2545 user
/// packs from account data, raw-JSON decoded (no `unstable-msc2545` needed).
pub(in crate::app) async fn load_all_packs(
    client: &matrix_sdk::Client,
) -> crate::matrix::PackStore {
    use matrix_sdk::ruma::events::StateEventType;
    let mut store = crate::matrix::PackStore::new();
    for room in client.joined_rooms() {
        let room_id = room.room_id().to_string();
        let ev_type = StateEventType::from("im.ponies.room_emotes");
        let Ok(events) = room.get_state_events(ev_type).await else {
            continue;
        };
        for raw in events {
            let Ok(json) = serde_json::to_value(&raw) else {
                continue;
            };
            let state_key = json
                .get("state_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let content = json.get("content").cloned().unwrap_or_default();
            if content.get("images").is_none() {
                continue;
            }
            store.upsert_pack(crate::matrix::PackStore::decode_state_content(
                &room_id, &state_key, &content,
            ));
        }
    }
    // MSC2545 user packs (`im.ponies.user_emotes` account data, custom type).
    {
        use matrix_sdk::ruma::events::GlobalAccountDataEventType;
        let ev_type = GlobalAccountDataEventType::from("im.ponies.user_emotes");
        if let Ok(Some(raw)) = client.account().account_data_raw(ev_type).await {
            if let Ok(json) = serde_json::to_value(&raw) {
                let content = json.get("content").cloned().unwrap_or_default();
                if content.get("images").is_some() {
                    store.upsert_pack(crate::matrix::PackStore::decode_state_content(
                        "account",
                        "user_emotes",
                        &content,
                    ));
                }
            }
        }
    }
    store
}

/// Load members from synced state and fetch history without delaying login.
pub(in crate::app) async fn load_initial_history(
    client: &matrix_sdk::Client,
    room_id: &str,
) -> Result<(Vec<TimelineRow>, Option<String>, Vec<Member>), String> {
    let id = matrix_sdk::ruma::OwnedRoomId::try_from(room_id).map_err(|e| e.to_string())?;
    let room = client.get_room(&id).ok_or("room not found")?;
    let room_members: Vec<_> = room
        .members_no_sync(matrix_sdk::RoomMemberships::ACTIVE)
        .await
        .map_err(|e| format!("members: {e}"))?
        .into_iter()
        .take(50)
        .collect();
    let ids: Vec<_> = room_members
        .iter()
        .map(|m| m.user_id().to_owned())
        .collect();
    let mut presence: std::collections::HashMap<String, String> = client
        .state_store()
        .get_presence_events(&ids)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|raw| presence_update(&serde_json::to_value(raw).ok()?))
        .collect();
    let members: Vec<Member> = room_members
        .into_iter()
        .map(|m| Member {
            presence: presence.remove(m.user_id().as_str()),
            display: m.display_name().unwrap_or_else(|| m.name()).to_owned(),
            mxid: m.user_id().to_string(),
            avatar_mxc: m.avatar_url().map(|u| u.to_string()),
            power: match m.power_level() {
                matrix_sdk::ruma::events::room::power_levels::UserPowerLevel::Int(level) => {
                    level.into()
                }
                _ => i64::MAX,
            },
        })
        .collect();
    let avatars = members
        .iter()
        .map(|m| (m.mxid.clone(), m.avatar_mxc.clone()))
        .collect();
    let names = members
        .iter()
        .map(|m| (m.mxid.clone(), m.display.clone()))
        .collect();
    let (mut rows, token) =
        load_room_history(&room, &avatars, &names, None, INITIAL_HISTORY).await?;
    place_stored_receipts(&room, &members, &mut rows).await;
    Ok((rows, token, members))
}

/// Put each member's stored read receipt on the rows, so seen-by avatars show right after a
/// room loads instead of only once someone reads something new.
async fn place_stored_receipts(
    room: &matrix_sdk::Room,
    members: &[Member],
    rows: &mut [TimelineRow],
) {
    use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
    for member in members {
        let Ok(user_id) = matrix_sdk::ruma::OwnedUserId::try_from(member.mxid.as_str()) else {
            continue;
        };
        // Clients send either unthreaded or main-thread receipts; the newer one counts.
        let mut newest: Option<(String, u64)> = None;
        for thread in [ReceiptThread::Unthreaded, ReceiptThread::Main] {
            if let Ok(Some((event_id, receipt))) = room
                .load_user_receipt(ReceiptType::Read, thread, &user_id)
                .await
            {
                let ts = receipt.ts.map(|ts| u64::from(ts.get())).unwrap_or_default();
                if newest.as_ref().is_none_or(|(_, seen)| ts >= *seen) {
                    newest = Some((event_id.to_string(), ts));
                }
            }
        }
        if let Some((event_id, ts)) = newest {
            seen_in(rows, &event_id, &member.mxid, ts);
        }
    }
}

/// (user, state) from a raw `m.presence` event; `currently_active` counts as online.
pub(in crate::app) fn presence_update(json: &serde_json::Value) -> Option<(String, String)> {
    let sender = json.get("sender")?.as_str()?.to_owned();
    let content = json.get("content")?;
    let active = content.get("currently_active").and_then(|v| v.as_bool()) == Some(true);
    let state = content.get("presence")?.as_str()?;
    let state = if active { "online" } else { state };
    Some((sender, state.to_owned()))
}

/// One screenful per history request. Older messages load on scroll.
pub(in crate::app) const INITIAL_HISTORY: u32 = 50;

/// Relation lookups per batch. One HTTP round trip each, so keep the burst small
/// and let the next frame pick up what the cap left behind — a page of history
/// drains over a few batches without stalling the UI.
pub(in crate::app) const RELATION_BURST: usize = 12;

/// Reactions fetched per message. Well past what any message carries in practice;
/// without it the server picks, and a busy message can come back short.
pub(in crate::app) const RELATION_PAGE: u32 = 100;

/// Guard rail: each page is its own round trip.
const _: () = assert!(INITIAL_HISTORY <= 100);

/// Fetch one page of history, newest last. Returns rows plus the token for
/// the page before them (`None` = start of room).
pub(in crate::app) async fn load_room_history(
    room: &matrix_sdk::Room,
    avatars: &std::collections::HashMap<String, Option<String>>,
    names: &std::collections::HashMap<String, String>,
    from: Option<String>,
    limit: u32,
) -> Result<(Vec<TimelineRow>, Option<String>), String> {
    use matrix_sdk::deserialized_responses::TimelineEvent;
    use matrix_sdk::ruma::UInt;
    let mut rows = Vec::new();
    let next_token;
    {
        let mut opts = matrix_sdk::room::MessagesOptions::backward().from(from.as_deref());
        opts.limit = UInt::new(limit.into()).unwrap_or(matrix_sdk::ruma::uint!(10));
        let msgs = room
            .messages(opts)
            .await
            .map_err(|e| format!("history: {e}"))?;
        // Newest-first on the wire; chronological on screen.
        let mut events: Vec<&TimelineEvent> = msgs.chunk.iter().collect();
        events.reverse();
        // `rows` holds older pages; the current page is newer.
        let mut page = Vec::new();
        for ev in events {
            let Ok(json) = serde_json::to_value(ev.raw()) else {
                continue;
            };
            // Only m.room.message with a body.
            let Some(t) = json.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            if t != "m.room.message" && t != "m.sticker" {
                continue;
            }
            let sender = ev.sender().map(|s| s.to_string()).unwrap_or("?".into());
            let display = names
                .get(&sender)
                .cloned()
                .unwrap_or_else(|| localpart(&sender));
            let content = json.get("content").cloned().unwrap_or_default();
            if let Some((target, new_body, new_fmt)) = edit_replacement(&content) {
                edit_in(&mut page, &target, new_body, new_fmt);
                continue;
            }
            let body = content
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if body.is_empty() && content.get("url").is_none() {
                continue;
            }
            let event_id = json
                .get("event_id")
                .and_then(|v| v.as_str())
                .unwrap_or("e")
                .to_owned();
            let ts = json.get("origin_server_ts").and_then(|v| v.as_u64());
            // Avatar from the fetched member list, not a store hit per event.
            let avatar = avatars.get(&sender).cloned().flatten();
            let mut row = decode_timeline_json(
                room, &sender, &display, event_id, t, &content, &body, ts, avatar,
            )
            .await;
            merge_reactions(&mut row.reactions, &bundled_annotations(&json));
            row.txn_id = json
                .get("unsigned")
                .and_then(|value| value.get("transaction_id"))
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            page.push(row);
        }
        rows.splice(..0, page);
        next_token = msgs.end.clone();
    }
    // Only the first page gets a placeholder; later empties mean start of room.
    if rows.is_empty() && from.is_none() {
        rows.push(empty_row("No messages yet — say hi"));
    }
    Ok((rows, next_token))
}

pub(in crate::app) fn decode_audio(
    content: &serde_json::Value,
    body: &str,
) -> Option<AudioAttachment> {
    let source: matrix_sdk::ruma::events::room::MediaSource =
        serde_json::from_value(content.clone()).ok()?;
    let mxc = media_source_mxc(&source);
    if mxc.is_empty() || mxc == "mxc://?" {
        return None;
    }
    let info = content.get("info").cloned().unwrap_or_default();
    let msc = content
        .get("org.matrix.msc1767.audio")
        .cloned()
        .unwrap_or_default();
    let waveform = msc
        .get("waveform")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v.min(1024) as f32 / 1024.0)
                .collect()
        })
        .unwrap_or_default();
    Some(AudioAttachment {
        mxc,
        source,
        name: body.to_owned(),
        mime: info
            .get("mimetype")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        duration_ms: info
            .get("duration")
            .and_then(|v| v.as_u64())
            .or_else(|| msc.get("duration").and_then(|v| v.as_u64())),
        waveform,
        is_voice: content.get("org.matrix.msc3245.voice").is_some(),
    })
}

/// Shared decoder for history + live sync, so arrivals render like backfill.
#[allow(clippy::too_many_arguments)] // flat decode signature, not a struct
pub(in crate::app) async fn decode_timeline_json(
    room: &matrix_sdk::Room,
    sender: &str,
    display: &str,
    event_id: String,
    event_type: &str,
    content: &serde_json::Value,
    body: &str,
    origin_server_ts: Option<u64>,
    // Sender avatar from the caller's member list; `None` falls back to store.
    avatar_hint: Option<String>,
) -> TimelineRow {
    let formatted = content
        .get("formatted_body")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    // `m.relates_to → m.in_reply_to → event_id`; strip the plaintext fallback
    // since we draw our own reply line.
    let reply_to_id = content
        .get("m.relates_to")
        .and_then(|r| r.get("m.in_reply_to"))
        .and_then(|r| r.get("event_id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    // Strip the spec's plain-text fallback off the body: we draw our own
    // reply line, so leaving it in printed the quoted original twice.
    let (reply_to, body) = if reply_to_id.is_some() {
        let (preview, stripped) = split_reply_fallback(body);
        (preview, stripped)
    } else {
        (None, body.to_owned())
    };
    let body = body.as_str();
    // Prefer the member list; store lookup only as fallback.
    let avatar_mxc = match avatar_hint {
        Some(mxc) => Some(mxc),
        None => avatar_for_sender(room, sender).await,
    };
    let info = content.get("info").cloned().unwrap_or_default();
    let (w, h) = (
        info.get("w").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        info.get("h").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
    );
    let msgtype = content
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    // `m.video` needs the player path, not a bare filename.
    let is_video = msgtype == "m.video";
    let audio = (msgtype == "m.audio")
        .then(|| decode_audio(content, body))
        .flatten();
    let image = (msgtype == "m.image" || is_video)
        .then_some(())
        .and_then(|_| {
            // Plain rooms carry `url`; encrypted rooms carry `file` (SDK decrypts).
            // Missing/placeholder mxc means no attachment.
            let source: matrix_sdk::ruma::events::room::MediaSource =
                serde_json::from_value(content.clone()).ok()?;
            let mxc = media_source_mxc(&source);
            if mxc.is_empty() || mxc == "mxc://?" {
                return None;
            }
            Some(ImageAttachment {
                mxc,
                source,
                thumbnail_source: thumbnail_media_source(&info),
                name: body.to_owned(),
                w,
                h,
                is_video,
                duration_ms: info.get("duration").and_then(|v| v.as_u64()),
            })
        });
    let sticker_image = if event_type == "m.sticker" && image.is_none() {
        serde_json::from_value::<matrix_sdk::ruma::events::room::MediaSource>(content.clone())
            .ok()
            .map(|source| ImageAttachment {
                mxc: media_source_mxc(&source),
                source,
                thumbnail_source: thumbnail_media_source(&info),
                name: body.to_owned(),
                w,
                h,
                is_video: false,
                duration_ms: None,
            })
    } else {
        None
    };
    TimelineRow {
        id: event_id,
        ts: format_ts(origin_server_ts),
        origin_server_ts: origin_server_ts.unwrap_or_default(),
        sender: sender.to_owned(),
        display_name: display.to_owned(),
        body: body.to_owned(),
        formatted,
        avatar_mxc,
        reply_to,
        reply_to_id,
        thread_count: 0,
        image: image.or(sticker_image),
        audio,
        reactions: vec![],
        edited: false,
        seen_by: Vec::new(),
        is_sticker: event_type == "m.sticker",
        txn_id: None,
    }
}

pub(in crate::app) fn plain_media_source(mxc: &str) -> matrix_sdk::ruma::events::room::MediaSource {
    let uri: &matrix_sdk::ruma::MxcUri = mxc.into();
    matrix_sdk::ruma::events::room::MediaSource::Plain(uri.to_owned())
}

pub(in crate::app) fn media_source_mxc(
    source: &matrix_sdk::ruma::events::room::MediaSource,
) -> String {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => uri.to_string(),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(file) => file.url.to_string(),
    }
}

/// Which media to decode for a timeline still: the event's own thumbnail when it has
/// one, else the file itself — except for video, where the file is a clip no image
/// decoder can read, so there is nothing worth asking the server for.
pub(in crate::app) fn timeline_still_source(
    img: &ImageAttachment,
) -> Option<matrix_sdk::ruma::events::room::MediaSource> {
    match (&img.thumbnail_source, img.is_video) {
        (Some(thumb), _) => Some(thumb.clone()),
        (None, false) => Some(img.source.clone()),
        (None, true) => None,
    }
}

pub(in crate::app) fn thumbnail_media_source(
    info: &serde_json::Value,
) -> Option<matrix_sdk::ruma::events::room::MediaSource> {
    let source = if let Some(file) = info.get("thumbnail_file") {
        serde_json::json!({ "file": file })
    } else if let Some(url) = info.get("thumbnail_url") {
        serde_json::json!({ "url": url })
    } else {
        return None;
    };
    serde_json::from_value(source).ok()
}

/// Decode one `/sync` into rows + reactions + verify notices, matching
/// `load_room_history` shapes via `decode_timeline_json`.
pub(in crate::app) async fn decode_sync_batch(
    client: &matrix_sdk::Client,
    resp: &matrix_sdk::sync::SyncResponse,
) -> SyncBatch {
    let mut batch = SyncBatch::default();
    for event in &resp.account_data {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(event.json().get()) {
            if value.get("type").and_then(|v| v.as_str()) == Some("m.recent_emoji") {
                batch.emoji_usage = value
                    .get("content")
                    .cloned()
                    .and_then(|content| serde_json::from_value(content).ok());
            }
        }
    }
    let mut avatar_memo: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut display_memo: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (room_id, update) in &resp.rooms.joined {
        let room_id_s = room_id.to_string();
        let Some(room) = client.get_room(room_id) else {
            continue;
        };
        // The SDK has already applied this update's state and tags.
        batch
            .room_meta
            .push((room_id_s.clone(), crate::app::session::room_meta(&room)));
        for raw in &update.account_data {
            let Ok(json) = serde_json::to_value(raw) else {
                continue;
            };
            if json.get("type").and_then(|v| v.as_str()) != Some("m.fully_read") {
                continue;
            }
            if let Some(event_id) = json.pointer("/content/event_id").and_then(|v| v.as_str()) {
                batch
                    .fully_read
                    .push((room_id_s.clone(), event_id.to_owned()));
            }
        }
        for ev in &update.timeline.events {
            let raw = ev.kind.raw();
            let Ok(json) = serde_json::to_value(raw) else {
                continue;
            };
            let Some(t) = json.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            let sender = json
                .get("sender")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_owned();
            let display = match display_memo.get(&sender) {
                Some(cached) => cached.clone(),
                None => {
                    let looked_up = display_name_for_sender(&room, &sender)
                        .await
                        .unwrap_or_else(|| localpart(&sender));
                    display_memo.insert(sender.clone(), looked_up.clone());
                    looked_up
                }
            };
            let event_id = json
                .get("event_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if event_id.is_empty() {
                continue;
            }
            if t == "m.reaction" {
                let content = json.get("content").cloned().unwrap_or_default();
                let relates = content.get("m.relates_to").cloned().unwrap_or_default();
                let target = relates
                    .get("event_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let key = relates
                    .get("key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                if !target.is_empty() && !key.is_empty() {
                    batch.reactions.push((
                        room_id_s.clone(),
                        SyncReaction {
                            target,
                            key,
                            sender,
                            event_id: event_id.clone(),
                        },
                    ));
                }
                continue;
            }
            if t == "m.room.redaction" {
                // Room v11 moved `redacts` into content; accept both locations.
                let redacts = json
                    .get("content")
                    .and_then(|c| c.get("redacts"))
                    .or_else(|| json.get("redacts"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !redacts.is_empty() {
                    batch
                        .redactions
                        .push((room_id_s.clone(), redacts.to_owned()));
                }
                continue;
            }
            if t != "m.room.message" && t != "m.sticker" {
                continue;
            }
            let content = json.get("content").cloned().unwrap_or_default();
            if let Some((target, new_body, new_fmt)) = edit_replacement(&content) {
                if !target.is_empty() {
                    batch
                        .edits
                        .push((room_id_s.clone(), target, new_body, new_fmt));
                }
                continue;
            }
            let body = content
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if body.is_empty() && content.get("url").is_none() && content.get("file").is_none() {
                continue;
            }
            let ts = json.get("origin_server_ts").and_then(|v| v.as_u64());
            // Memoize per sender; bursts otherwise hit the store per message.
            let avatar = match avatar_memo.get(&sender) {
                Some(cached) => cached.clone(),
                None => {
                    let looked_up = avatar_for_sender(&room, &sender).await;
                    avatar_memo.insert(sender.clone(), looked_up.clone());
                    looked_up
                }
            };
            let mut row = decode_timeline_json(
                &room, &sender, &display, event_id, t, &content, &body, ts, avatar,
            )
            .await;
            merge_reactions(&mut row.reactions, &bundled_annotations(&json));
            // Stamp the server-echoed txn so the local echo is replaced.
            row.txn_id = json
                .get("unsigned")
                .and_then(|u| u.get("transaction_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            batch.rows.push((room_id_s.clone(), row));
        }
    }
    // Ephemeral `m.receipt`s (`m.read` + threaded both count as seen).
    for (room_id, update) in &resp.rooms.joined {
        let room_id_s = room_id.to_string();
        for raw in &update.ephemeral {
            let Ok(json) = serde_json::to_value(raw) else {
                continue;
            };
            if json.get("type").and_then(|v| v.as_str()) == Some("m.typing") {
                let users = json
                    .pointer("/content/user_ids")
                    .and_then(|v| v.as_array())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                batch.typing.push((room_id_s.clone(), users));
                continue;
            }
            if json.get("type").and_then(|v| v.as_str()) != Some("m.receipt") {
                continue;
            }
            let content = json.get("content").cloned().unwrap_or_default();
            let Some(map) = content.as_object() else {
                continue;
            };
            for (event_id, receipts) in map {
                let Some(obj) = receipts.as_object() else {
                    continue;
                };
                for (_rtype, users) in obj {
                    let Some(users) = users.as_object() else {
                        continue;
                    };
                    for (user_id, receipt) in users {
                        let ts = receipt
                            .get("ts")
                            .and_then(|v| v.as_u64())
                            .unwrap_or_default();
                        batch.receipts.push((
                            room_id_s.clone(),
                            event_id.clone(),
                            user_id.clone(),
                            ts,
                        ));
                    }
                }
            }
        }
    }
    for ev in &resp.to_device {
        let raw = ev.as_raw();
        let Ok(json) = serde_json::to_value(raw) else {
            continue;
        };
        let is_request = json
            .get("type")
            .and_then(|v| v.as_str())
            .map(|t| t == "m.key.verification.request")
            .unwrap_or(false);
        if !is_request {
            continue;
        }
        let sender = raw
            .get_field::<String>("sender")
            .ok()
            .flatten()
            .unwrap_or_default();
        let content: serde_json::Value =
            raw.get_field("content").ok().flatten().unwrap_or_default();
        let txn = content
            .get("transaction_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if sender.is_empty() || txn.is_empty() {
            continue;
        }
        crate::verify::note_request_flow(&sender, &txn);
        batch.verify_flows.push(crate::verify::IncomingRequest {
            flow_id: txn,
            user_id: sender,
            device_id: String::new(),
        });
    }
    for raw in &resp.presence {
        if let Some(update) = serde_json::to_value(raw)
            .ok()
            .and_then(|json| presence_update(&json))
        {
            batch.presence.push(update);
        }
    }
    batch
}

/// Sender avatar mxc from the member state event (cheap, cached by sync).
pub(in crate::app) async fn avatar_for_sender(
    room: &matrix_sdk::Room,
    sender: &str,
) -> Option<String> {
    use matrix_sdk::ruma::OwnedUserId;
    let Ok(uid) = OwnedUserId::try_from(sender) else {
        return None;
    };
    let member = room.get_member_no_sync(&uid).await.ok()??;
    member.avatar_url().map(|u| u.to_string())
}

pub(in crate::app) async fn display_name_for_sender(
    room: &matrix_sdk::Room,
    sender: &str,
) -> Option<String> {
    use matrix_sdk::ruma::OwnedUserId;
    let Ok(uid) = OwnedUserId::try_from(sender) else {
        return None;
    };
    let member = room.get_member_no_sync(&uid).await.ok()??;
    member.display_name().map(|n| n.to_owned())
}

pub(in crate::app) fn empty_row(body: &str) -> TimelineRow {
    TimelineRow {
        id: "empty".into(),
        ts: format_ts(now_millis()),
        origin_server_ts: 0,
        sender: "system".into(),
        display_name: "system".into(),
        body: body.into(),
        formatted: None,
        avatar_mxc: None,
        reply_to: None,
        reply_to_id: None,
        thread_count: 0,
        image: None,
        audio: None,
        reactions: vec![],
        is_sticker: false,
        txn_id: None,
        edited: false,
        seen_by: Vec::new(),
    }
}
