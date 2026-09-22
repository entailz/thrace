/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

use crate::app::decode::{avatar_for_sender, decode_timeline_json, display_name_for_sender};
use crate::app::text::{format_ts, localpart};
use crate::app::{PinnedMessage, ThraceApp};

impl ThraceApp {
    pub(in crate::app) fn open_pins(&mut self) {
        let (Some(client), Some(room_id)) = (self.client.clone(), self.current_room_id()) else {
            return;
        };
        if self.pinned_room.as_deref() == Some(room_id.as_str()) {
            self.pinned_room = None;
            return;
        }
        self.pinned_room = Some(room_id.clone());
        self.pinned_messages.clear();
        self.pinned_error = None;
        self.pinned_loading = true;
        let (tx, rx) = std::sync::mpsc::channel();
        self.pinned_rx = Some(rx);
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let result = load_pins(&client, &room_id).await;
            let _ = tx.send((room_id, result));
            ctx.request_repaint();
        });
    }
}

async fn load_pins(
    client: &matrix_sdk::Client,
    room_id: &str,
) -> Result<Vec<PinnedMessage>, String> {
    let id = matrix_sdk::ruma::OwnedRoomId::try_from(room_id).map_err(|e| e.to_string())?;
    let room = client.get_room(&id).ok_or("room not found")?;
    let ids = match room.load_pinned_events().await {
        Ok(ids) => ids.or_else(|| room.pinned_event_ids()).unwrap_or_default(),
        Err(error) => room
            .pinned_event_ids()
            .ok_or_else(|| format!("pinned messages: {error}"))?,
    };
    let mut pins = Vec::with_capacity(ids.len());
    for id in ids {
        let event = room.load_or_fetch_event(&id, None).await;
        let pin = match event {
            Ok(event) => {
                let json = serde_json::to_value(event.raw()).unwrap_or_default();
                let mut pin = parse_pinned_message(id.to_string(), &json);
                let sender = json.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                pin.display_name = display_name_for_sender(&room, sender)
                    .await
                    .unwrap_or_else(|| pin.sender.clone());
                pin.avatar_mxc = avatar_for_sender(&room, sender).await;
                let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if event_type == "m.room.message" || event_type == "m.sticker" {
                    pin.timeline_row = Some(
                        decode_timeline_json(
                            &room,
                            sender,
                            &pin.display_name,
                            pin.id.clone(),
                            event_type,
                            &json["content"],
                            &pin.body,
                            Some(pin.origin_server_ts),
                            pin.avatar_mxc.clone(),
                        )
                        .await,
                    );
                }
                pin
            }
            Err(_) => PinnedMessage {
                id: id.to_string(),
                sender: String::new(),
                display_name: String::new(),
                avatar_mxc: None,
                body: "Message unavailable".into(),
                ts: String::new(),
                origin_server_ts: 0,
                timeline_row: None,
            },
        };
        pins.push(pin);
    }
    pins.sort_by(|a, b| b.origin_server_ts.cmp(&a.origin_server_ts));
    Ok(pins)
}

fn parse_pinned_message(id: String, json: &serde_json::Value) -> PinnedMessage {
    let sender = json.get("sender").and_then(|v| v.as_str()).unwrap_or("?");
    let content = &json["content"];
    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let body = content
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| match event_type {
            "m.room.encrypted" => "Encrypted message (unable to decrypt)".into(),
            "m.room.redaction" => "Message was removed".into(),
            "m.sticker" => "Sticker".into(),
            "m.room.message" => content
                .get("msgtype")
                .and_then(|v| v.as_str())
                .unwrap_or("Message")
                .trim_start_matches("m.")
                .to_owned(),
            "m.room.topic" => content
                .get("topic")
                .and_then(|v| v.as_str())
                .map(|s| format!("Topic: {s}"))
                .unwrap_or_else(|| "Topic changed".into()),
            "m.room.name" => content
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| format!("Room name: {s}"))
                .unwrap_or_else(|| "Room name changed".into()),
            "m.room.member" => content
                .get("membership")
                .and_then(|v| v.as_str())
                .map(|s| format!("Membership: {s}"))
                .unwrap_or_else(|| "Membership changed".into()),
            "" if json
                .get("unsigned")
                .and_then(|v| v.get("redacted_because"))
                .is_some() =>
            {
                "Message was removed".into()
            }
            "" => "Event content unavailable".into(),
            other => other.trim_start_matches("m.room.").replace('.', " "),
        });
    let origin_server_ts = json
        .get("origin_server_ts")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    PinnedMessage {
        id,
        sender: localpart(sender),
        display_name: localpart(sender),
        avatar_mxc: None,
        body,
        ts: format_ts(Some(origin_server_ts)),
        origin_server_ts,
        timeline_row: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pinned_message_content() {
        let json = serde_json::json!({
            "sender": "@alice:example.org",
            "content": {"body": "Pinned text"}
        });
        let pin = parse_pinned_message("$event:example.org".into(), &json);
        assert_eq!(pin.id, "$event:example.org");
        assert_eq!(pin.sender, "alice");
        assert_eq!(pin.body, "Pinned text");
    }

    #[test]
    fn describes_pinned_events_without_a_body() {
        let topic = serde_json::json!({
            "type": "m.room.topic", "sender": "@alice:example.org",
            "content": {"topic": "Release notes"}
        });
        assert_eq!(
            parse_pinned_message("$topic".into(), &topic).body,
            "Topic: Release notes"
        );
        let encrypted = serde_json::json!({"type": "m.room.encrypted", "content": {}});
        assert_eq!(
            parse_pinned_message("$secret".into(), &encrypted).body,
            "Encrypted message (unable to decrypt)"
        );
    }
}
