/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

use super::*;
use crate::app::decode::{
    decode_audio, empty_row, media_source_mxc, plain_media_source, timeline_still_source,
};
use crate::app::rows::{
    bundled_annotations, edit_in, edit_replacement, fold_reaction, mentions_user,
    merge_initial_history, merge_reactions, merge_row, parse_relation, receipt_target, redact_in,
    seen_in, split_reply_fallback, RowMerge, DELETED_BODY,
};
use crate::app::session::extract_login_token;
use crate::app::sync::{sync_filter, SYNC_TIMELINE_LIMIT};
use crate::app::text::{
    continues_group, format_ts, format_ts_clock, local_day, localpart, mention_mxid,
    mention_prefix, mentioned_ids_in, now_millis, rich_bodies_in, sanitise, short_room,
    shortcode_end, truncate_name,
};

fn row(id: &str, sender: &str, body: &str, txn: Option<&str>) -> TimelineRow {
    TimelineRow {
        id: id.into(),
        ts: format_ts(now_millis()),
        origin_server_ts: 0,
        sender: sender.into(),
        display_name: sender.trim_start_matches('@').into(),
        body: body.into(),
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
        txn_id: txn.map(str::to_owned),
    }
}

fn attachment(mxc: &str, is_video: bool, thumb: Option<&str>) -> ImageAttachment {
    ImageAttachment {
        mxc: mxc.into(),
        source: plain_media_source(mxc),
        thumbnail_source: thumb.map(plain_media_source),
        name: "clip.mp4".into(),
        w: 1920,
        h: 1080,
        is_video,
        duration_ms: Some(9_000),
    }
}

#[test]
fn video_without_a_thumbnail_asks_for_no_still() {
    // Asking downloads the whole clip for the image decoder to reject it.
    assert!(timeline_still_source(&attachment("mxc://hs/clip", true, None)).is_none());
}

#[test]
fn video_with_a_thumbnail_decodes_the_thumbnail() {
    let clip = attachment("mxc://hs/clip", true, Some("mxc://hs/poster"));
    let source = timeline_still_source(&clip).expect("a thumbnail is an image");
    assert_eq!(media_source_mxc(&source), "mxc://hs/poster");
}

#[test]
fn image_without_a_thumbnail_falls_back_to_the_file() {
    let pic = attachment("mxc://hs/pic", false, None);
    let source = timeline_still_source(&pic).expect("an image decodes from its file");
    assert_eq!(media_source_mxc(&source), "mxc://hs/pic");
}

#[test]
fn reply_fallback_is_parsed_and_stripped() {
    let body = "> <@alice:hs> the original message\n> second line\n\nmy reply";
    let (preview, stripped) = split_reply_fallback(body);
    let (who, quoted) = preview.expect("fallback carries a preview");
    assert_eq!(who, "alice");
    assert_eq!(quoted, "the original message second line");
    assert_eq!(stripped, "my reply");
}

#[test]
fn reply_body_without_fallback_is_untouched() {
    let (preview, stripped) = split_reply_fallback("just the reply");
    assert!(preview.is_none());
    assert_eq!(stripped, "just the reply");
}

#[test]
fn quote_without_user_form_still_strips() {
    let (preview, stripped) = split_reply_fallback("> plain quoted text\n\nreply");
    let (who, quoted) = preview.expect("still a fallback");
    assert_eq!(who, "?");
    assert_eq!(quoted, "plain quoted text");
    assert_eq!(stripped, "reply");
}

#[test]
fn echo_does_not_erase_reply_context() {
    // Reported bug: server echo without reply context blanked the local marker.
    let mut local = row("local-1", "@me:hs", "my reply", Some("t1"));
    local.reply_to = Some(("alice".into(), "original".into()));
    local.reply_to_id = Some("$orig".into());
    let mut rows = vec![local];

    let server = row("$real", "@me:hs", "my reply", Some("t1"));
    assert!(server.reply_to_id.is_none());
    assert_eq!(merge_row(&mut rows, server), RowMerge::ReplacedEcho);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "$real");
    assert_eq!(
        rows[0].reply_to_id.as_deref(),
        Some("$orig"),
        "reply target must survive the echo"
    );
    assert!(rows[0].reply_to.is_some(), "reply preview must survive too");
}

#[test]
fn initial_history_preserves_live_order_and_edits() {
    let mut edited = row("$b", "@b:hs", "edited body", None);
    edited.edited = true;
    edited.seen_by = vec!["@reader:hs".into()];
    let mut live = vec![
        row("$a", "@b:hs", "older live message", None),
        edited,
        row("$d", "@b:hs", "latest live message", None),
    ];
    let history = vec![
        row("$b", "@b:hs", "original body", None),
        row("$c", "@b:hs", "message from history", None),
        row("$d", "@b:hs", "latest live message", None),
    ];
    merge_initial_history(&mut live, history);
    assert_eq!(
        live.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["$a", "$b", "$c", "$d"]
    );
    assert_eq!(live[1].body, "edited body");
    assert!(live[1].edited);
    assert_eq!(live[1].seen_by, ["@reader:hs"]);
}

