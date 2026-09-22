/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Timeline row surgery: folding sync events into a `Vec<TimelineRow>`.

use crate::app::{Reaction, Reactor, SyncReaction, ThraceApp, TimelineRow};

/// Split the spec's reply fallback (quoted lines, blank line, reply) off a body.
///
/// ```text
/// > <@alice:hs> original message
/// > second line of it
///
/// the actual reply
/// ```
///
/// Returns `(quoted (sender, text), body without the quote)`. Without the
/// fallback (MSC2781) there is nothing to strip; the renderer resolves the
/// target from the timeline instead.
pub(in crate::app) fn split_reply_fallback(body: &str) -> (Option<(String, String)>, String) {
    if !body.starts_with('>') {
        return (None, body.to_owned());
    }
    let mut sender: Option<String> = None;
    let mut quoted: Vec<String> = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    let mut in_quote = true;
    for line in body.lines() {
        if in_quote {
            if let Some(q) = line.strip_prefix('>') {
                let q = q.strip_prefix(' ').unwrap_or(q);
                if sender.is_none() {
                    if let Some((who, text)) = q.strip_prefix('<').and_then(|r| r.split_once("> "))
                    {
                        sender = Some(who.to_owned());
                        quoted.push(text.to_owned());
                        continue;
                    }
                    // Quoted, but not in the `<user>` form — still a fallback.
                    sender = Some(String::new());
                }
                quoted.push(q.to_owned());
                continue;
            }
            in_quote = false;
            if line.trim().is_empty() {
                continue;
            }
        }
        rest.push(line);
    }
    let preview = sender.map(|s| {
        let who = s
            .trim_start_matches('@')
            .split(':')
            .next()
            .filter(|w| !w.is_empty())
            .unwrap_or("?")
            .to_owned();
        (who, quoted.join(" "))
    });
    (preview, rest.join("\n"))
}

/// Fold one wire reaction into its target row. Unknown target: not paged in yet.
/// Server-bundled reaction counts from `unsigned.m.relations`, so chips survive
/// a restart instead of waiting for a live reaction to arrive.
pub(in crate::app) fn bundled_annotations(event: &serde_json::Value) -> Vec<Reaction> {
    let Some(chunk) = event
        .get("unsigned")
        .and_then(|u| u.get("m.relations"))
        .and_then(|r| r.get("m.annotation"))
        .and_then(|a| a.get("chunk"))
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    chunk
        .iter()
        .filter_map(|entry| {
            let key = entry.get("key").and_then(|v| v.as_str())?;
            let count = entry.get("count").and_then(|v| v.as_u64())? as usize;
            (!key.is_empty() && count > 0).then(|| Reaction {
                key: key.to_owned(),
                senders: Vec::new(),
                bundled: count,
            })
        })
        .collect()
}

pub(in crate::app) fn merge_reactions(target: &mut Vec<Reaction>, extra: &[Reaction]) {
    for group in extra {
        match target.iter_mut().find(|r| r.key == group.key) {
            Some(existing) => {
                existing.bundled = existing.bundled.max(group.bundled);
                for sender in &group.senders {
                    match existing.senders.iter_mut().find(|s| s.user == sender.user) {
                        Some(known) => {
                            if known.event_id.is_empty() {
                                known.event_id = sender.event_id.clone();
                            }
                        }
                        None => existing.senders.push(sender.clone()),
                    }
                }
            }
            None => target.push(group.clone()),
        }
    }
}

pub(in crate::app) fn parse_relation(
    json: &serde_json::Value,
    target: &str,
) -> Option<SyncReaction> {
    if json.get("type").and_then(|v| v.as_str()) != Some("m.reaction") {
        return None;
    }
    let sender = json.get("sender")?.as_str()?.to_owned();
    let event_id = json.get("event_id")?.as_str()?.to_owned();
    let relates = json.get("content")?.get("m.relates_to")?;
    if relates.get("event_id").and_then(|v| v.as_str()) != Some(target) {
        return None;
    }
    let key = relates.get("key")?.as_str()?.to_owned();
    if key.is_empty() {
        return None;
    }
    Some(SyncReaction {
        target: target.to_owned(),
        key,
        sender,
        event_id,
    })
}

