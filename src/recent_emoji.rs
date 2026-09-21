/*
SPDX-License-Identifier: AGPL-3.0-only
*/

//! Emoji usage stored as Matrix account data, not client configuration.

use serde::{Deserialize, Serialize};

const EVENT_TYPE: &str = "m.recent_emoji";
const STORAGE_LIMIT: usize = 100;

/// Interoperable account-data content, ordered by most recent use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentEmoji {
    #[serde(default)]
    recent_emoji: Vec<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Usage {
    emoji: String,
    total: u64,
}

impl RecentEmoji {
    /// Restore usage from the SDK's account-data store after sync.
    pub async fn load(client: &matrix_sdk::Client) -> Result<Self, String> {
        let raw = client
            .account()
            .account_data_raw(EVENT_TYPE.into())
            .await
            .map_err(|e| e.to_string())?;
        match raw {
            Some(raw) => serde_json::from_str(raw.json().get()).map_err(|e| e.to_string()),
            None => Ok(Self::default()),
        }
    }

    /// Save through Matrix; subsequent sync also persists this in the SDK store.
    pub async fn save(&self, client: &matrix_sdk::Client) -> Result<(), String> {
        let json = serde_json::value::to_raw_value(self).map_err(|e| e.to_string())?;
        let raw = matrix_sdk::ruma::serde::Raw::from_json(json);
        client
            .account()
            .set_account_data_raw(EVENT_TYPE.into(), raw)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Count a selection, retaining a bounded history of distinct emojis.
    pub fn record(&mut self, emoji: &str) {
        let total = self
            .recent_emoji
            .iter()
            .find(|entry| entry.emoji == emoji)
            .map_or(1, |entry| entry.total.saturating_add(1));
        self.recent_emoji.retain(|entry| entry.emoji != emoji);
        self.recent_emoji.insert(
            0,
            Usage {
                emoji: emoji.into(),
                total,
            },
        );
        self.recent_emoji.truncate(STORAGE_LIMIT);
    }

    /// Rank by frequency, preserving recency for ties.
    pub fn frequent(&self, limit: usize) -> Vec<String> {
        let mut entries = self.recent_emoji.clone();
        entries.sort_by(|a, b| b.total.cmp(&a.total));
        entries
            .into_iter()
            .take(limit)
            .map(|entry| entry.emoji)
            .collect()
    }

    /// Do not let an older sync echo undo locally counted picks.
    pub fn merge(&mut self, incoming: Self) {
        let mut updates = Vec::new();
        for entry in incoming.recent_emoji {
            let current = self.recent_emoji.iter().find(|e| e.emoji == entry.emoji);
            if current.is_none_or(|existing| entry.total > existing.total) {
                self.recent_emoji
                    .retain(|existing| existing.emoji != entry.emoji);
                updates.push(entry);
            }
        }
        updates.append(&mut self.recent_emoji);
        self.recent_emoji = updates;
        self.recent_emoji.truncate(STORAGE_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_rank_usage_with_recent_picks_breaking_ties() {
        let mut usage = RecentEmoji::default();
        for emoji in ["a", "a", "b", "b", "c"] {
            usage.record(emoji);
        }
        assert_eq!(usage.frequent(3), ["b", "a", "c"]);
        let encoded = serde_json::to_value(&usage).unwrap();
        assert_eq!(
            encoded["recent_emoji"][0],
            serde_json::json!({"emoji": "c", "total": 1})
        );
        let restored: RecentEmoji = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored.frequent(3), usage.frequent(3));
    }

    #[test]
    fn new_remote_picks_are_kept_when_history_is_full() {
        let mut usage = RecentEmoji::default();
        for n in 0..100 {
            usage.record(&n.to_string());
        }
        let mut remote = RecentEmoji::default();
        remote.record("remote");
        usage.merge(remote);
        assert_eq!(usage.frequent(1), ["remote"]);
        assert_eq!(usage.recent_emoji.len(), STORAGE_LIMIT);
    }

    #[test]
    fn stale_sync_does_not_undo_picks_and_history_is_bounded() {
        let mut usage = RecentEmoji::default();
        usage.record("\u{1F44D}\u{1F3FD}");
        let stale = usage.clone();
        usage.record("\u{1F44D}\u{1F3FD}");
        usage.merge(stale);
        assert_eq!(usage.recent_emoji[0].total, 2);
        for n in 0..120 {
            usage.record(&n.to_string());
        }
        assert_eq!(usage.recent_emoji.len(), STORAGE_LIMIT);
        assert_eq!(usage.recent_emoji[0].emoji, "119");
    }
}