#[test]
fn initial_history_keeps_new_messages_and_reconciles_local_echoes() {
    let mut live = vec![
        row("local-1", "@me:hs", "hello", Some("txn")),
        row("$new", "@b:hs", "new", None),
    ];
    merge_initial_history(
        &mut live,
        vec![row("$confirmed", "@me:hs", "hello", Some("txn"))],
    );
    assert_eq!(
        live.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["$confirmed", "$new"]
    );
    merge_initial_history(&mut live, vec![empty_row("No messages yet")]);
    assert_eq!(live.len(), 2);
    assert!(live.iter().all(|row| row.id != "empty"));
}

#[test]
fn matrix_to_links_resolve_to_an_mxid() {
    assert_eq!(
        mention_mxid("https://matrix.to/#/@alice:example.org").as_deref(),
        Some("@alice:example.org")
    );
    assert_eq!(
        mention_mxid("https://matrix.to/#/%40alice%3Aexample.org").as_deref(),
        Some("@alice:example.org")
    );
    // MSC2312 URI scheme.
    assert_eq!(
        mention_mxid("matrix:u/alice:example.org").as_deref(),
        Some("@alice:example.org")
    );
    // A trailing via/query stays out of the id.
    assert_eq!(
        mention_mxid("https://matrix.to/#/@alice:example.org?via=hs").as_deref(),
        Some("@alice:example.org")
    );
}

#[test]
fn non_user_links_are_not_mentions() {
    assert!(mention_mxid("https://matrix.to/#/!room:example.org").is_none());
    assert!(mention_mxid("https://matrix.to/#/%23alias%3Aexample.org").is_none());
    assert!(mention_mxid("https://example.com/@notanmxid").is_none());
}

#[test]
fn shared_rooms_counts_the_current_room_once() {
    let member = |mxid: &str| Member {
        display: mxid.trim_start_matches('@').into(),
        mxid: mxid.into(),
        power: 0,
        presence: None,
        avatar_mxc: None,
    };
    let mut by_room = std::collections::HashMap::new();
    by_room.insert("!other:hs".to_owned(), vec![member("@bob:hs")]);
    by_room.insert("!third:hs".to_owned(), vec![member("@carol:hs")]);
    // The room on screen is NOT in `by_room` — its members live apart.
    let here = [member("@bob:hs")];

    assert_eq!(
        count_shared_rooms(&here, &by_room, "@bob:hs"),
        2,
        "current room plus one other, counted once each"
    );
    assert_eq!(count_shared_rooms(&here, &by_room, "@carol:hs"), 1);
    assert_eq!(count_shared_rooms(&here, &by_room, "@dave:hs"), 0);
}

#[test]
fn timestamps_format_as_local_wall_clock() {
    // Regression: messages once rendered the literal string "hist".
    assert_eq!(format_ts(None), "");
    let stamped = format_ts(now_millis());
    assert!(
        stamped.len() == 5 && stamped.as_bytes()[2] == b':',
        "today should render as HH:MM, got {stamped:?}"
    );
    // Old events stay dated, not misread as today.
    let old = format_ts(Some(1_000_000_000_000));
    assert!(old.contains("2001"), "expected a dated stamp, got {old:?}");
    assert_ne!(old, stamped);
}

#[test]
fn edit_rewrites_the_row_in_place() {
    // Must replace in place, not append beside the original.
    let mut rows = [
        row("$a", "@me:hs", "teh original", None),
        row("$b", "@other:hs", "unrelated", None),
    ];
    assert!(edit_in(&mut rows, "$a", "the original, fixed".into(), None));
    assert_eq!(rows.len(), 2, "no extra row may appear");
    assert_eq!(rows[0].body, "the original, fixed");
    assert!(rows[0].edited, "edited messages are marked");
    assert!(!rows[1].edited, "other messages are untouched");
}

#[test]
fn edit_for_an_unloaded_message_is_dropped() {
    let mut rows = [row("$a", "@me:hs", "hi", None)];
    assert!(!edit_in(&mut rows, "$missing", "x".into(), None));
    assert_eq!(rows[0].body, "hi");
}

#[test]
fn decode_audio_reads_msc1767_voice_notes() {
    let content = serde_json::json!({
        "body": "Voice message",
        "msgtype": "m.audio",
        "url": "mxc://hs/aaa",
        "info": {"mimetype": "audio/ogg", "size": 1234},
        "org.matrix.msc1767.audio": {"duration": 1500, "size": 1234, "waveform": [0, 512, 1024, 2048]},
        "org.matrix.msc3245.voice": {},
    });
    let audio = decode_audio(&content, "Voice message").expect("audio must decode");
    assert_eq!(audio.mxc, "mxc://hs/aaa");
    assert_eq!(audio.mime, "audio/ogg");
    assert_eq!(audio.duration_ms, Some(1500));
    assert_eq!(audio.waveform, vec![0.0, 0.5, 1.0, 1.0]);
    assert!(audio.is_voice);
}

#[test]
fn decode_audio_accepts_plain_music_without_msc_keys() {
    let content = serde_json::json!({
        "body": "song.mp3",
        "msgtype": "m.audio",
        "url": "mxc://hs/bbb",
        "info": {"mimetype": "audio/mpeg", "duration": 180000},
    });
    let audio = decode_audio(&content, "song.mp3").expect("audio must decode");
    assert_eq!(audio.duration_ms, Some(180000));
    assert!(audio.waveform.is_empty());
    assert!(!audio.is_voice);
}

#[test]
fn localpart_strips_sigil_and_server() {
    assert_eq!(localpart("@alice:hs"), "alice");
    assert_eq!(localpart("@bob:matrix.org"), "bob");
    assert_eq!(localpart("?"), "?");
}