pub(in crate::app) fn fold_reaction(rows: &mut [TimelineRow], react: &SyncReaction) -> bool {
    let Some(row) = rows.iter_mut().find(|r| r.id == react.target) else {
        return false;
    };
    match row.reactions.iter_mut().find(|g| g.key == react.key) {
        Some(group) => match group.senders.iter_mut().find(|s| s.user == react.sender) {
            // Local echo has no event id yet; without it un-react cannot redact.
            Some(existing) => existing.event_id = react.event_id.clone(),
            None => group.senders.push(Reactor {
                user: react.sender.clone(),
                event_id: react.event_id.clone(),
            }),
        },
        None => row.reactions.push(Reaction {
            key: react.key.clone(),
            senders: vec![Reactor {
                user: react.sender.clone(),
                event_id: react.event_id.clone(),
            }],
            bundled: 0,
        }),
    }
    true
}

/// Rewrite one message from an `m.replace`. Unknown target: not paged in, dropped.
pub(in crate::app) fn edit_in(
    rows: &mut [TimelineRow],
    target: &str,
    body: String,
    formatted: Option<String>,
) -> bool {
    let Some(row) = rows.iter_mut().find(|r| r.id == target) else {
        return false;
    };
    row.body = body;
    row.formatted = formatted;
    row.edited = true;
    true
}

pub(in crate::app) fn edit_replacement(
    content: &serde_json::Value,
) -> Option<(String, String, Option<String>)> {
    let relates = content.get("m.relates_to")?;
    if relates.get("rel_type").and_then(|v| v.as_str()) != Some("m.replace") {
        return None;
    }
    let target = relates
        .get("event_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let new = content.get("m.new_content").cloned().unwrap_or_default();
    let body = new
        .get("body")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            content
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim_start_matches("* ")
                .to_owned()
        });
    let formatted = new
        .get("formatted_body")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Some((target, body, formatted))
}

/// The event our read receipt should point at, or `None` when nothing is owed:
/// the newest message is off screen, the window is in the background, or that
/// message already carries our receipt. A scrolled-up reader has not seen it,
/// so arrival alone must never mark a message read.
pub(in crate::app) fn receipt_target<'a>(
    rows: &'a [TimelineRow],
    newest_visible: Option<&str>,
    focused: bool,
    last_sent: Option<&str>,
) -> Option<&'a str> {
    if !focused {
        return None;
    }
    let newest = rows
        .iter()
        .rev()
        .find(|r| r.sender != "system" && r.id.starts_with('$'))?;
    // Identity, not a pixel threshold: the newest message itself must be the
    // last one on screen.
    if newest_visible != Some(newest.id.as_str()) {
        return None;
    }
    (last_sent != Some(newest.id.as_str())).then_some(newest.id.as_str())
}

/// Move one person's read receipt onto `event_id`. Unknown target (they read
/// newer than paged in): ignored, so no avatar disappears.
pub(in crate::app) fn seen_in(rows: &mut [TimelineRow], event_id: &str, user_id: &str) -> bool {
    let Some(target) = rows.iter().position(|r| r.id == event_id) else {
        return false;
    };
    // Receipts only move forward.
    for (i, row) in rows.iter_mut().enumerate() {
        if i != target {
            row.seen_by.retain(|u| u != user_id);
        }
    }
    let row = &mut rows[target];
    if !row.seen_by.iter().any(|u| u == user_id) {
        row.seen_by.push(user_id.to_owned());
    }
    true
}

/// Body shown for a redacted message. Shared with the test asserting it.
pub(in crate::app) const DELETED_BODY: &str = "Message deleted";

/// Apply a redaction: drop the named reaction, or tombstone the message.
pub(in crate::app) fn redact_in(rows: &mut [TimelineRow], redacted: &str) {
    for row in rows.iter_mut() {
        for group in row.reactions.iter_mut() {
            group.senders.retain(|s| s.event_id != redacted);
        }
        // Drop emptied groups.
        row.reactions.retain(|g| !g.senders.is_empty());
    }
    // Redacted messages keep their slot, like other clients' "message deleted".
    if let Some(row) = rows.iter_mut().find(|r| r.id == redacted) {
        row.body = DELETED_BODY.into();
        row.formatted = None;
        row.image = None;
        row.audio = None;
        row.reactions.clear();
    }
}

