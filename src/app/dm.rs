/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Direct messages: finding an existing DM, creating one, and the local stub that stands in.

use crate::app::text::{format_ts, now_millis};
use crate::app::{DmResult, Member, RoomEntry, ThraceApp, TimelineRow};

impl ThraceApp {
    /// Index of the DM with `mxid`, if the sidebar already has one.
    /// Matches by user id only: display names collide and nicknames change,
    /// so neither may stand in for the mxid.
    pub(in crate::app) fn find_dm(&self, mxid: &str) -> Option<usize> {
        let stub_id = format!("dm:{mxid}");
        self.rooms.iter().enumerate().position(|(i, r)| {
            if !r.is_dm {
                return false;
            }
            if r.room_id == mxid || r.room_id == stub_id || r.name == mxid {
                return true;
            }
            let members = if i == self.current {
                Some(&self.members)
            } else {
                self.members_by_room.get(&r.room_id)
            };
            members.is_some_and(|ms| ms.iter().any(|m| m.mxid == mxid))
        })
    }

    /// Click a user → open the existing DM or create one.
    /// Matching is by mxid only: a nickname is not a user.
    pub(in crate::app) fn open_dm(&mut self, mxid: &str, display: &str) {
        let Ok(user_id) = matrix_sdk::ruma::OwnedUserId::try_from(mxid) else {
            self.status = format!("bad user id: {mxid}");
            return;
        };
        if let Some(idx) = self.find_dm(mxid) {
            // A leftover stub never finished creating: switch to it and retry.
            let is_stub = self
                .rooms
                .get(idx)
                .is_some_and(|r| r.room_id.starts_with("dm:"));
            self.switch_room(idx);
            if is_stub {
                self.spawn_dm_creation(format!("dm:{mxid}"), user_id, display);
            }
            return;
        }
        // The SDK may know a DM room the sidebar hasn't listed yet.
        if let Some(client) = self.client.clone() {
            if let Some(room) = client.get_dm_room(&user_id) {
                let id = room.room_id().to_string();
                if let Some(idx) = self.rooms.iter().position(|r| r.room_id == id) {
                    self.switch_room(idx);
                } else {
                    let avatar = self
                        .members
                        .iter()
                        .chain(self.members_by_room.values().flatten())
                        .find(|m| m.mxid == mxid)
                        .and_then(|m| m.avatar_mxc.clone());
                    self.switch_to_room_entry(
                        RoomEntry {
                            room_id: id,
                            name: display.into(),
                            unread: 0,
                            mentioned: false,
                            is_dm: true,
                            avatar_mxc: avatar,
                            ..Default::default()
                        },
                        vec![Member {
                            display: display.into(),
                            mxid: mxid.into(),
                            avatar_mxc: None,
                            power: 0,
                            presence: None,
                        }],
                    );
                }
                return;
            }
        }
        // No DM room: stub locally so chat switches now, then create the
        // real room. `poll_dm` swaps the stub id for the real one.
        let stub_id = format!("dm:{mxid}");
        // Search every loaded room; profile cards appear anywhere.
        let avatar = self
            .members
            .iter()
            .chain(self.members_by_room.values().flatten())
            .find(|m| m.mxid == mxid)
            .and_then(|m| m.avatar_mxc.clone());
        self.switch_to_room_entry(
            RoomEntry {
                room_id: stub_id.clone(),
                name: format!("@{display}"),
                unread: 0,
                mentioned: false,
                is_dm: true,
                avatar_mxc: avatar,
                ..Default::default()
            },
            vec![Member {
                display: display.into(),
                mxid: mxid.into(),
                avatar_mxc: None,
                power: 0,
                presence: None,
            }],
        );
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: "dm-new".into(),
            ts: format_ts(now_millis()),
            origin_server_ts: 0,
            sender: "system".into(),
            display_name: "system".into(),
            body: format!("Direct message with {display} ({mxid}) — creating room…"),
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
        // Logged-out creation is refused inside; the stub still switches.
        self.spawn_dm_creation(stub_id, user_id, display);
    }

    /// Ask the server for the real DM room; `poll_dm` swaps the stub id out.
    pub(in crate::app) fn spawn_dm_creation(
        &mut self,
        stub_id: String,
        user_id: matrix_sdk::ruma::OwnedUserId,
        display: &str,
    ) {
        let Some(client) = self.client.clone() else {
            self.status = "log in to create the DM".into();
            return;
        };
        if self.dm_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.dm_tx = Some(tx);
            self.dm_rx = Some(rx);
        }
        let tx = self.dm_tx.clone().expect("just created");
        let ctx = self.ctx.clone();
        self.status = format!("creating DM with {display} …");
        self.rt.spawn(async move {
            let msg = match client.create_dm(&user_id).await {
                Ok(room) => DmResult::Created {
                    stub_id,
                    room_id: room.room_id().to_string(),
                },
                Err(e) => DmResult::Failed(format!("create DM: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Push a new sidebar entry and switch to it, stashing the old room.
    pub(in crate::app) fn switch_to_room_entry(&mut self, entry: RoomEntry, members: Vec<Member>) {
        if let Some(cur) = self.rooms.get(self.current).map(|r| r.room_id.clone()) {
            self.timelines.insert(
                cur.clone(),
                std::mem::take(std::rc::Rc::make_mut(&mut self.rows)),
            );
            self.members_by_room
                .insert(cur, std::mem::take(&mut self.members));
        }
        self.rooms.push(entry);
        self.current = self.rooms.len() - 1;
        self.rows = std::rc::Rc::new(Vec::new());
        self.members = members;
        self.replying_to = None;
        self.react_target = None;
        self.history_queue
            .select(self.rooms[self.current].room_id.as_str());
        self.pump_history();
    }

    /// Drain finished DM creations: swap the stub id for the real room.
    pub(in crate::app) fn poll_dm(&mut self) {
        let msg = match &self.dm_rx {
            Some(rx) => rx.try_recv().ok(),
            None => None,
        };
        let Some(msg) = msg else { return };
        match msg {
            DmResult::Created { stub_id, room_id } => {
                // Move any stashed state from the stub key to the real one.
                if let Some(rows) = self.timelines.remove(&stub_id) {
                    self.timelines.insert(room_id.clone(), rows);
                }
                if let Some(members) = self.members_by_room.remove(&stub_id) {
                    self.members_by_room.insert(room_id.clone(), members);
                }
                self.history_queue.retarget(&stub_id, &room_id);
                if let Some(entry) = self.rooms.iter_mut().find(|r| r.room_id == stub_id) {
                    entry.room_id = room_id.clone();
                    entry.name = entry.name.trim_start_matches('@').to_owned();
                }
                // Retire the "creating room…" placeholder if still on screen.
                for r in std::rc::Rc::make_mut(&mut self.rows)
                    .iter_mut()
                    .filter(|r| r.id == "dm-new")
                {
                    r.body = "Direct message — say hi".into();
                }
                self.status = "DM ready".into();
                self.pump_history();
            }
            DmResult::Failed(e) => {
                self.status = format!("send failed: {e}");
            }
        }
    }
}