#[test]
fn history_page_folds_edits_instead_of_appending() {
    let content = serde_json::json!({
        "body": " * fixed",
        "msgtype": "m.text",
        "m.new_content": {"body": "fixed", "msgtype": "m.text"},
        "m.relates_to": {"rel_type": "m.replace", "event_id": "$a"},
    });
    let (target, body, _) = edit_replacement(&content).expect("edit must parse");
    assert_eq!(target, "$a");
    let mut page = vec![row("$a", "@me:hs", "teh original", None)];
    assert!(edit_in(&mut page, &target, body, None));
    assert_eq!(page.len(), 1, "edit must not append a row");
    assert_eq!(page[0].body, "fixed");
    assert!(page[0].edited);
}

#[test]
fn edit_replacement_ignores_non_edits() {
    for content in [
        serde_json::json!({"body": "hi"}),
        serde_json::json!({"body": "hi", "m.relates_to": {"m.in_reply_to": {"event_id": "$a"}}}),
        serde_json::json!({"body": "hi", "m.relates_to": {"rel_type": "m.thread", "event_id": "$a"}}),
    ] {
        assert!(edit_replacement(&content).is_none());
    }
    let missing_target = serde_json::json!({
        "body": " * x",
        "m.relates_to": {"rel_type": "m.replace"},
    });
    let (target, _, _) = edit_replacement(&missing_target).expect("still an edit shape");
    assert!(target.is_empty(), "callers skip target-less edits");
}

#[test]
fn mention_prefix_only_completes_the_word_being_typed() {
    assert_eq!(mention_prefix("hey @al"), Some("@al"));
    assert_eq!(mention_prefix("@al"), Some("@al"));
    assert_eq!(mention_prefix("first line\n@al"), Some("@al"));
    assert_eq!(mention_prefix("@al\n"), None);
    // Not a mention at all.
    assert_eq!(mention_prefix("hello there"), None);
    // Only the final word is a candidate; completing must not move the cursor.
    assert_eq!(mention_prefix("@bob said hi"), None);
    // Full mxid: nothing left to complete.
    assert_eq!(mention_prefix("@bob:matrix.org"), None);
}

fn mention_member(mxid: &str, display: &str) -> Member {
    Member {
        display: display.into(),
        mxid: mxid.into(),
        power: 0,
        presence: None,
        avatar_mxc: None,
    }
}

fn emoji_packs() -> PackStore {
    let mut packs = PackStore::new();
    packs.upsert_pack(crate::matrix::ImagePack {
        address: None,
        display_name: "test".into(),
        images: vec![crate::matrix::PackImage {
            shortcode: "dance".into(),
            mxc_url: "mxc://hs/aaa".into(),
            is_emoji: true,
            is_sticker: false,
        }],
    });
    packs
}

#[test]
fn mention_bodies_link_the_mxid_not_the_nickname() {
    let members = [mention_member("@alice:hs", "Alice Cooper")];
    let (plain, html) = rich_bodies_in(&members, &PackStore::new(), "hi @alice:hs yo");
    assert_eq!(plain, "hi @alice:hs yo");
    let html = html.expect("a mention is rich text");
    assert!(
        html.contains("<a href=\"https://matrix.to/#/@alice:hs\">Alice Cooper</a>"),
        "pill links the mxid, labels the display name: {html}"
    );
    // The notifier sees the real user id, not a nickname.
    let ids = mentioned_ids_in(&members, "hi @alice:hs yo");
    assert_eq!(ids, ["@alice:hs"]);
}

#[test]
fn mention_and_emoji_share_one_message() {
    // The mxid's colon must not eat the `:shortcode:` after it.
    let members = [mention_member("@alice:hs", "Alice")];
    let (plain, html) = rich_bodies_in(&members, &emoji_packs(), "hi @alice:hs :dance:");
    assert_eq!(plain, "hi @alice:hs :dance:");
    let html = html.expect("mention + emoji is rich text");
    assert!(html.contains("matrix.to/#/@alice:hs"), "pill kept: {html}");
    assert!(html.contains("data-mx-emoticon"), "emoji kept: {html}");
    assert!(html.contains("mxc://hs/aaa"), "emoji image kept: {html}");
}

#[test]
fn plain_text_stays_plain() {
    let members = [mention_member("@alice:hs", "Alice")];
    let (plain, html) = rich_bodies_in(&members, &emoji_packs(), "just words");
    assert_eq!(plain, "just words");
    assert!(html.is_none());
    assert!(mentioned_ids_in(&members, "just words").is_empty());
}

#[test]
fn shortcode_scan_ignores_stray_colons() {
    assert_eq!(shortcode_end(":dance:"), Some(7));
    assert_eq!(shortcode_end(":hs rest"), None);
    assert_eq!(shortcode_end("dance:"), None);
    assert_eq!(shortcode_end(":has space:"), None);
}

#[test]
fn every_settings_tab_is_labelled_and_iconed() {
    // Every tab needs a label and an icon.
    for tab in SettingsTab::ALL {
        assert!(!tab.label().is_empty());
        assert!(!tab.icon().is_empty(), "{} has no icon", tab.label());
    }
    // Bump on new tabs: the only catch for a variant missing from ALL.
    assert_eq!(
        SettingsTab::ALL.len(),
        7,
        "ALL must list every tab, or some become unreachable"
    );
    // No duplicates shadowing a section.
    let mut labels: Vec<&str> = SettingsTab::ALL.iter().map(|t| t.label()).collect();
    labels.sort_unstable();
    let before = labels.len();
    labels.dedup();
    assert_eq!(before, labels.len(), "duplicate tab in ALL");
}