/// Outcome of folding a synced row into a timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum RowMerge {
    /// Genuinely new message.
    Appended,
    /// Confirmed one of our own local echoes.
    ReplacedEcho,
    /// Already present — an overlapping sync batch re-sent it.
    Deduped,
}

/// Fold a synced row in, replacing the local echo it confirms. Match by
/// `unsigned.transaction_id` first (bodies repeat, echo sender differs);
/// event id is the fallback against overlapping sync batches.
pub(in crate::app) fn merge_row(rows: &mut Vec<TimelineRow>, mut row: TimelineRow) -> RowMerge {
    /// Carry locally folded state (receipts, reactions, edits, reply context)
    /// onto a replacement row; a re-delivered message event carries none of it.
    fn carry_over(new: &mut TimelineRow, old: &TimelineRow) {
        if new.reply_to.is_none() {
            new.reply_to = old.reply_to.clone();
        }
        if new.reply_to_id.is_none() {
            new.reply_to_id = old.reply_to_id.clone();
        }
        if new.seen_by.is_empty() {
            new.seen_by = old.seen_by.clone();
        }
        merge_reactions(&mut new.reactions, &old.reactions);
        new.edited |= old.edited;
    }
    if let Some(txn) = row.txn_id.clone() {
        if let Some(echo) = rows.iter_mut().find(|r| r.txn_id.as_deref() == Some(&txn)) {
            carry_over(&mut row, echo);
            *echo = row;
            return RowMerge::ReplacedEcho;
        }
    }
    if let Some(existing) = rows.iter_mut().find(|r| r.id == row.id) {
        carry_over(&mut row, existing);
        *existing = row;
        return RowMerge::Deduped;
    }
    rows.push(row);
    RowMerge::Appended
}

/// Retain live edits, receipts and local echoes when the first history page arrives.
pub(in crate::app) fn merge_initial_history(
    target: &mut Vec<TimelineRow>,
    history: Vec<TimelineRow>,
) {
    let mut live: std::collections::VecDeque<_> = std::mem::take(target).into();
    for mut row in history {
        let overlap = live.iter().position(|existing| {
            existing.id == row.id || (row.txn_id.is_some() && existing.txn_id == row.txn_id)
        });
        if let Some(index) = overlap {
            // Live sync can contain messages older than this page in a busy room.
            target.extend(live.drain(..index));
            let existing = live.pop_front().unwrap();
            if existing.id == row.id {
                row = existing;
            } else {
                // A history event can confirm a pending local echo.
                let mut echo = vec![existing];
                merge_row(&mut echo, row);
                row = echo.pop().unwrap();
            }
        }
        target.push(row);
    }
    target.extend(live);
    if target.iter().any(|row| row.sender != "system") {
        target.retain(|row| row.id != "empty");
    }
}

/// Does this row mention us? Matches the full mxid and the localpart beside a pill.
pub(in crate::app) fn mentions_user(row: &TimelineRow, own: &str) -> bool {
    if own.is_empty() {
        return false;
    }
    let body = row.body.to_lowercase();
    if body.contains(&own.to_lowercase()) {
        return true;
    }
    let local = own.trim_start_matches('@').split(':').next().unwrap_or("");
    !local.is_empty() && body.contains(&local.to_lowercase())
}

impl ThraceApp {
    /// Local echo for our reaction; add-only, un-react redacts.
    pub(in crate::app) fn bump_reaction(row: &mut TimelineRow, key: String, own_user: &str) {
        if let Some(entry) = row.reactions.iter_mut().find(|r| r.key == key) {
            if !entry.senders.iter().any(|s| s.user == own_user) {
                // No event id yet; sync fills it on echo.
                entry.senders.push(Reactor {
                    user: own_user.to_owned(),
                    event_id: String::new(),
                });
            }
        } else {
            row.reactions.push(Reaction {
                key,
                senders: vec![Reactor {
                    user: own_user.to_owned(),
                    event_id: String::new(),
                }],
                bundled: 0,
            });
        }
    }
}
