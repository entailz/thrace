/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Live sync, history paging and reaction backfill — everything that keeps rows current.

use crate::app::decode::{
    decode_sync_batch, load_initial_history, load_room_history, INITIAL_HISTORY, RELATION_BURST,
    RELATION_PAGE,
};
use crate::app::rows::{
    edit_in, fold_reaction, mentions_user, merge_initial_history, merge_row, parse_relation,
    receipt_target, redact_in, seen_in, RowMerge,
};
use crate::app::text::snippet;
use crate::app::{
    InitialHistory, RelationResults, SendResult, SyncBatch, SyncReaction, ThraceApp, TimelineRow,
};

/// Reaction senders the server aggregated before this session, so tooltips
/// name names and redaction matches. Encrypted relations decrypt per event.
pub(in crate::app) async fn fetch_message_reactions(
    client: &matrix_sdk::Client,
    room: &matrix_sdk::Room,
    target: &str,
) -> Option<Vec<SyncReaction>> {
    use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
    let (Ok(room_id), Ok(event_id)) = (
        OwnedRoomId::try_from(room.room_id().as_str()),
        OwnedEventId::try_from(target),
    ) else {
        // A malformed id is nothing to retry; report it resolved and empty.
        return Some(Vec::new());
    };
    // Ask by relation type only. Filtering by event type misses encrypted rooms,
    // where reactions travel as plain `m.reaction` rather than `m.room.encrypted`
    // — and a server that does encrypt them sends the other kind. The loop below
    // handles both, so let it decide rather than the query.
    let mut request =
        matrix_sdk::ruma::api::client::relations::get_relating_events_with_rel_type::v1::Request::new(
            room_id,
            event_id,
            matrix_sdk::ruma::events::relation::RelationType::Annotation,
        );
    request.limit = matrix_sdk::ruma::UInt::new(RELATION_PAGE.into());
    let Ok(response) = client.send(request).await else {
        return None;
    };
    let mut out = Vec::new();
    for raw in response.chunk {
        let Ok(json) = serde_json::to_value(&raw) else {
            continue;
        };
        if let Some(react) = parse_relation(&json, target) {
            out.push(react);
            continue;
        }
        if json.get("type").and_then(|v| v.as_str()) != Some("m.room.encrypted") {
            continue;
        }
        let cast = raw.cast_ref_unchecked::<
            matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent,
        >();
        let Ok(decrypted) = room.decrypt_event(cast, None).await else {
            continue;
        };
        let Ok(json) = serde_json::to_value(decrypted.raw()) else {
            continue;
        };
        if let Some(react) = parse_relation(&json, target) {
            out.push(react);
        }
    }
    Some(out)
}

/// Timeline events a single `/sync` may carry per room. The initial sync is
/// unbounded server-side and dominates first-paint cost.
pub(in crate::app) const SYNC_TIMELINE_LIMIT: u32 = 20;

/// Sync filter: lazy-loaded members and a bounded timeline. Without it the
/// server sends every member event per room on initial sync, which we discard.
pub(in crate::app) fn sync_filter() -> matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter
{
    use matrix_sdk::ruma::api::client::filter::{
        FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter,
    };
    use matrix_sdk::ruma::api::client::sync::sync_events;

    let lazy = LazyLoadOptions::Enabled {
        include_redundant_members: false,
    };
    let mut state = RoomEventFilter::default();
    state.lazy_load_options = lazy;

    let mut timeline = RoomEventFilter::default();
    timeline.lazy_load_options = lazy;
    timeline.limit = matrix_sdk::ruma::UInt::new(SYNC_TIMELINE_LIMIT.into());

    let mut room = RoomFilter::default();
    room.state = state;
    room.timeline = timeline;

    let mut filter = FilterDefinition::default();
    filter.room = room;
    sync_events::v3::Filter::FilterDefinition(filter)
}