#[test]
fn notification_modes_map_to_the_sdk() {
    use matrix_sdk::notification_settings::RoomNotificationMode as M;
    assert_eq!(RoomNotify::All.sdk(), M::AllMessages);
    assert_eq!(RoomNotify::MentionsOnly.sdk(), M::MentionsAndKeywordsOnly);
    assert_eq!(RoomNotify::Mute.sdk(), M::Mute);
    for mode in [RoomNotify::All, RoomNotify::MentionsOnly, RoomNotify::Mute] {
        assert!(!mode.label().is_empty());
    }
}

#[test]
fn jump_button_appears_only_well_above_the_newest_message() {
    // Rows, not pixels: images and one-liners differ in height.
    let hidden_rows = |content_h: f32, offset: f32, view_h: f32, rows: usize| {
        let below = (content_h - offset - view_h).max(0.0);
        let row_h = if rows == 0 {
            0.0
        } else {
            content_h / rows as f32
        };
        if row_h > 0.0 {
            below / row_h
        } else {
            0.0
        }
    };

    assert!(hidden_rows(2000.0, 1400.0, 600.0, 100) < 30.0);
    assert!(hidden_rows(2000.0, 0.0, 600.0, 100) >= 30.0);
    assert_eq!(hidden_rows(400.0, 0.0, 600.0, 10), 0.0);
    assert_eq!(hidden_rows(0.0, 0.0, 600.0, 0), 0.0);
}

#[test]
fn a_page_of_scrollback_drops_what_is_already_shown() {
    // Pages overlap live sync; drop seam duplicates.
    let existing = [
        row("$c", "@a:hs", "third", None),
        row("$d", "@a:hs", "fourth", None),
    ];
    let page = [
        row("$a", "@a:hs", "first", None),
        row("$b", "@a:hs", "second", None),
        row("$c", "@a:hs", "third", None),
    ];
    let known: std::collections::HashSet<&str> = existing.iter().map(|r| r.id.as_str()).collect();
    let fresh: Vec<&TimelineRow> = page
        .iter()
        .filter(|r| !known.contains(r.id.as_str()))
        .collect();
    assert_eq!(fresh.len(), 2, "the overlapping row must be dropped");
    assert_eq!(fresh[0].id, "$a");
    assert_eq!(fresh[1].id, "$b");
}

#[test]
fn video_filenames_cannot_escape_the_temp_dir() {
    // Sender-controlled names must not steer the path.
    assert_eq!(sanitise("clip.mp4"), "clip.mp4");
    // What matters is nothing redirecting the write, not the exact substitution.
    for hostile in ["../../etc/passwd", "/absolute/path", "a\\b", "..", ""] {
        let safe = sanitise(hostile);
        assert!(!safe.contains('/'), "{hostile:?} -> {safe:?} kept a slash");
        assert!(
            !safe.contains('\\'),
            "{hostile:?} -> {safe:?} kept a backslash"
        );
        assert!(
            !safe.starts_with('.'),
            "{hostile:?} -> {safe:?} starts with a dot"
        );
        assert!(!safe.is_empty(), "{hostile:?} produced an empty name");
    }
    assert_eq!(sanitise(""), "video");
    assert!(sanitise(&"x".repeat(500)).len() <= 64);
}

#[test]
fn slash_completion_keeps_arguments() {
    let mut app_input = "/kic @bob:hs being rude".to_owned();
    let args = app_input
        .split_once(' ')
        .map(|(_, rest)| rest.to_owned())
        .unwrap_or_default();
    app_input = format!("/kick {args}");
    assert_eq!(app_input, "/kick @bob:hs being rude");
}

#[test]
fn slash_prefix_matches_the_expected_commands() {
    let verb = "k";
    let hits: Vec<&str> = SLASH_COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd[1..].starts_with(verb))
        .map(|(cmd, _)| *cmd)
        .collect();
    assert!(hits.contains(&"/kick"), "expected /kick, got {hits:?}");
    assert!(!hits.contains(&"/join"));

    // An exact verb matches itself, so Enter runs instead of completing.
    let exact: Vec<&str> = SLASH_COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd[1..] == *"ban")
        .map(|(cmd, _)| *cmd)
        .collect();
    assert_eq!(exact, vec!["/ban"]);
}

#[test]
fn every_listed_slash_command_is_dispatched() {
    // Listed commands must all dispatch; no fake entries.
    let dispatched = [
        "me", "react", "reply", "sticker", "shrug", "join", "invite", "topic", "nick", "leave",
        "part", "roomname", "plain", "spoiler", "kick", "ban", "unban", "op", "deop", "ignore",
        "unignore", "help",
    ];
    for (cmd, _) in SLASH_COMMANDS {
        let verb = cmd.trim_start_matches('/');
        assert!(
            dispatched.contains(&verb),
            "/{verb} is listed in the palette but never dispatched"
        );
    }
}

