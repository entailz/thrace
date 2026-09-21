/*
SPDX-License-Identifier: AGPL-3.0-only
*/

//! Bounded history prefetching, with the selected room ahead of background work.

use std::collections::{HashSet, VecDeque};

/// Tracks initial history requests independently of messages received through sync.
#[derive(Default)]
pub struct HistoryQueue {
    pending: VecDeque<String>,
    active: HashSet<String>,
    loaded: HashSet<String>,
    failed: HashSet<String>,
}

impl HistoryQueue {
    /// Queue rooms in sidebar order.
    pub fn new(rooms: impl IntoIterator<Item = String>) -> Self {
        Self {
            pending: rooms.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Explicit selection retries failures and queues newly joined rooms.
    pub fn select(&mut self, room: &str) {
        self.failed.remove(room);
        if !self.loaded.contains(room) && !self.active.contains(room) {
            self.pending.retain(|id| id != room);
            self.pending.push_front(room.to_owned());
        }
    }

    /// Start the selected room first, reserving a third slot for foreground work.
    pub fn next_batch(&mut self, selected: Option<&str>) -> Vec<String> {
        let mut batch = Vec::new();
        if let Some(room) = selected {
            if self.active.len() < 3
                && !self.loaded.contains(room)
                && !self.active.contains(room)
                && !self.failed.contains(room)
            {
                self.pending.retain(|id| id != room);
                self.active.insert(room.to_owned());
                batch.push(room.to_owned());
            }
        }
        // Give the first visible history request the connection to itself.
        if selected.is_some() && self.loaded.is_empty() && self.failed.is_empty() {
            return batch;
        }
        let limit = if selected.is_some_and(|id| self.active.contains(id)) {
            3
        } else {
            2
        };
        while self.active.len() < limit {
            let Some(room) = self.pending.pop_front() else {
                break;
            };
            if self.loaded.contains(&room)
                || self.active.contains(&room)
                || self.failed.contains(&room)
            {
                continue;
            }
            self.active.insert(room.clone());
            batch.push(room);
        }
        batch
    }

    /// Complete a request. Failed rooms retry only after explicit selection.
    pub fn complete(&mut self, room: &str, success: bool) {
        self.active.remove(room);
        if success {
            self.loaded.insert(room.to_owned());
        } else {
            self.failed.insert(room.to_owned());
        }
    }

    /// Rename a room id everywhere: a local DM stub becoming the real room.
    pub fn retarget(&mut self, from: &str, to: &str) {
        for id in self.pending.iter_mut().filter(|id| id.as_str() == from) {
            *id = to.to_owned();
        }
        if self.active.remove(from) {
            self.active.insert(to.to_owned());
        }
        if self.loaded.remove(from) {
            self.loaded.insert(to.to_owned());
        }
        if self.failed.remove(from) {
            self.failed.insert(to.to_owned());
        }
    }

    pub fn is_loaded(&self, room: &str) -> bool {
        self.loaded.contains(room)
    }
    pub fn has_failed(&self, room: &str) -> bool {
        self.failed.contains(room)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> HistoryQueue {
        HistoryQueue::new(["a", "b", "c", "d", "e", "f"].map(str::to_owned))
    }

    #[test]
    fn first_chat_loads_alone_then_background_work_is_bounded() {
        let mut queue = queue();
        assert_eq!(queue.next_batch(Some("c")), ["c"]);
        assert!(queue.next_batch(Some("c")).is_empty());
        queue.complete("c", true);
        assert_eq!(queue.next_batch(Some("c")), ["a", "b"]);
        assert!(queue.next_batch(Some("c")).is_empty());
        queue.select("f");
        assert_eq!(queue.next_batch(Some("f")), ["f"]);
        queue.select("e");
        assert!(queue.next_batch(Some("e")).is_empty());
        queue.complete("a", true);
        assert_eq!(queue.next_batch(Some("e")), ["e"]);
        assert_eq!(queue.active.len(), 3);
    }

    #[test]
    fn failures_retry_on_selection_and_loaded_rooms_are_not_fetched_again() {
        let mut queue = queue();
        assert_eq!(queue.next_batch(Some("a")), ["a"]);
        queue.complete("a", false);
        assert_eq!(queue.next_batch(Some("a")), ["b", "c"]);
        assert!(queue.has_failed("a"));
        queue.select("a");
        assert_eq!(queue.next_batch(Some("a")), ["a"]);
        queue.complete("a", true);
        queue.select("a");
        assert!(queue.next_batch(Some("a")).is_empty());
        assert!(queue.is_loaded("a"));
    }

    #[test]
    fn dm_stub_becomes_the_real_room() {
        let mut queue = HistoryQueue::new(["!real:hs".to_owned()]);
        queue.select("dm:@bob:hs");
        assert_eq!(queue.next_batch(Some("dm:@bob:hs")), ["dm:@bob:hs"]);
        queue.retarget("dm:@bob:hs", "!dm:hs");
        // The in-flight stub request now counts for the real room.
        queue.complete("!dm:hs", true);
        assert!(queue.is_loaded("!dm:hs"));
        assert!(!queue.has_failed("dm:@bob:hs"));
    }
}