impl ThraceApp {
    /// Cancel requests and discard old channels when changing accounts.
    pub(in crate::app) fn stop_history_loading(&mut self) {
        for (_, task) in self.history_tasks.drain() {
            task.abort();
        }
        self.history_queue = Default::default();
        self.history_rx = None;
        self.history_tx = None;
        self.page_rx = None;
        self.page_tx = None;
        self.paginating.clear();
        self.pending_scroll_fix = None;
    }

    /// Fill a small background queue, keeping a slot for the selected room.
    pub(in crate::app) fn pump_history(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.history_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.history_tx = Some(tx);
            self.history_rx = Some(rx);
        }
        let selected = self.current_room_id();
        for room_id in self.history_queue.next_batch(selected.as_deref()) {
            // Local DM stub: no room on the server yet; `poll_dm` swaps in
            // the real id and history loads then.
            if room_id.starts_with("dm:") {
                continue;
            }
            if !self.rooms.iter().any(|room| room.room_id == room_id) {
                self.history_queue.complete(&room_id, false);
                continue;
            }
            let client = client.clone();
            let tx = self.history_tx.clone().unwrap();
            let ctx = self.ctx.clone();
            let id = room_id.clone();
            let task = self.rt.spawn(async move {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(45),
                    load_initial_history(&client, &id),
                )
                .await
                .unwrap_or_else(|_| Err("history request timed out".into()));
                let _ = tx.send(InitialHistory {
                    room_id: id,
                    result,
                });
                ctx.request_repaint();
            });
            self.history_tasks.insert(room_id, task);
        }
    }

    /// Merge initial history with any live messages received while it loaded.
    pub(in crate::app) fn poll_history(&mut self) {
        loop {
            let Some(reply) = self.history_rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
                break;
            };
            let room_id = reply.room_id;
            self.history_tasks.remove(&room_id);
            self.history_queue.complete(&room_id, reply.result.is_ok());
            if !self.rooms.iter().any(|room| room.room_id == room_id) {
                continue;
            }
            let (history, token, members) = match reply.result {
                Ok(data) => data,
                Err(error) => {
                    if self.current_room_id().as_deref() == Some(&room_id) {
                        self.status = format!("Could not load messages: {error}");
                    }
                    continue;
                }
            };
            if let Some(token) = token {
                self.back_tokens.insert(room_id.clone(), token);
            }
            if let Some(room) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
                if room.is_dm && room.avatar_mxc.is_none() {
                    room.avatar_mxc = members
                        .iter()
                        .find(|m| m.mxid != self.own_user)
                        .and_then(|m| m.avatar_mxc.clone());
                }
            }
            let is_current = self.current_room_id().as_deref() == Some(&room_id);
            let target = if is_current {
                self.members = members;
                std::rc::Rc::make_mut(&mut self.rows)
            } else {
                self.members_by_room.insert(room_id.clone(), members);
                self.timelines.entry(room_id.clone()).or_default()
            };
            merge_initial_history(target, history);
            let latest = target
                .iter()
                .rev()
                .find(|row| row.sender != "system")
                .cloned();
            if let Some(latest) = latest {
                self.refresh_preview(&room_id, &latest);
            }
        }
    }

    /// Abort the live-sync task and drop its channel.
    pub(in crate::app) fn stop_live_sync(&mut self) {
        if let Some(task) = self.sync_task.take() {
            task.abort();
        }
        self.sync_running = false;
        self.sync_rx = None;
    }

    /// Live sync loop: decode `/sync` batches off-thread, pump to `sync_rx`.
    pub(in crate::app) fn ensure_live_sync(&mut self, _ctx: &egui::Context) {
        if self.sync_running || self.client.is_none() {
            return;
        }
        self.sync_running = true;
        let client = self.client.clone().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        self.sync_rx = Some(rx);
        let ctx = self.ctx.clone();
        let task = self.rt.spawn(async move {
            let settings = matrix_sdk::config::SyncSettings::default()
                .timeout(std::time::Duration::from_secs(30))
                .filter(sync_filter());
            let _ = client
                .sync_with_callback(settings, |resp| {
                    let tx = tx.clone();
                    let ctx = ctx.clone();
                    let client = client.clone();
                    async move {
                        let batch = decode_sync_batch(&client, &resp).await;
                        if batch.is_empty() {
                            // Empty long-poll timeout: no repaint owed.
                            return matrix_sdk::LoopCtrl::Continue;
                        }
                        if tx.send(batch).is_err() {
                            // Receiver dropped (logout): stop syncing.
                            return matrix_sdk::LoopCtrl::Break;
                        }
                        ctx.request_repaint();
                        matrix_sdk::LoopCtrl::Continue
                    }
                })
                .await;
        });
        self.sync_task = Some(task);
    }

    /// Loaded messages still needing their reaction senders resolved, newest
    /// first. Unresolved ones are picked *before* the cap applies — capping
    /// first pinned the window to the newest screenful, which filled `fetched`
    /// and then starved every older page behind it.
    pub(in crate::app) fn relation_targets(
        rows: &[TimelineRow],
        fetched: &std::collections::HashSet<String>,
        limit: usize,
    ) -> Vec<String> {
        rows.iter()
            .rev()
            .filter(|r| r.sender != "system" && r.id.starts_with('$'))
            .map(|r| r.id.as_str())
            .filter(|id| !fetched.contains(*id))
            .take(limit)
            .map(str::to_owned)
            .collect()
    }

    /// Backfill reaction senders for the messages this room has loaded, a burst
    /// at a time. Called every frame, so a page resolves in full as soon as it
    /// lands — the first page on open, an older page when scrollback brings it
    /// in — without the reader having to scroll past each message. Failures go
    /// back to unfetched, so the next frame retries them.
    pub(in crate::app) fn pump_relations(&mut self) {
        if self.relations_busy {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        let targets = Self::relation_targets(&self.rows, &self.relations_fetched, RELATION_BURST);
        if targets.is_empty() {
            return;
        }
        for id in &targets {
            self.relations_fetched.insert(id.clone());
        }
        if self.react_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.react_tx = Some(tx);
            self.react_rx = Some(rx);
        }
        let tx = self.react_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        self.relations_busy = true;
        self.rt.spawn(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let room = OwnedRoomId::try_from(room_id.clone())
                .ok()
                .and_then(|rid| client.get_room(&rid));
            // Every target must come back with a verdict, or ids that went out
            // stay marked fetched and their reactions never resolve.
            let mut pending: std::collections::HashSet<String> = targets.iter().cloned().collect();
            let mut done: RelationResults = Vec::with_capacity(targets.len());
            if let Some(room) = room {
                // One round trip per message, so run the burst together —
                // sequentially a page of history takes seconds to fill in.
                let mut set = tokio::task::JoinSet::new();
                for target in targets {
                    let (client, room) = (client.clone(), room.clone());
                    set.spawn(async move {
                        let reactions = fetch_message_reactions(&client, &room, &target).await;
                        (target, reactions)
                    });
                }
                while let Some(joined) = set.join_next().await {
                    if let Ok((target, reactions)) = joined {
                        pending.remove(&target);
                        done.push((target, reactions));
                    }
                }
            }
            // Room gone, or a lookup that panicked and never reported: mark it
            // failed so the id goes back to unfetched and the next frame retries.
            done.extend(pending.into_iter().map(|target| (target, None)));
            // Always report, even when every message came back bare: the reply is
            // what clears `relations_busy`.
            let _ = tx.send((room_id, done));
            ctx.request_repaint();
        });
    }

    /// Drain backfilled reaction senders into their rows and release the burst guard.
    pub(in crate::app) fn poll_relations(&mut self) {
        let ready: Vec<(String, RelationResults)> = self
            .react_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        if ready.is_empty() {
            return;
        }
        self.relations_busy = false;
        let mut failed = 0;
        for (room_id, results) in ready {
            for (target, result) in results {
                match result {
                    Some(reactions) => {
                        for react in reactions {
                            self.apply_sync_reaction(&room_id, react);
                        }
                    }
                    None => {
                        self.relations_fetched.remove(&target);
                        failed += 1;
                    }
                }
            }
        }
        if failed > 0 {
            eprintln!("thrace: {failed} reaction backfill requests failed; will retry");
        }
    }

    /// Drain live-sync batches into rooms; bump unread for background rooms.
    pub(in crate::app) fn poll_live_sync(&mut self) {
        loop {
            let batch = match &self.sync_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(batch) = batch else { break };
            self.apply_sync_batch(batch);
        }
    }

    pub(in crate::app) fn apply_sync_batch(&mut self, mut batch: SyncBatch) {
        if let Some(usage) = batch.emoji_usage.take() {
            self.emoji_usage.merge(usage);
            self.recent_emoji = self.emoji_usage.frequent(18);
        }
        let cur_id = self.rooms.get(self.current).map(|r| r.room_id.clone());
        let own = self.own_user.clone();
        for (room_id, row) in batch.rows.drain(..) {
            let is_current = Some(&room_id) == cur_id.as_ref();
            let is_own = !own.is_empty() && row.sender == own;
            let mentions = mentions_user(&row, &own);
            let preview_row = (row.sender != "system").then(|| row.clone());
            let merged = if is_current {
                merge_row(std::rc::Rc::make_mut(&mut self.rows), row)
            } else {
                // Echo reconciliation covers background rooms too.
                merge_row(self.timelines.entry(room_id.clone()).or_default(), row)
            };
            // Only new messages from others badge a room.
            if merged == RowMerge::Appended && !is_current && !is_own {
                self.bump_unread(&room_id, mentions);
            }
            if let Some(last) = preview_row {
                self.refresh_preview(&room_id, &last);
            }
        }
        for (room_id, react) in batch.reactions {
            self.apply_sync_reaction(&room_id, react);
        }
        for (room_id, event_id, user_id) in batch.receipts {
            self.apply_sync_receipt(&room_id, &event_id, &user_id);
        }
        for (room_id, target, body, formatted) in batch.edits {
            self.apply_edit(&room_id, &target, body, formatted);
        }
        for (room_id, redacted) in batch.redactions {
            self.apply_redaction(&room_id, &redacted);
        }
        for flow in batch.verify_flows {
            if self.seen_incoming.insert(flow.flow_id.clone()) {
                self.incoming = Some(flow);
                self.status = "incoming verification — accept?".into();
            }
        }
    }

    /// The timeline to mutate for `room_id`: the live rows when that room is on
    /// screen, the stashed timeline otherwise. `None` for a room we hold nothing for.
    pub(in crate::app) fn timeline_for_mut(
        &mut self,
        room_id: &str,
    ) -> Option<&mut Vec<TimelineRow>> {
        let current = self
            .rooms
            .get(self.current)
            .is_some_and(|r| r.room_id == room_id);
        if current {
            Some(std::rc::Rc::make_mut(&mut self.rows))
        } else {
            self.timelines.get_mut(room_id)
        }
    }

    /// Rewrite a message in place from an `m.replace`.
    pub(in crate::app) fn apply_edit(
        &mut self,
        room_id: &str,
        target: &str,
        body: String,
        formatted: Option<String>,
    ) {
        if let Some(rows) = self.timeline_for_mut(room_id) {
            edit_in(rows.as_mut_slice(), target, body, formatted);
        }
    }

    /// Apply one `m.room.redaction`: drop reaction or tombstone message.
    pub(in crate::app) fn apply_redaction(&mut self, room_id: &str, redacted: &str) {
        if let Some(rows) = self.timeline_for_mut(room_id) {
            redact_in(rows.as_mut_slice(), redacted);
        }
    }

    pub(in crate::app) fn bump_unread(&mut self, room_id: &str, mentioned: bool) {
        if let Some(room) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
            room.unread += 1;
            room.mentioned |= mentioned;
        }
    }

    /// Refresh a room's one-line preview from its newest real message.
    pub(in crate::app) fn refresh_preview(&mut self, room_id: &str, row: &TimelineRow) {
        if row.sender == "system" {
            return;
        }
        let text = format!("{}: {}", row.display_name, snippet(&row.body, 48));
        if let Some(room) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
            room.preview = Some(text);
        }
    }

    /// Fold one wire reaction into its target; ignore unknown targets.
    pub(in crate::app) fn apply_sync_reaction(&mut self, room_id: &str, react: SyncReaction) {
        if let Some(rows) = self.timeline_for_mut(room_id) {
            fold_reaction(rows.as_mut_slice(), &react);
        }
    }

    /// Fold one `m.read` receipt into its target; receipts move forward.
    pub(in crate::app) fn apply_sync_receipt(
        &mut self,
        room_id: &str,
        event_id: &str,
        user_id: &str,
    ) {
        if let Some(rows) = self.timeline_for_mut(room_id) {
            seen_in(rows.as_mut_slice(), event_id, user_id);
        }
    }

    /// Mark the newest message read, when the reader has actually reached it.
    /// `newest_visible` is the last real row on screen this frame.
    pub(in crate::app) fn send_current_read_receipt(&mut self, newest_visible: Option<&str>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        // `None` means the backend does not report focus; assume focused rather
        // than never sending a receipt there.
        let focused = self.ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let Some(target) = receipt_target(
            &self.rows,
            newest_visible,
            focused,
            self.last_receipt.get(&room_id).map(String::as_str),
        )
        .map(str::to_owned) else {
            return;
        };
        self.last_receipt.insert(room_id.clone(), target.clone());
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(room_id) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(event_id) = OwnedEventId::try_from(target) else {
                return SendResult::Failed("bad event id".into());
            };
            let Some(room) = client.get_room(&room_id) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
            use matrix_sdk::ruma::events::receipt::ReceiptThread;
            match room
                .send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
                .await
            {
                // Silent: receipts are background traffic.
                Ok(()) => SendResult::Done(String::new()),
                Err(error) => SendResult::Failed(format!("receipt: {error}")),
            }
        });
    }

    /// Switch room; swaps `rows` from per-room cache.
    pub(in crate::app) fn switch_room(&mut self, idx: usize) {
        if idx >= self.rooms.len() {
            return;
        }
        // Drop unused DM stubs so misclicks leave no dead entries.
        let idx = match self.take_empty_dm_stub(idx) {
            Some(adjusted) => adjusted,
            None => return,
        };
        if idx == self.current {
            // Already there: stashing below would round-trip the live rows
            // through the cache and come back empty.
            return;
        }
        // Stash outgoing timeline + members — unless we already arrived:
        // dropping an empty DM stub points `current` at the target, and
        // stashing then would overwrite its cache with empty rows.
        let target = self.rooms.get(idx).map(|r| r.room_id.clone());
        if let Some(cur) = self.rooms.get(self.current).map(|r| r.room_id.clone()) {
            if Some(cur.as_str()) != target.as_deref() {
                self.timelines.insert(
                    cur.clone(),
                    std::mem::take(std::rc::Rc::make_mut(&mut self.rows)),
                );
                self.members_by_room
                    .insert(cur, std::mem::take(&mut self.members));
            }
        }
        self.current = idx;
        let id = self.rooms[idx].room_id.clone();
        self.rows = std::rc::Rc::new(self.timelines.remove(&id).unwrap_or_default());
        self.history_queue.select(&id);
        self.pump_history();
        self.members = self.members_by_room.remove(&id).unwrap_or_default();
        self.replying_to = None;
        self.react_target = None;
        self.rooms[idx].unread = 0;
        self.rooms[idx].mentioned = false;
        // The wire receipt follows from the timeline once it renders at the
        // bottom; clearing the badge here is only the local hint.
    }

    /// Drop unused DM stub; return `idx` adjusted (`None` if target vanished).
    pub(in crate::app) fn take_empty_dm_stub(&mut self, idx: usize) -> Option<usize> {
        let cur = self.current;
        if cur == idx || cur >= self.rooms.len() {
            return Some(idx);
        }
        let room = &self.rooms[cur];
        let is_stub = room.room_id.starts_with("dm:");
        if !is_stub {
            return Some(idx);
        }
        let unused = self
            .rows
            .iter()
            .all(|r| r.sender == "system" || r.id == "dm-new");
        if !unused {
            return Some(idx);
        }
        let stub_id = room.room_id.clone();
        self.rooms.remove(cur);
        self.timelines.remove(&stub_id);
        self.members_by_room.remove(&stub_id);
        std::rc::Rc::make_mut(&mut self.rows).clear();
        self.members.clear();
        // Removal shifts later entries down one.
        let idx = if idx > cur { idx - 1 } else { idx };
        if idx >= self.rooms.len() {
            self.current = self.rooms.len().saturating_sub(1);
            return None;
        }
        self.current = idx;
        Some(idx)
    }

    /// Fetch the next older page once initial history has arrived.
    pub(in crate::app) fn paginate_current(&mut self) {
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        if self.paginating.contains(&room_id) || !self.history_queue.is_loaded(&room_id) {
            return;
        }
        let Some(token) = self.back_tokens.get(&room_id).cloned() else {
            // No token: at the start of the room.
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.page_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.page_tx = Some(tx);
            self.page_rx = Some(rx);
        }
        let tx = self.page_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        // Pre-resolve known sender avatars; avoids a store hit per message.
        let avatars: std::collections::HashMap<String, Option<String>> = self
            .members
            .iter()
            .map(|m| (m.mxid.clone(), m.avatar_mxc.clone()))
            .collect();
        let names: std::collections::HashMap<String, String> = self
            .members
            .iter()
            .map(|m| (m.mxid.clone(), m.display.clone()))
            .collect();
        self.paginating.insert(room_id.clone());
        self.rt.spawn(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let rows_and_token = match OwnedRoomId::try_from(room_id.clone())
                .ok()
                .and_then(|rid| client.get_room(&rid))
            {
                Some(room) => {
                    load_room_history(&room, &avatars, &names, Some(token), INITIAL_HISTORY).await
                }
                None => Err("room not found".into()),
            };
            let _ = tx.send((room_id, rows_and_token));
            ctx.request_repaint();
        });
    }

    /// Prepend an arriving older page above the visible rows.
    pub(in crate::app) fn apply_page(
        &mut self,
        room_id: String,
        mut page: Vec<TimelineRow>,
        token: Option<String>,
    ) {
        self.paginating.remove(&room_id);
        match token {
            Some(t) => {
                self.back_tokens.insert(room_id.clone(), t);
            }
            // No further token: no more history.
            None => {
                self.back_tokens.remove(&room_id);
            }
        }
        if page.is_empty() {
            return;
        }
        let target = if self.current_room_id().as_deref() == Some(room_id.as_str()) {
            std::rc::Rc::make_mut(&mut self.rows)
        } else if let Some(tl) = self.timelines.get_mut(&room_id) {
            tl
        } else {
            return;
        };
        // Pages can overlap live sync; drop rows already on screen.
        let known: std::collections::HashSet<&str> = target.iter().map(|r| r.id.as_str()).collect();
        page.retain(|r| !known.contains(r.id.as_str()));
        if page.is_empty() {
            return;
        }
        // Hold the view steady against insertion above the viewport.
        if self
            .rooms
            .get(self.current)
            .is_some_and(|room| room.room_id == room_id)
        {
            self.pending_scroll_fix = Some(self.last_content_height);
        }
        target.splice(..0, page);
    }
}
