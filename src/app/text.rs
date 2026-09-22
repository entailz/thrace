/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Formatting and parsing helpers shared across the app.

use crate::app::Member;
use crate::matrix::PackStore;

/// Extract the mxid from a user link: matrix.to pill (often percent-encoded)
/// or `matrix:u/…`. Room and alias links return `None`.
pub(in crate::app) fn mention_mxid(link: &str) -> Option<String> {
    let id = if let Some(frag) = link.split("matrix.to/#/").nth(1) {
        let raw = frag.split(['?', '/']).next().unwrap_or(frag);
        percent_decode(raw)
    } else if let Some(rest) = link.strip_prefix("matrix:u/") {
        let raw = rest.split(['?', '/']).next().unwrap_or(rest);
        format!("@{}", percent_decode(raw))
    } else {
        return None;
    };
    (id.starts_with('@') && id.contains(':')).then_some(id)
}

pub(in crate::app) fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let (Some(h), Some(l)) = (hex(raw[i + 1]), hex(raw[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Wall-clock now, in the same units the server stamps events with.
pub(in crate::app) fn now_millis() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Format `origin_server_ts` as a local stamp: `HH:MM` today, dated otherwise.
pub(in crate::app) fn format_ts(millis: Option<u64>) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(ms) = millis else {
        return String::new();
    };
    let Some(dt) = Local.timestamp_millis_opt(ms as i64).single() else {
        return String::new();
    };
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if dt.year() == now.year() {
        dt.format("%d %b %H:%M").to_string()
    } else {
        dt.format("%d %b %Y %H:%M").to_string()
    }
}

/// The `@word` at the end of `input`. Only the final word counts, so a
/// mid-sentence completion cannot move the cursor unexpectedly.
pub(in crate::app) fn mention_prefix(input: &str) -> Option<&str> {
    let word = input.rsplit(' ').next()?;
    if !word.starts_with('@') || word.len() > 64 {
        return None;
    }
    // A finished mxid is not partial; stop offering completions.
    if word.contains(':') {
        return None;
    }
    Some(word)
}

/// Length of the `:shortcode:` at the start of `s`, if it is well-formed.
/// Bounded so a stray colon in prose (or an mxid) is plain text, not emoji.
pub(in crate::app) fn shortcode_end(s: &str) -> Option<usize> {
    if !s.starts_with(':') {
        return None;
    }
    let end = s[1..].find(':')? + 1;
    // `:a:` .. `:32-char-shortcode:`; inner text has no spaces or markup.
    if !(2..=34).contains(&end) {
        return None;
    }
    let inner = &s[1..end];
    if inner
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '+')
    {
        Some(end + 1)
    } else {
        None
    }
}

/// Free core of `ThraceApp::mentioned_ids`, so tests need no app instance.
pub(in crate::app) fn mentioned_ids_in(
    members: &[Member],
    text: &str,
) -> Vec<matrix_sdk::ruma::OwnedUserId> {
    let mut ids = Vec::new();
    for m in members {
        let named = text.contains(m.mxid.as_str()) || text.contains(&format!("@{}", m.display));
        if !named {
            continue;
        }
        if let Ok(id) = matrix_sdk::ruma::OwnedUserId::try_from(m.mxid.clone()) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Free core of `ThraceApp::rich_bodies`, so tests need no app instance.
pub(in crate::app) fn rich_bodies_in(
    members: &[Member],
    packs: &PackStore,
    raw: &str,
) -> (String, Option<String>) {
    let mut html = String::new();
    let mut rich = false;
    let mut rest = raw;
    while !rest.is_empty() {
        // Longest member mxid first, so overlapping ids can't half-match.
        if let Some(m) = members
            .iter()
            .filter(|m| rest.starts_with(m.mxid.as_str()))
            .max_by_key(|m| m.mxid.len())
        {
            rich = true;
            html.push_str(&format!(
                "<a href=\"https://matrix.to/#/{}\">{}</a>",
                m.mxid,
                html_escape(&m.display)
            ));
            rest = &rest[m.mxid.len()..];
            continue;
        }
        if let Some(end) = shortcode_end(rest) {
            let sc = &rest[..end];
            if let Some(img) = packs.resolve(sc) {
                rich = true;
                let key = sc.trim_matches(':');
                html.push_str(&format!(
                    "<img data-mx-emoticon src=\"{}\" alt=\"{key}\" title=\"{key}\" height=\"32\" />",
                    img.mxc_url
                ));
            } else {
                html.push_str(&html_escape(sc));
            }
            rest = &rest[end..];
            continue;
        }
        let ch = rest.chars().next().expect("non-empty");
        // Escape the char; per-char `replace` would re-scan.
        match ch {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            _ => html.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }
    if rich {
        (raw.to_owned(), Some(html))
    } else {
        (raw.to_owned(), None)
    }
}

/// Escape text for inclusion in `formatted_body`.
pub(in crate::app) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Make a filename safe for the temp directory. Bodies can carry slashes,
/// `..`, anything the sender chose; none of it may steer the path.
pub(in crate::app) fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let trimmed = cleaned.trim_matches('.').to_owned();
    if trimmed.is_empty() {
        "video".to_owned()
    } else {
        trimmed
    }
}

pub(in crate::app) fn snippet(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Char-boundary-safe truncation with `…`. `snippet` slices raw bytes; names
/// are arbitrary unicode, so count chars.
pub(in crate::app) fn truncate_name(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

pub(in crate::app) fn short_room(room_id: &str) -> String {
    if room_id.starts_with('!') {
        // No name yet; show a compact id prefix until sync fills names.
        let end = room_id.find(':').unwrap_or(room_id.len().min(12));
        room_id[..end.min(room_id.len())].to_owned()
    } else {
        room_id.to_owned()
    }
}

pub(in crate::app) fn localpart(sender: &str) -> String {
    sender
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(sender)
        .to_owned()
}