#[test]
fn redelivery_keeps_receipts_and_reactions() {
    // Overlapping re-delivery must not wipe receipts or reactions.
    let mut rows = vec![row("$a", "@b:hs", "hi", None)];
    rows[0].seen_by = vec!["@reader:hs".into()];
    rows[0].reactions = vec![Reaction {
        key: "\u{1F44D}".into(),
        senders: vec![Reactor {
            user: "@c:hs".into(),
            event_id: "$r".into(),
        }],
        bundled: 0,
    }];
    rows[0].edited = true;

    assert_eq!(
        merge_row(&mut rows, row("$a", "@b:hs", "hi", None)),
        RowMerge::Deduped
    );
    assert_eq!(rows[0].seen_by, vec!["@reader:hs".to_owned()]);
    assert_eq!(rows[0].reactions.len(), 1, "reactions must survive too");
    assert!(rows[0].edited, "edited flag must survive");
}

#[test]
fn echo_confirmation_keeps_receipts() {
    // Same hazard on the txn-id confirmation path.
    let mut local = row("local-1", "@me:hs", "hi", Some("t1"));
    local.seen_by = vec!["@reader:hs".into()];
    let mut rows = vec![local];
    merge_row(&mut rows, row("$real", "@me:hs", "hi", Some("t1")));
    assert_eq!(rows[0].id, "$real");
    assert_eq!(rows[0].seen_by, vec!["@reader:hs".to_owned()]);
}

#[test]
fn receipt_for_an_unloaded_event_leaves_the_old_one_alone() {
    // Unknown target: leave existing receipts alone.
    let mut rows = [row("$a", "@b:hs", "hi", None)];
    rows[0].seen_by = vec!["@reader:hs".into()];

    assert!(!seen_in(&mut rows, "$not-loaded", "@reader:hs", 0));
    assert_eq!(
        rows[0].seen_by,
        vec!["@reader:hs".to_owned()],
        "receipt must stay put when the target is unknown"
    );
}

#[test]
fn a_receipt_moves_forward_to_the_newer_message() {
    let mut rows = [
        row("$a", "@b:hs", "first", None),
        row("$b", "@b:hs", "second", None),
    ];
    assert!(seen_in(&mut rows, "$a", "@reader:hs", 0));
    assert_eq!(rows[0].seen_by.len(), 1);

    assert!(seen_in(&mut rows, "$b", "@reader:hs", 0));
    assert!(rows[0].seen_by.is_empty(), "must leave the older message");
    assert_eq!(rows[1].seen_by, vec!["@reader:hs".to_owned()]);
}

#[test]
fn sync_filter_requests_lazy_loaded_members() {
    // Assert the wire form, not the builder.
    use matrix_sdk::ruma::api::client::sync::sync_events;
    let sync_events::v3::Filter::FilterDefinition(filter) = sync_filter() else {
        panic!("expected an inline filter definition");
    };
    let json = serde_json::to_value(&filter).expect("filter serialises");
    let room = &json["room"];
    assert_eq!(
        room["state"]["lazy_load_members"], true,
        "state must lazy-load members, or the initial sync ships every \
         m.room.member event for every room"
    );
    assert_eq!(room["timeline"]["lazy_load_members"], true);
    assert_eq!(
        room["timeline"]["limit"], SYNC_TIMELINE_LIMIT,
        "timeline must be bounded"
    );
}

#[test]
fn a_reply_points_at_a_loadable_target() {
    // Jump-to-original resolves by id; it must survive decoding.
    let mut rows = [
        row("$orig", "@a:hs", "the original", None),
        row("$reply", "@b:hs", "my reply", None),
    ];
    rows[1].reply_to_id = Some("$orig".into());

    let target = rows[1].reply_to_id.clone().unwrap();
    assert!(
        rows.iter().any(|r| r.id == target),
        "reply target must resolve against loaded rows"
    );

    // An unpaged target must not resolve.
    rows[1].reply_to_id = Some("$missing".into());
    let target = rows[1].reply_to_id.clone().unwrap();
    assert!(!rows.iter().any(|r| r.id == target));
}

#[test]
fn merge_replaces_local_echo_by_txn_id() {
    let mut rows = vec![row("local-1", "@me:hs", "hello", Some("abc"))];
    let merged = merge_row(&mut rows, row("$real", "@me:hs", "hello", Some("abc")));
    assert_eq!(merged, RowMerge::ReplacedEcho);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "$real");
}

#[test]
fn merge_dedupes_overlapping_sync_batches() {
    let mut rows = vec![row("$a", "@b:hs", "hi", None)];
    assert_eq!(
        merge_row(&mut rows, row("$a", "@b:hs", "hi", None)),
        RowMerge::Deduped
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn merge_appends_genuinely_new_rows() {
    let mut rows = vec![row("$a", "@b:hs", "hi", None)];
    assert_eq!(
        merge_row(&mut rows, row("$b", "@c:hs", "yo", None)),
        RowMerge::Appended
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn merge_matches_echo_even_when_sender_string_differs() {
    // Echo sender string differs from server mxid; the txn id is the key.
    let mut rows = vec![row("local-2", "you", "same text", Some("t9"))];
    let merged = merge_row(&mut rows, row("$srv", "@me:hs", "same text", Some("t9")));
    assert_eq!(merged, RowMerge::ReplacedEcho);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sender, "@me:hs");
}

#[test]
fn identical_bodies_from_others_are_not_echoes() {
    let mut rows = vec![row("$a", "@b:hs", "lol", None)];
    assert_eq!(
        merge_row(&mut rows, row("$b", "@c:hs", "lol", None)),
        RowMerge::Appended
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn mention_detection_matches_mxid_and_localpart() {
    assert!(mentions_user(
        &row("$a", "@b:hs", "hey @me:hs look", None),
        "@me:hs"
    ));
    assert!(mentions_user(
        &row("$a", "@b:hs", "Me, are you there?", None),
        "@me:hs"
    ));
    assert!(!mentions_user(
        &row("$a", "@b:hs", "nothing here", None),
        "@me:hs"
    ));
    assert!(!mentions_user(&row("$a", "@b:hs", "anything", None), ""));
}

#[test]
fn sync_batch_is_empty_only_when_nothing_landed() {
    let mut batch = SyncBatch::default();
    assert!(
        batch.is_empty(),
        "a keepalive long-poll must not wake the UI"
    );
    batch
        .receipts
        .push(("!r:hs".into(), "$e".into(), "@u:hs".into(), 0));
    assert!(!batch.is_empty(), "receipts still need applying");
}

#[test]
fn extracts_token_from_url() {
    assert_eq!(extract_login_token("abc123"), "abc123");
    assert_eq!(
        extract_login_token("http://localhost:8008/callback?loginToken=XYZ&foo=1"),
        "XYZ"
    );
}

#[test]
fn short_room_compact() {
    assert!(short_room("!abc123:matrix.org").starts_with("!abc"));
}

#[test]
fn truncate_name_is_char_safe() {
    // Chars, not bytes: multi-byte names must not panic.
    assert_eq!(truncate_name("alice", 16), "alice");
    assert_eq!(
        truncate_name("averyverylongdisplayname", 16),
        "averyverylongdis…"
    );
    assert_eq!(truncate_name("🦀🦀🦀🦀🦀", 3), "🦀🦀🦀…");
}

#[test]
fn bundled_annotations_seed_counts_without_senders() {
    let event = serde_json::json!({
        "type": "m.room.message",
        "content": {"body": "hi", "msgtype": "m.text"},
        "unsigned": {"m.relations": {"m.annotation": {"chunk": [
            {"type": "m.reaction", "key": "👍", "count": 3},
            {"type": "m.reaction", "key": "", "count": 1},
            {"type": "m.reaction", "key": "🎉", "count": 0},
        ]}}},
    });
    let groups = bundled_annotations(&event);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key, "👍");
    assert_eq!(groups[0].count(), 3);
    assert!(bundled_annotations(&serde_json::json!({})).is_empty());
}

#[test]
fn live_senders_merge_into_bundled_counts() {
    let mut reactions = bundled_annotations(&serde_json::json!({
        "unsigned": {"m.relations": {"m.annotation": {"chunk": [
            {"type": "m.reaction", "key": "👍", "count": 3},
        ]}}},
    }));
    merge_reactions(
        &mut reactions,
        &[Reaction {
            key: "👍".into(),
            senders: vec![Reactor {
                user: "@b:hs".into(),
                event_id: "$r1".into(),
            }],
            bundled: 0,
        }],
    );
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].count(), 3);
    assert!(reactions[0].owns("@b:hs"));
}

#[test]
fn parse_relation_accepts_matching_reactions_only() {
    let hit = serde_json::json!({
        "type": "m.reaction",
        "sender": "@b:hs",
        "event_id": "$r1",
        "content": {"m.relates_to": {
            "rel_type": "m.annotation",
            "event_id": "$msg",
            "key": "👍",
        }},
    });
    let react = parse_relation(&hit, "$msg").expect("must parse");
    assert_eq!(react.key, "👍");
    assert_eq!(react.sender, "@b:hs");
    assert_eq!(react.event_id, "$r1");
    assert!(parse_relation(&hit, "$other").is_none());
    let wrong_type = serde_json::json!({
        "type": "m.room.message",
        "sender": "@b:hs",
        "event_id": "$r1",
        "content": {"body": "hi"},
    });
    assert!(parse_relation(&wrong_type, "$msg").is_none());
}

/// A room's loaded timeline: oldest first, newest last.
fn loaded(ids: &[&str]) -> Vec<TimelineRow> {
    ids.iter().map(|id| row(id, "@a:hs", "hi", None)).collect()
}

fn fetched(ids: &[&str]) -> std::collections::HashSet<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

#[test]
fn relation_targets_cover_loaded_messages_without_bundle_gate() {
    let rows = loaded(&["$a", "$b"]);
    assert_eq!(
        ThraceApp::relation_targets(&rows, &fetched(&[]), 25),
        vec!["$b".to_owned(), "$a".to_owned()],
        "newest first, no bundle needed"
    );
    assert_eq!(
        ThraceApp::relation_targets(&rows, &fetched(&["$b"]), 25),
        vec!["$a".to_owned()]
    );
    assert!(ThraceApp::relation_targets(&[], &fetched(&[]), 25).is_empty());
}

#[test]
fn relation_targets_reach_older_messages_once_the_newest_are_fetched() {
    // The regression: capping before dropping resolved ids pinned the window
    // to the newest screenful, so once those were fetched every older page
    // was starved and no request was ever issued.
    let rows: Vec<TimelineRow> = (0..40)
        .map(|i| row(&format!("${i}"), "@a:hs", "hi", None))
        .collect();
    // Everything from $15 up is already resolved.
    let done: std::collections::HashSet<String> = (15..40).map(|i| format!("${i}")).collect();
    let targets = ThraceApp::relation_targets(&rows, &done, 12);
    assert_eq!(targets.len(), 12, "older messages must still be reachable");
    assert!(
        targets.iter().all(|id| !done.contains(id)),
        "already-resolved messages must not be re-requested: {targets:?}"
    );
    assert_eq!(targets[0], "$14", "newest unresolved first");
}

#[test]
fn relation_targets_cap_the_burst() {
    let rows = loaded(&["$a", "$b", "$c", "$d"]);
    assert_eq!(
        ThraceApp::relation_targets(&rows, &fetched(&[]), 2),
        vec!["$d".to_owned(), "$c".to_owned()],
        "a page drains newest first, a burst at a time"
    );
}

#[test]
fn relation_targets_skip_what_cannot_carry_reactions() {
    // System notices and un-acked local echoes have no relations to fetch.
    let notice = TimelineRow {
        sender: "system".into(),
        ..row("$s", "@a:hs", "joined", None)
    };
    let rows = vec![
        notice,
        row("local-1", "@me:hs", "sending", Some("t1")),
        row("$a", "@a:hs", "hi", None),
    ];
    assert_eq!(
        ThraceApp::relation_targets(&rows, &fetched(&[]), 25),
        vec!["$a".to_owned()]
    );
}

#[test]
fn a_receipt_waits_until_the_newest_message_is_on_screen() {
    let rows = [
        row("$a", "@a:hs", "old", None),
        row("$b", "@b:hs", "new", None),
    ];
    assert_eq!(
        receipt_target(&rows, Some("$b"), true, None),
        Some("$b"),
        "at the bottom of a focused window"
    );
    assert_eq!(
        receipt_target(&rows, Some("$a"), true, None),
        None,
        "scrolled up: the newest message has not been seen"
    );
    assert_eq!(
        receipt_target(&rows, None, true, None),
        None,
        "nothing on screen"
    );
}

#[test]
fn a_receipt_waits_for_window_focus() {
    let rows = [row("$b", "@b:hs", "new", None)];
    assert_eq!(receipt_target(&rows, Some("$b"), false, None), None);
    // Refocusing with the same message on screen sends it.
    assert_eq!(receipt_target(&rows, Some("$b"), true, None), Some("$b"));
}

#[test]
fn a_receipt_is_not_resent_for_the_same_message() {
    let rows = [row("$b", "@b:hs", "new", None)];
    assert_eq!(receipt_target(&rows, Some("$b"), true, Some("$b")), None);
    // A different room's last receipt must not suppress this one.
    assert_eq!(
        receipt_target(&rows, Some("$b"), true, Some("$other")),
        Some("$b")
    );
}

#[test]
fn system_notices_never_carry_a_receipt() {
    let notice = TimelineRow {
        sender: "system".into(),
        ..row("$s", "@a:hs", "joined", None)
    };
    let rows = [row("$b", "@b:hs", "new", None), notice];
    // The notice is the last row but never enters the visible set, so the
    // real message below it is what gets marked read.
    assert_eq!(receipt_target(&rows, Some("$b"), true, None), Some("$b"));
    // A local echo cannot be receipted either.
    let echo = [row("local-1", "@me:hs", "sending", Some("t1"))];
    assert_eq!(receipt_target(&echo, Some("local-1"), true, None), None);
}

#[test]
fn sync_reaction_folds_into_target() {
    let mut rows = vec![row("$msg", "@a:hs", "hi", None)];
    let react = |sender: &str, id: &str| SyncReaction {
        target: "$msg".into(),
        key: "\u{1F44D}".into(),
        sender: sender.into(),
        event_id: id.into(),
    };
    assert!(fold_reaction(&mut rows, &react("@b:hs", "$r1")));
    assert!(fold_reaction(&mut rows, &react("@c:hs", "$r2")));
    assert_eq!(rows[0].reactions.len(), 1);
    assert_eq!(rows[0].reactions[0].count(), 2);
}

#[test]
fn sync_fills_in_the_event_id_of_our_local_echo() {
    // Echo upgrade, not duplicate: no event id until sync confirms.
    let mut rows = vec![row("$msg", "@a:hs", "hi", None)];
    ThraceApp::bump_reaction(&mut rows[0], "\u{1F44D}".into(), "@me:hs");
    assert_eq!(rows[0].reactions[0].count(), 1);
    assert!(rows[0].reactions[0].event_of("@me:hs").is_none());

    fold_reaction(
        &mut rows,
        &SyncReaction {
            target: "$msg".into(),
            key: "\u{1F44D}".into(),
            sender: "@me:hs".into(),
            event_id: "$mine".into(),
        },
    );
    assert_eq!(rows[0].reactions[0].count(), 1, "must not duplicate");
    assert_eq!(rows[0].reactions[0].event_of("@me:hs"), Some("$mine"));
}

#[test]
fn redaction_removes_the_reaction_it_names() {
    let mut rows = vec![row("$msg", "@a:hs", "hi", None)];
    for (sender, id) in [("@b:hs", "$r1"), ("@c:hs", "$r2")] {
        fold_reaction(
            &mut rows,
            &SyncReaction {
                target: "$msg".into(),
                key: "\u{1F44D}".into(),
                sender: sender.into(),
                event_id: id.into(),
            },
        );
    }
    assert_eq!(rows[0].reactions[0].count(), 2);

    redact_in(&mut rows, "$r1");
    assert_eq!(rows[0].reactions[0].count(), 1);
    assert!(!rows[0].reactions[0].owns("@b:hs"));

    redact_in(&mut rows, "$r2");
    assert!(rows[0].reactions.is_empty(), "empty group must be dropped");
}

#[test]
fn redacting_a_message_tombstones_it_in_place() {
    let mut rows = vec![
        row("$a", "@a:hs", "first", None),
        row("$b", "@b:hs", "second", None),
    ];
    redact_in(&mut rows, "$a");
    assert_eq!(rows.len(), 2, "slot is kept so ordering still reads");
    assert_eq!(rows[0].body, DELETED_BODY);
    assert_eq!(rows[1].body, "second");
}

#[test]
fn upload_sniffs_mime_and_caps_size() {
    let img = PendingUpload::from_bytes("cat.png".to_owned(), vec![0u8; 16]).unwrap();
    assert!(img.is_image());
    let bin = PendingUpload::from_bytes("archive.zzz-unknown".to_owned(), vec![0u8; 16]).unwrap();
    assert!(!bin.is_image());
    assert_eq!(bin.mime, "application/octet-stream");
    assert!(PendingUpload::from_bytes(
        "big.bin".to_owned(),
        vec![0u8; PendingUpload::MAX_BYTES + 1]
    )
    .is_err());
}

fn stamped(id: &str, sender: &str, origin_server_ts: u64) -> TimelineRow {
    TimelineRow {
        origin_server_ts,
        ..row(id, sender, "hi", None)
    }
}

#[test]
fn grouping_needs_same_sender_within_two_minutes() {
    // Midday, so the local day cannot change within the offsets used here.
    let noon = chrono::Local::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .unwrap()
        .timestamp_millis() as u64;
    let first = stamped("$1", "@a:x", noon);
    assert!(continues_group(
        &first,
        &stamped("$2", "@a:x", noon + 119_000)
    ));
    assert!(!continues_group(
        &first,
        &stamped("$2", "@a:x", noon + 120_000)
    ));
    assert!(!continues_group(
        &first,
        &stamped("$2", "@b:x", noon + 1_000)
    ));
    assert!(!continues_group(
        &stamped("$0", "system", noon),
        &stamped("$1", "system", noon)
    ));
    // Local echoes have no server stamp yet and stay with their sender.
    assert!(continues_group(&first, &stamped("$2", "@a:x", 0)));
}

#[test]
fn grouping_breaks_across_local_days() {
    let midnight = chrono::Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .unwrap()
        .timestamp_millis() as u64;
    let late = stamped("$1", "@a:x", midnight - 30_000);
    let early = stamped("$2", "@a:x", midnight + 30_000);
    assert_ne!(
        local_day(late.origin_server_ts),
        local_day(early.origin_server_ts)
    );
    assert!(!continues_group(&late, &early));
    assert_eq!(local_day(0), None);
}

#[cfg(feature = "slint-ui")]
#[test]
fn day_labels_name_recent_days() {
    use crate::app::text::day_label;
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 23).unwrap();
    assert_eq!(day_label(today, today), "Today");
    assert_eq!(day_label(today.pred_opt().unwrap(), today), "Yesterday");
    assert_eq!(
        day_label(chrono::NaiveDate::from_ymd_opt(2026, 3, 2).unwrap(), today),
        "Monday, 02 March"
    );
    assert_eq!(
        day_label(
            chrono::NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
            today
        ),
        "31 December 2025"
    );
}

#[test]
fn clock_setting_picks_24_or_12_hour_times() {
    let afternoon = chrono::Local::now()
        .date_naive()
        .and_hms_opt(13, 5, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .unwrap()
        .timestamp_millis() as u64;
    assert_eq!(format_ts_clock(Some(afternoon), false), "13:05");
    assert_eq!(format_ts_clock(Some(afternoon), true), "1:05 PM");
    assert_eq!(format_ts(Some(afternoon)), "13:05");
}

#[test]
fn presence_events_map_to_a_state() {
    use crate::app::decode::presence_update;
    let event = |content: serde_json::Value| serde_json::json!({ "type": "m.presence", "sender": "@a:hs", "content": content });
    assert_eq!(
        presence_update(&event(serde_json::json!({ "presence": "offline" }))),
        Some(("@a:hs".into(), "offline".into()))
    );
    assert_eq!(
        presence_update(&event(
            serde_json::json!({ "presence": "unavailable", "currently_active": true })
        )),
        Some(("@a:hs".into(), "online".into()))
    );
    assert_eq!(presence_update(&event(serde_json::json!({}))), None);
}

#[test]
fn a_receipt_on_a_hidden_event_lands_on_the_message_before_it() {
    let stamped = |id: &str, ts: u64| TimelineRow {
        origin_server_ts: ts,
        ..row(id, "@b:hs", "hi", None)
    };
    let mut rows = [
        stamped("$a", 1_000),
        stamped("$b", 2_000),
        stamped("$c", 3_000),
    ];
    // The reader's receipt points at a reaction sent after "$b".
    assert!(seen_in(&mut rows, "$reaction", "@reader:hs", 2_500));
    assert_eq!(rows[1].seen_by, vec!["@reader:hs".to_owned()]);
    // An older stamp can't pull the receipt back.
    assert!(!seen_in(&mut rows, "$older", "@reader:hs", 1_500));
    assert_eq!(rows[1].seen_by, vec!["@reader:hs".to_owned()]);
    assert!(seen_in(&mut rows, "$c", "@reader:hs", 0));
    assert!(rows[1].seen_by.is_empty());
}
