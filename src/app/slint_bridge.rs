/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Temporary adapter while Matrix state is extracted from the egui application.

use super::ThraceApp;
use crate::{
    AppWindow, CodeLineView, CodeTokenView, CommandView, DeviceView, EmbedRuleView, EmbedView,
    EmojiCategoryView, EmojiView, MemberView, MessageBlockView, MessageView, PackView, PinView,
    ReactionView, ReceiptView, RoomView, SasEmojiView, Theme, UploadView,
};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

struct SlintMedia {
    ready: HashMap<String, slint::Image>,
    pending: HashSet<String>,
    tx: std::sync::mpsc::Sender<(String, Option<egui::ColorImage>)>,
    rx: std::sync::mpsc::Receiver<(String, Option<egui::ColorImage>)>,
}

impl SlintMedia {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            ready: HashMap::new(),
            pending: HashSet::new(),
            tx,
            rx,
        }
    }

    fn poll(&mut self) {
        while let Ok((mxc, image)) = self.rx.try_recv() {
            self.pending.remove(&mxc);
            if let Some(image) = image.and_then(color_image_to_slint) {
                self.ready.insert(mxc, image);
            }
        }
    }

    fn request(&mut self, core: &ThraceApp, mxc: &str) {
        let uri: &matrix_sdk::ruma::MxcUri = mxc.into();
        self.request_source(
            core,
            mxc,
            matrix_sdk::ruma::events::room::MediaSource::Plain(uri.to_owned()),
            (96, 96),
        );
    }

    fn request_source(
        &mut self,
        core: &ThraceApp,
        key: &str,
        source: matrix_sdk::ruma::events::room::MediaSource,
        size: (u32, u32),
    ) {
        let mxc = key;
        if mxc.is_empty() || self.ready.contains_key(mxc) {
            return;
        }
        let Some(client) = core.client.clone() else {
            return;
        };
        if !self.pending.insert(mxc.to_owned()) {
            return;
        }
        let fetch = crate::media_cache::MediaFetch {
            mxc: mxc.to_owned(),
            source,
            thumb_width: size.0,
            thumb_height: size.1,
            thumbnail: true,
        };
        let tx = self.tx.clone();
        let key = mxc.to_owned();
        core.rt.spawn(async move {
            let result = crate::media_cache::fetch_and_decode(&client, fetch).await;
            let image = result.image.ok().and_then(first_frame);
            let _ = tx.send((key, image));
        });
    }

    fn request_original(
        &mut self,
        core: &ThraceApp,
        key: &str,
        source: matrix_sdk::ruma::events::room::MediaSource,
    ) {
        if key.is_empty() || self.ready.contains_key(key) || !self.pending.insert(key.to_owned()) {
            return;
        }
        let Some(client) = core.client.clone() else {
            return;
        };
        let fetch = crate::media_cache::MediaFetch {
            mxc: key.to_owned(),
            source,
            thumb_width: 2048,
            thumb_height: 2048,
            thumbnail: false,
        };
        let tx = self.tx.clone();
        let key = key.to_owned();
        core.rt.spawn(async move {
            let result = crate::media_cache::fetch_and_decode(&client, fetch).await;
            let image = result.image.ok().and_then(first_frame);
            let _ = tx.send((key, image));
        });
    }

    fn request_http(&mut self, core: &ThraceApp, url: &str) {
        let key = format!("http:{url}");
        if self.ready.contains_key(&key) || !self.pending.insert(key.clone()) {
            return;
        }
        let tx = self.tx.clone();
        let target = url.to_owned();
        core.rt.spawn(async move {
            let image = crate::embed::fetch_image(&target).await.ok();
            let _ = tx.send((key, image));
        });
    }

    fn get(&self, mxc: Option<&String>) -> (slint::Image, bool) {
        let image = mxc.and_then(|mxc| self.ready.get(mxc)).cloned();
        (image.clone().unwrap_or_default(), image.is_some())
    }
}

fn first_frame(decoded: crate::media_cache::Decoded) -> Option<egui::ColorImage> {
    Some(match decoded {
        crate::media_cache::Decoded::Still(image) => image,
        crate::media_cache::Decoded::Frames(frames) => frames.into_iter().next()?.0,
    })
}

fn color_image_to_slint(image: egui::ColorImage) -> Option<slint::Image> {
    let width = u32::try_from(image.size[0]).ok()?;
    let height = u32::try_from(image.size[1]).ok()?;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    for (target, pixel) in buffer.make_mut_slice().iter_mut().zip(image.pixels) {
        *target = slint::Rgba8Pixel {
            r: pixel.r(),
            g: pixel.g(),
            b: pixel.b(),
            a: pixel.a(),
        };
    }
    Some(slint::Image::from_rgba8(buffer))
}

fn initial(name: &str) -> slint::SharedString {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into())
        .into()
}

fn compact_preview(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn playback_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

/// One sidebar entry: a section header or a room (an index into `core.rooms`).
#[derive(Debug, Clone, PartialEq)]
enum ListItem {
    Header {
        section: &'static str,
        unread: usize,
        collapsed: bool,
    },
    Room(usize),
}

/// Sidebar sections in display order, like Element and SchildiChat.
const SECTIONS: [&str; 4] = ["Favourites", "People", "Rooms", "Low priority"];

fn room_section(room: &super::RoomEntry) -> &'static str {
    if room.meta.favourite {
        "Favourites"
    } else if room.meta.low_priority {
        "Low priority"
    } else if room.is_dm {
        "People"
    } else {
        "Rooms"
    }
}

/// Filter, sort and optionally group the sidebar. `rooms` itself is never reordered because
/// the rest of the app addresses rooms by index.
fn room_list(
    rooms: &[super::RoomEntry],
    filter: &str,
    sort: &str,
    grouped: bool,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<ListItem> {
    let mut order: Vec<usize> = (0..rooms.len())
        .filter(|&i| match filter {
            "rooms" => !rooms[i].is_dm,
            "dms" => rooms[i].is_dm,
            _ => true,
        })
        .collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&rooms[a], &rooms[b]);
        let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
        let by_activity = || b.last_activity.cmp(&a.last_activity).then_with(by_name);
        match sort {
            "name" => by_name(),
            "unread" => (b.unread > 0).cmp(&(a.unread > 0)).then_with(by_activity),
            _ => by_activity(),
        }
    });
    if !grouped {
        return order.into_iter().map(ListItem::Room).collect();
    }
    let mut items = Vec::new();
    for section in SECTIONS {
        let in_section: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| room_section(&rooms[i]) == section)
            .collect();
        if in_section.is_empty() {
            continue;
        }
        let is_collapsed = collapsed.contains(section);
        items.push(ListItem::Header {
            section,
            unread: in_section.iter().map(|&i| rooms[i].unread).sum(),
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            items.extend(in_section.into_iter().map(ListItem::Room));
        }
    }
    items
}

/// "Alice is typing…" line for the users typing in the open room.
fn typing_text(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{one} is typing…"),
        [one, two] => format!("{one} and {two} are typing…"),
        [one, rest @ ..] => format!("{one} and {} others are typing…", rest.len()),
    }
}

/// Tooltip for a message's read receipts: "Seen by Ann, Bo and 3 others".
fn seen_by_text(names: &[String]) -> String {
    const LISTED: usize = 8;
    match names.len() {
        0 => String::new(),
        n if n <= LISTED => format!("Seen by {}", names.join(", ")),
        n => format!(
            "Seen by {} and {} others",
            names[..LISTED].join(", "),
            n - LISTED
        ),
    }
}

/// Member-list group for a power level, highest first (Cinny's default tags).
fn member_group(power: i64) -> &'static str {
    match power {
        i64::MAX => "Creators",
        100.. => "Admins",
        50..=99 => "Moderators",
        ..=-1 => "Muted",
        _ => "Members",
    }
}

/// Message time in the chosen clock; local echoes keep the stamp taken when they were sent.
fn stamp(origin_server_ts: u64, fallback: &str, twelve_hour: bool) -> String {
    if origin_server_ts == 0 {
        return fallback.to_owned();
    }
    super::text::format_ts_clock(Some(origin_server_ts), twelve_hour)
}

fn egui_to_slint(color: egui::Color32) -> slint::Color {
    slint::Color::from_argb_u8(color.a(), color.r(), color.g(), color.b())
}

fn slint_color(value: Option<&String>, fallback: (u8, u8, u8)) -> slint::Color {
    egui_to_slint(
        value
            .and_then(|value| crate::theme::parse_hex(value))
            .unwrap_or_else(|| egui::Color32::from_rgb(fallback.0, fallback.1, fallback.2)),
    )
}

fn apply_theme(window: &AppWindow, theme: &crate::theme::ThemeFile) {
    let tokens = window.global::<Theme>();
    tokens.set_bg(slint_color(theme.palette.get("bg"), (17, 20, 24)));
    tokens.set_panel(slint_color(theme.palette.get("panel"), (23, 29, 35)));
    tokens.set_text(slint_color(theme.palette.get("text"), (234, 241, 245)));
    tokens.set_muted(slint_color(theme.palette.get("muted"), (130, 146, 159)));
    tokens.set_selection(slint_color(theme.palette.get("selection"), (40, 57, 67)));
    tokens.set_accent(slint_color(theme.palette.get("accent"), (113, 201, 194)));
    tokens.set_hover(slint_color(
        theme.palette.get("surface").or(theme.palette.get("hover")),
        (35, 46, 55),
    ));
    tokens.set_border(slint_color(theme.palette.get("border"), (44, 58, 68)));
    tokens.set_danger(slint_color(theme.palette.get("danger"), (255, 133, 133)));
    tokens.set_success(slint_color(theme.palette.get("success"), (100, 201, 138)));
    tokens.set_warning(slint_color(theme.palette.get("warning"), (229, 184, 92)));
    window.set_theme_name(theme.name.clone().into());
}

fn model_contents_eq<T: PartialEq>(model: &ModelRc<T>, next: &[T]) -> bool {
    model.row_count() == next.len()
        && (0..next.len()).all(|i| model.row_data(i).as_ref() == next.get(i))
}

fn reuse_or_new<T: Clone + PartialEq + 'static>(old: Option<ModelRc<T>>, next: Vec<T>) -> ModelRc<T> {
    if let Some(old) = old {
        if model_contents_eq(&old, &next) {
            return old;
        }
    }
    ModelRc::new(VecModel::from(next))
}

fn reconcile<T: Clone + PartialEq + 'static>(model: &VecModel<T>, next: Vec<T>) {
    let common = model.row_count().min(next.len());
    for (index, value) in next.iter().take(common).enumerate() {
        if model.row_data(index).as_ref() != Some(value) {
            model.set_row_data(index, value.clone());
        }
    }
    while model.row_count() > next.len() {
        model.remove(model.row_count() - 1);
    }
    for value in next.into_iter().skip(common) {
        model.push(value);
    }
}

fn fuzzy_shortcode_score(value: &str, query: &str) -> Option<usize> {
    let value = value.to_lowercase();
    let query = query.trim().trim_matches(':').to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if value == query {
        return Some(0);
    }
    if value.starts_with(&query) {
        return Some(10 + value.len().saturating_sub(query.len()));
    }
    if let Some(position) = value.find(&query) {
        return Some(100 + position);
    }
    let mut cursor = 0usize;
    let mut gaps = 0usize;
    for needle in query.chars() {
        let remainder = value.get(cursor..)?;
        let offset = remainder.find(needle)?;
        gaps += offset;
        cursor += offset + needle.len_utf8();
    }
    Some(1_000 + gaps)
}

fn emoji_views(
    core: &ThraceApp,
    media: &mut SlintMedia,
    query: &str,
    tab: &str,
    category: &str,
) -> Vec<EmojiView> {
    if tab == "emoji" {
        let showing_recent = query.trim().is_empty() && category == "Frequently used";
        let mut values: Vec<(String, bool)> = if showing_recent {
            core.recent_emoji
                .iter()
                .cloned()
                .map(|emoji| (emoji, false))
                .collect()
        } else {
            Vec::new()
        };
        let found: Vec<(&str, &str, bool)> = if query.trim().is_empty() && !showing_recent {
            crate::emoji::CATEGORIES
                .iter()
                .filter(|group| group.name == category)
                .flat_map(|group| {
                    group
                        .emojis
                        .iter()
                        .map(move |(emoji, skin)| (group.name, *emoji, *skin))
                })
                .collect()
        } else {
            crate::emoji::search(query)
        };
        for (_, value, supports_skin) in found {
            if !values.iter().any(|(existing, _)| existing == value) {
                values.push((value.to_owned(), supports_skin));
            }
        }
        return values
            .into_iter()
            .map(|(base, supports_skin)| {
                let value = if supports_skin && !core.skin_tone.is_empty() {
                    format!("{base}{}", core.skin_tone)
                } else {
                    base.clone()
                };
                let key = format!("unicode:{value}");
                if !media.ready.contains_key(&key) {
                    if let Some(image) = core
                        .emoji_font
                        .decode(&value)
                        .and_then(color_image_to_slint)
                    {
                        media.ready.insert(key.clone(), image);
                    }
                }
                let (image, has_image) = media.get(Some(&key));
                EmojiView {
                    name: crate::emoji::name_of(&base).into(),
                    value: value.into(),
                    pack: category.into(),
                    image,
                    has_image,
                    custom: false,
                    sticker: false,
                }
            })
            .collect();
    }
    let q = query.trim().trim_matches(':').to_lowercase();
    let stickers = tab == "stickers";
    let mut candidates = core
        .packs
        .packs()
        .iter()
        .flat_map(|pack| pack.images.iter().map(move |item| (pack, item)))
        .filter(|(_, item)| {
            if stickers {
                item.is_sticker
            } else {
                item.is_emoji
            }
        })
        .filter_map(|(pack, item)| {
            fuzzy_shortcode_score(&item.shortcode, &q).map(|score| (score, pack, item))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(left_score, _, left), (right_score, _, right)| {
        left_score
            .cmp(right_score)
            .then_with(|| left.shortcode.cmp(&right.shortcode))
    });
    candidates
        .into_iter()
        .map(|(_, pack, item)| {
            media.request(core, &item.mxc_url);
            let (image, has_image) = media.get(Some(&item.mxc_url));
            EmojiView {
                value: format!(":{}:", item.shortcode).into(),
                name: item.shortcode.clone().into(),
                pack: pack.display_name.clone().into(),
                image,
                has_image,
                custom: true,
                sticker: stickers,
            }
        })
        .collect()
}

#[cfg(test)]
mod slint_bridge_tests {
    use super::{
        compact_preview, fuzzy_shortcode_score, member_group, room_list, seen_by_text,
        spans_markdown, typing_text, ListItem,
    };
    use crate::app::{RoomEntry, RoomMeta};
    use std::collections::HashSet;

    fn room(name: &str, is_dm: bool, last_activity: u64, unread: usize) -> RoomEntry {
        RoomEntry {
            room_id: format!("!{name}:hs"),
            name: name.into(),
            is_dm,
            last_activity,
            unread,
            ..Default::default()
        }
    }

    #[test]
    fn room_list_sorts_without_reordering_rooms() {
        let rooms = [
            room("beta", false, 10, 0),
            room("alpha", false, 30, 0),
            room("gamma", false, 20, 2),
        ];
        let none = HashSet::new();
        let order = |sort| room_list(&rooms, "all", sort, false, &none);
        assert_eq!(
            order("activity"),
            [ListItem::Room(1), ListItem::Room(2), ListItem::Room(0)]
        );
        assert_eq!(
            order("unread"),
            [ListItem::Room(2), ListItem::Room(1), ListItem::Room(0)]
        );
        assert_eq!(
            order("name"),
            [ListItem::Room(1), ListItem::Room(0), ListItem::Room(2)]
        );
    }

    #[test]
    fn room_list_groups_sections_and_folds_them() {
        let mut fav = room("fav", false, 1, 0);
        fav.meta = RoomMeta {
            favourite: true,
            low_priority: true,
            topic: None,
        };
        let mut low = room("low", false, 1, 3);
        low.meta.low_priority = true;
        let rooms = [room("dm", true, 1, 0), room("room", false, 1, 0), fav, low];
        let header = |section, unread, collapsed| ListItem::Header {
            section,
            unread,
            collapsed,
        };
        // Favourite wins over low priority; empty sections are skipped.
        assert_eq!(
            room_list(&rooms, "all", "name", true, &HashSet::new()),
            [
                header("Favourites", 0, false),
                ListItem::Room(2),
                header("People", 0, false),
                ListItem::Room(0),
                header("Rooms", 0, false),
                ListItem::Room(1),
                header("Low priority", 3, false),
                ListItem::Room(3),
            ]
        );
        let folded = HashSet::from(["Low priority".to_owned()]);
        let items = room_list(&rooms, "dms", "name", true, &folded);
        assert_eq!(items, [header("People", 0, false), ListItem::Room(0)]);
        let items = room_list(&rooms, "rooms", "name", true, &folded);
        assert_eq!(items.last(), Some(&header("Low priority", 3, true)));
    }

    #[test]
    fn typing_line_names_up_to_two_people() {
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(typing_text(&[]), "");
        assert_eq!(typing_text(&names(&["Ann"])), "Ann is typing…");
        assert_eq!(
            typing_text(&names(&["Ann", "Bo"])),
            "Ann and Bo are typing…"
        );
        assert_eq!(
            typing_text(&names(&["Ann", "Bo", "Cy"])),
            "Ann and 2 others are typing…"
        );
    }

    #[test]
    fn mentions_are_coloured_links_that_still_parse() {
        let spans = [
            crate::markdown::Span::plain("hi "),
            crate::markdown::Span {
                link: Some("https://matrix.to/#/@ann:hs".into()),
                ..crate::markdown::Span::plain("Ann <3")
            },
            crate::markdown::Span {
                link: Some("https://example.com".into()),
                ..crate::markdown::Span::plain("site")
            },
        ];
        let markdown = spans_markdown(&spans, &|mxid| {
            assert_eq!(mxid, "@ann:hs");
            "#7dd3d8".into()
        });
        assert_eq!(
            markdown,
            "hi <font color=\"#7dd3d8\">**[Ann \\<3](https://matrix.to/#/@ann:hs)**</font>[site](https://example.com)"
        );
        assert!(slint::StyledText::from_markdown(&markdown).is_ok());
    }

    #[test]
    fn seen_by_lists_the_first_eight_readers() {
        let names: Vec<String> = (1..=10).map(|i| format!("u{i}")).collect();
        assert_eq!(seen_by_text(&[]), "");
        assert_eq!(seen_by_text(&names[..2]), "Seen by u1, u2");
        assert_eq!(
            seen_by_text(&names),
            "Seen by u1, u2, u3, u4, u5, u6, u7, u8 and 2 others"
        );
    }

    #[test]
    fn member_groups_follow_power_levels() {
        assert_eq!(member_group(i64::MAX), "Creators");
        assert_eq!(member_group(100), "Admins");
        assert_eq!(member_group(50), "Moderators");
        assert_eq!(member_group(0), "Members");
        assert_eq!(member_group(-1), "Muted");
    }

    #[test]
    fn custom_emoji_fuzzy_search_ranks_exact_prefix_and_subsequence() {
        assert!(
            fuzzy_shortcode_score("party_parrot", "party")
                < fuzzy_shortcode_score("afterparty", "party")
        );
        assert!(fuzzy_shortcode_score("party_parrot", "ppr").is_some());
        assert!(fuzzy_shortcode_score("party_parrot", "xyz").is_none());
    }

    #[test]
    fn conversation_preview_collapses_markdown_line_breaks() {
        assert_eq!(
            compact_preview("first\n  second\tthird"),
            "first second third"
        );
    }
}

fn markdown_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Build styled text from spans. User mentions are bold and drawn in that user's nick colour
/// (`mention_color` maps an mxid to a hex colour), like the sender names.
fn styled_spans(
    spans: &[crate::markdown::Span],
    mention_color: &dyn Fn(&str) -> String,
) -> slint::StyledText {
    slint::StyledText::from_markdown(&spans_markdown(spans, mention_color)).unwrap_or_else(|_| {
        slint::StyledText::from_plain_text(
            &spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
        )
    })
}

/// The markdown (with inline `<font>` for mentions) that [`styled_spans`] parses.
fn spans_markdown(
    spans: &[crate::markdown::Span],
    mention_color: &dyn Fn(&str) -> String,
) -> String {
    let mut markdown = String::new();
    for span in spans {
        let mut text = markdown_escape(&span.text);
        if span.code && !span.text.contains('`') {
            text = format!("`{}`", span.text);
        }
        if span.bold {
            text = format!("**{text}**");
        }
        if span.italic {
            text = format!("*{text}*");
        }
        if let Some(link) = &span.link {
            let destination = link.replace(')', "\\)");
            text = format!("[{text}]({destination})");
            // Slint records a span when its tag closes and later spans win, so the colour has
            // to wrap the link: nested inside it, the link colour would paint over it.
            if let Some(mxid) = super::text::mention_mxid(link) {
                text = format!("<font color=\"{}\">**{text}**</font>", mention_color(&mxid));
            }
        }
        markdown.push_str(&text);
    }
    markdown
}

fn message_blocks(
    row: &super::TimelineRow,
    mention_color: &dyn Fn(&str) -> String,
) -> Vec<MessageBlockView> {
    crate::markdown::render_message(&row.body, row.formatted.as_deref())
        .blocks
        .into_iter()
        .map(|block| match block {
            crate::markdown::Block::Spans(spans) => MessageBlockView {
                kind: "text".into(),
                text: spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
                    .into(),
                styled: styled_spans(&spans, mention_color),
                language: "".into(),
                lines: ModelRc::default(),
            },
            crate::markdown::Block::Code(code) => {
                let lines: Vec<CodeLineView> = code
                    .code
                    .split('\n')
                    .map(|line| {
                        let tokens = crate::highlight::tokenize(line, &code.lang)
                            .into_iter()
                            .map(|(text, kind)| {
                                let kind = match kind {
                                    crate::highlight::Kind::Plain => "plain",
                                    crate::highlight::Kind::Keyword => "keyword",
                                    crate::highlight::Kind::String => "string",
                                    crate::highlight::Kind::Comment => "comment",
                                    crate::highlight::Kind::Number => "number",
                                };
                                CodeTokenView {
                                    text: text.into(),
                                    kind: kind.into(),
                                }
                            })
                            .collect::<Vec<_>>();
                        CodeLineView {
                            tokens: ModelRc::new(VecModel::from(tokens)),
                        }
                    })
                    .collect();
                MessageBlockView {
                    kind: "code".into(),
                    text: code.code.into(),
                    styled: slint::StyledText::from_plain_text(""),
                    language: code.lang.into(),
                    lines: ModelRc::new(VecModel::from(lines)),
                }
            }
        })
        .collect()
}

pub fn run(
    config: crate::config::ConfigFile,
    cli_theme: Option<String>,
) -> Result<(), slint::PlatformError> {
    let mut core = ThraceApp::new_with_context(egui::Context::default(), config, cli_theme);
    core.try_restore_cached();
    let core = Rc::new(RefCell::new(core));
    let window = AppWindow::new()?;
    let rooms_model = Rc::new(VecModel::<RoomView>::default());
    let messages_model = Rc::new(VecModel::<MessageView>::default());
    let members_model = Rc::new(VecModel::<MemberView>::default());
    let mention_model = Rc::new(VecModel::<MemberView>::default());
    let pins_model = Rc::new(VecModel::<PinView>::default());
    let emoji_model = Rc::new(VecModel::<EmojiView>::default());
    let embed_model = Rc::new(VecModel::<EmbedRuleView>::default());
    let devices_model = Rc::new(VecModel::<DeviceView>::default());
    let sas_model = Rc::new(VecModel::<SasEmojiView>::default());
    let emoji_categories_model = Rc::new(VecModel::<EmojiCategoryView>::default());
    let skin_tones_model = Rc::new(VecModel::<EmojiView>::default());
    let uploads_model = Rc::new(VecModel::<UploadView>::default());
    let commands_model = Rc::new(VecModel::<CommandView>::default());
    let packs_model = Rc::new(VecModel::<PackView>::default());
    let ignored_model = Rc::new(VecModel::<slint::SharedString>::default());
    let opened_sso_url = Rc::new(RefCell::new(String::new()));
    let video_target = Rc::new(RefCell::new(None::<String>));
    let lightbox_target = Rc::new(RefCell::new(None::<String>));
    let own_unverified = Rc::new(std::cell::Cell::new(false));
    let media = Rc::new(RefCell::new(SlintMedia::new()));
    // Sidebar sections the user folded; kept for the session only.
    let collapsed_sections = Rc::new(RefCell::new(std::collections::HashSet::<String>::new()));
    {
        let collapsed_sections = Rc::clone(&collapsed_sections);
        window.on_toggle_section(move |section| {
            let mut collapsed = collapsed_sections.borrow_mut();
            if !collapsed.remove(section.as_str()) {
                collapsed.insert(section.into());
            }
        });
    }
    window.set_rooms(ModelRc::from(Rc::clone(&rooms_model)));
    window.set_messages(ModelRc::from(Rc::clone(&messages_model)));
    window.set_members(ModelRc::from(Rc::clone(&members_model)));
    window.set_mention_results(ModelRc::from(Rc::clone(&mention_model)));
    window.set_pins(ModelRc::from(Rc::clone(&pins_model)));
    window.set_emoji_results(ModelRc::from(Rc::clone(&emoji_model)));
    window.set_embed_rules(ModelRc::from(Rc::clone(&embed_model)));
    window.set_devices(ModelRc::from(Rc::clone(&devices_model)));
    window.set_sas_emojis(ModelRc::from(Rc::clone(&sas_model)));
    window.set_emoji_categories(ModelRc::from(Rc::clone(&emoji_categories_model)));
    window.set_skin_tones(ModelRc::from(Rc::clone(&skin_tones_model)));
    window.set_pending_uploads(ModelRc::from(Rc::clone(&uploads_model)));
    window.set_command_results(ModelRc::from(Rc::clone(&commands_model)));
    window.set_emoji_packs(ModelRc::from(Rc::clone(&packs_model)));
    window.set_ignored_users(ModelRc::from(Rc::clone(&ignored_model)));
    {
        let core = core.borrow();
        apply_theme(&window, &core.theme);
    }

    {
        let core = Rc::clone(&core);
        window.on_login(move |homeserver, username, password| {
            let mut core = core.borrow_mut();
            core.login.homeserver = homeserver.into();
            core.login.username = username.into();
            core.login.password = password.into();
            core.start_login();
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_login_sso(move |homeserver| {
            let mut core = core.borrow_mut();
            core.login.homeserver = homeserver.into();
            let ctx = core.ctx.clone();
            core.start_sso(&ctx);
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_select_room(move |index| {
            let mut core = core.borrow_mut();
            if index >= 0 && (index as usize) < core.rooms.len() {
                core.switch_room(index as usize);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_load_older(move || core.borrow_mut().paginate_current());
    }
    {
        let core = Rc::clone(&core);
        window.on_retry_history(move || {
            let mut core = core.borrow_mut();
            if let Some(room_id) = core.current_room_id() {
                core.history_queue.select(&room_id);
                core.pump_history();
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_send_message(move |body| {
            let mut core = core.borrow_mut();
            core.input = body.into();
            core.send_current_input();
        });
    }
    {
        let weak = window.as_weak();
        window.on_accept_mention(move |mxid| {
            let Some(window) = weak.upgrade() else { return };
            let mut text = window.get_composer_text().to_string();
            if let Some(prefix) = super::text::mention_prefix(&text).map(str::to_owned) {
                text.truncate(text.len().saturating_sub(prefix.len()));
                text.push_str(mxid.as_str());
                text.push(' ');
                window.set_composer_text(text.into());
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_complete_command(move |command| {
            let Some(window) = weak.upgrade() else { return };
            let text = window.get_composer_text().to_string();
            let args = text
                .split_once(' ')
                .map(|(_, rest)| rest)
                .unwrap_or_default();
            let completed = if args.is_empty() {
                format!("{} ", command.as_str())
            } else {
                format!("{} {args}", command.as_str())
            };
            window.set_composer_text(completed.into());
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_room_action(move |room_id, action| {
            let mut core = core.borrow_mut();
            let action = match action.as_str() {
                "read" => super::RoomMenuAction::MarkRead,
                "unread" => super::RoomMenuAction::MarkUnread,
                "favourite" => {
                    // Flip locally now; the server's `m.tag` echo confirms it on the next sync.
                    let Some(room) = core
                        .rooms
                        .iter_mut()
                        .find(|r| r.room_id == room_id.as_str())
                    else {
                        return;
                    };
                    room.meta.favourite = !room.meta.favourite;
                    super::RoomMenuAction::Favourite(room.meta.favourite)
                }
                "notify-all" => super::RoomMenuAction::Notify(super::RoomNotify::All),
                "notify-mute" => super::RoomMenuAction::Notify(super::RoomNotify::Mute),
                "notify-mentions" => super::RoomMenuAction::Notify(super::RoomNotify::MentionsOnly),
                "invite" => super::RoomMenuAction::Invite,
                "copy" => super::RoomMenuAction::CopyLink,
                "leave" => super::RoomMenuAction::Leave,
                _ => return,
            };
            core.room_menu_action(room_id.to_string(), action);
            if let Some(window) = weak.upgrade() {
                if action == super::RoomMenuAction::Invite {
                    window.set_composer_text(core.input.clone().into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_mention_profile(move |mxid| {
            let Some(window) = weak.upgrade() else { return };
            let mut text = window.get_composer_text().to_string();
            if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                text.push(' ');
            }
            text.push_str(mxid.as_str());
            text.push(' ');
            window.set_composer_text(text.into());
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_show_profile(
            move |mxid, fallback_name, fallback_avatar, fallback_has_avatar| {
                let core = core.borrow();
                let (name, _, online) = core.profile_of(mxid.as_str());
                if let Some(window) = weak.upgrade() {
                    window.set_profile_id(mxid.clone());
                    let name = if name.is_empty() {
                        fallback_name
                    } else {
                        name.into()
                    };
                    window.set_profile_initial(initial(&name).into());
                    window.set_profile_name(name);
                    window.set_profile_avatar(fallback_avatar);
                    window.set_profile_has_avatar(fallback_has_avatar);
                    window.set_profile_online(online);
                    window.set_profile_shared_rooms(
                        core.shared_rooms(mxid.as_str()).min(i32::MAX as usize) as i32,
                    );
                    window.set_profile_is_self(mxid.as_str() == core.own_user);
                    window.invoke_show_profile_card();
                }
            },
        );
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_verify_profile(move |mxid| {
            let mut core = core.borrow_mut();
            core.verify_user_input = mxid.to_string();
            core.refresh_devices();
            if let Some(window) = weak.upgrade() {
                window.set_verify_target(mxid);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_toggle_audio(move |event_id| {
            let mut core = core.borrow_mut();
            let event_id = event_id.to_string();
            let Some(audio) = core
                .rows
                .iter()
                .find(|row| row.id == event_id)
                .and_then(|row| row.audio.clone())
            else {
                return;
            };
            let now = playback_now();
            if let Some((active_id, player)) = core.audio.as_mut() {
                if active_id == &event_id {
                    if player.finished {
                        player.seek(now, 0.0);
                        if player.is_paused() {
                            player.toggle_pause(now);
                        }
                    } else {
                        player.toggle_pause(now);
                    }
                    return;
                }
            }
            if let Some(path) = core.audio_paths.get(&audio.mxc).cloned() {
                let duration = audio
                    .duration_ms
                    .map(|ms| ms as f64 / 1000.0)
                    .or_else(|| core.audio_lengths.get(&event_id).copied())
                    .unwrap_or_default();
                match crate::audio::AudioPlayer::start(&path, now, 0.0, duration) {
                    Ok(player) => core.audio = Some((event_id, player)),
                    Err(error) => {
                        core.audio_errors.insert(event_id, error);
                    }
                }
            } else {
                core.audio_errors.remove(&event_id);
                core.fetch_audio(&event_id, &audio);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let media = Rc::clone(&media);
        let target = Rc::clone(&lightbox_target);
        window.on_open_image(move |event_id| {
            let event_id = event_id.to_string();
            let core = core.borrow();
            let Some(image) = core
                .rows
                .iter()
                .find(|row| row.id == event_id)
                .and_then(|row| row.image.clone())
            else {
                return;
            };
            let key = format!("original:{}", image.mxc);
            media
                .borrow_mut()
                .request_original(&core, &key, image.source);
            *target.borrow_mut() = Some(event_id);
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_save_image(move |event_id| {
            let image = core
                .borrow()
                .rows
                .iter()
                .find(|row| row.id == event_id.as_str())
                .and_then(|row| row.image.clone());
            if let Some(image) = image {
                core.borrow_mut().download_image(&image);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let target = Rc::clone(&video_target);
        let weak = window.as_weak();
        window.on_play_video(move |event_id| {
            let event_id = event_id.to_string();
            let mut core = core.borrow_mut();
            let Some(image) = core
                .rows
                .iter()
                .find(|row| row.id == event_id)
                .and_then(|row| row.image.clone())
                .filter(|image| image.is_video)
            else {
                return;
            };
            if target.borrow().as_deref() != Some(event_id.as_str()) {
                core.video = None;
            }
            *target.borrow_mut() = Some(event_id);
            if !core
                .video_file
                .as_ref()
                .is_some_and(|(mxc, _)| mxc == &image.mxc)
            {
                core.fetch_video(&image);
            }
            if let Some(window) = weak.upgrade() {
                window.set_video_title(image.name.into());
                window.set_video_state("Loading…".into());
                window.set_video_has_frame(false);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_toggle_video(move || {
            let mut core = core.borrow_mut();
            if let Some(player) = core.video.as_mut() {
                player.toggle_pause(playback_now());
                if let Some(window) = weak.upgrade() {
                    window.set_video_playing(!player.is_paused());
                }
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let target = Rc::clone(&video_target);
        let weak = window.as_weak();
        window.on_stop_video(move || {
            core.borrow_mut().video = None;
            *target.borrow_mut() = None;
            if let Some(window) = weak.upgrade() {
                window.set_video_has_frame(false);
                window.set_video_playing(false);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_open_pin(move |id| {
            let mut core = core.borrow_mut();
            let id = id.to_string();
            if !core.rows.iter().any(|row| row.id == id) {
                if let Some(row) = core
                    .pinned_messages
                    .iter()
                    .find(|pin| pin.id == id)
                    .and_then(|pin| pin.timeline_row.clone())
                {
                    let rows = Rc::make_mut(&mut core.rows);
                    let at = rows
                        .iter()
                        .position(|existing| existing.origin_server_ts > row.origin_server_ts)
                        .unwrap_or(rows.len());
                    rows.insert(at, row);
                }
            }
            core.scroll_to_event = Some(id.clone());
            core.highlight_event = Some((id.clone(), 0.0));
            if let Some(window) = weak.upgrade() {
                // Look up the pin itself; checking the old highlight first jumped back to it.
                if let Some(index) = core.rows.iter().position(|row| row.id == id) {
                    window.invoke_jump_to_message(index as i32, id.into());
                }
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_open_message(move |id| {
            let mut core = core.borrow_mut();
            let id = id.to_string();
            core.scroll_to_event = Some(id.clone());
            core.highlight_event = Some((id.clone(), 0.0));
            if let Some(window) = weak.upgrade() {
                if let Some(index) = core.rows.iter().position(|row| row.id == id) {
                    window.invoke_jump_to_message(index as i32, id.into());
                }
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_reply_message(move |id| {
            let id = id.to_string();
            if core.borrow().rows.iter().any(|row| row.id == id) {
                let mut core = core.borrow_mut();
                core.editing = None;
                core.replying_to = Some(id);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_edit_message(move |id| {
            let id = id.to_string();
            let mut core = core.borrow_mut();
            if let Some(row) = core
                .rows
                .iter()
                .find(|row| row.id == id && row.sender == core.own_user)
                .cloned()
            {
                core.replying_to = None;
                core.editing = Some(id);
                core.input = row.body.clone();
                if let Some(window) = weak.upgrade() {
                    window.set_composer_text(row.body.into());
                }
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_copy_message(move |id| {
            let mut core = core.borrow_mut();
            let Some(body) = core
                .rows
                .iter()
                .find(|row| row.id == id.as_str())
                .map(|row| row.body.clone())
            else {
                return;
            };
            core.status = match arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(body))
            {
                Ok(()) => "message copied".into(),
                Err(error) => format!("copy failed: {error}"),
            };
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_copy_text(move |text| {
            let mut core = core.borrow_mut();
            core.status = match arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
            {
                Ok(()) => "copied".into(),
                Err(error) => format!("copy failed: {error}"),
            };
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_open_dm(move |mxid, display| {
            core.borrow_mut().open_dm(mxid.as_str(), display.as_str())
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_refresh_devices(move || core.borrow_mut().refresh_devices());
    }
    {
        let core = Rc::clone(&core);
        window.on_load_devices(move |user| {
            let mut core = core.borrow_mut();
            core.verify_user_input = user.to_string();
            core.refresh_devices();
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_verify_device(move |user, device| {
            core.borrow_mut()
                .start_verify_device(user.as_str(), device.as_str())
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_confirm_verification(move |matched| core.borrow_mut().verify_confirm(matched));
    }
    {
        let core = Rc::clone(&core);
        window.on_accept_verification(move || core.borrow_mut().accept_incoming());
    }
    {
        let core = Rc::clone(&core);
        window.on_decline_verification(move || core.borrow_mut().decline_incoming());
    }
    {
        let core = Rc::clone(&core);
        window.on_select_skin_tone(move |modifier| core.borrow_mut().skin_tone = modifier.into());
    }
    {
        let core = Rc::clone(&core);
        window.on_remove_upload(move |index| {
            let mut core = core.borrow_mut();
            let index = index.max(0) as usize;
            if index < core.pending_uploads.len() {
                core.pending_uploads.remove(index);
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_set_notification_default(move |direct, mode| {
            let mode = match mode.as_str() {
                "all" => super::RoomNotify::All,
                "mute" => super::RoomNotify::Mute,
                _ => super::RoomNotify::MentionsOnly,
            };
            core.borrow_mut().set_default_notifications(direct, mode);
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_refresh_ignored(move || core.borrow_mut().refresh_ignored());
    }
    {
        let core = Rc::clone(&core);
        window.on_unignore_user(move |mxid| {
            let mut core = core.borrow_mut();
            core.moderate(mxid.to_string(), super::Moderation::Unignore);
            core.ignored_users.retain(|user| user != mxid.as_str());
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_open_link(move |url| {
            let target = {
                let core = core.borrow();
                crate::embed::rewrite(&core.embed_rules, url.as_str())
                    .unwrap_or_else(|| url.to_string())
            };
            if let Err(error) = webbrowser::open(&target) {
                core.borrow_mut().status = format!("open link: {error}");
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let media = Rc::clone(&media);
        let weak = window.as_weak();
        window.on_open_rich_link(move |url| {
            if let Some(mxid) = super::text::mention_mxid(url.as_str()) {
                let core = core.borrow();
                let (name, avatar_mxc, online) = core.profile_of(&mxid);
                if let Some(mxc) = avatar_mxc.as_ref() {
                    media.borrow_mut().request(&core, mxc);
                }
                let (avatar, has_avatar) = media.borrow().get(avatar_mxc.as_ref());
                if let Some(window) = weak.upgrade() {
                    window.set_profile_initial(initial(&name).into());
                    window.set_profile_name(name.into());
                    window.set_profile_id(mxid.clone().into());
                    window.set_profile_avatar(avatar);
                    window.set_profile_has_avatar(has_avatar);
                    window.set_profile_online(online);
                    window.set_profile_shared_rooms(
                        core.shared_rooms(&mxid).min(i32::MAX as usize) as i32,
                    );
                    window.set_profile_is_self(mxid == core.own_user);
                    window.invoke_show_profile_card();
                }
                return;
            }
            let target = {
                let core = core.borrow();
                crate::embed::rewrite(&core.embed_rules, url.as_str())
                    .unwrap_or_else(|| url.to_string())
            };
            if let Err(error) = webbrowser::open(&target) {
                core.borrow_mut().status = format!("open link: {error}");
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_delete_message(move |id| core.borrow_mut().delete_message(id.into()));
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_cancel_composer_context(move || {
            let mut core = core.borrow_mut();
            core.replying_to = None;
            core.editing = None;
            core.input.clear();
            if let Some(window) = weak.upgrade() {
                window.set_composer_text("".into());
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_edit_last_message(move || {
            let mut core = core.borrow_mut();
            core.start_edit_last();
            if let Some(window) = weak.upgrade() {
                window.set_composer_text(core.input.clone().into());
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_react_message(move |id, key| {
            let mut core = core.borrow_mut();
            if let Some(index) = core.rows.iter().position(|row| row.id == id.as_str()) {
                core.send_reaction(index, key.into());
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_logout(move || core.borrow_mut().logout());
    }
    {
        let core = Rc::clone(&core);
        window.on_open_attachments(move || core.borrow_mut().pick_files());
    }
    window.on_can_accept_drop(|data| data.has_file_paths());
    {
        let core = Rc::clone(&core);
        window.on_stage_drop(move |data| {
            let Ok(paths) = data.file_paths() else { return };
            let files = paths
                .map(|path| egui::DroppedFile {
                    path: Some(path.to_owned()),
                    ..Default::default()
                })
                .collect();
            core.borrow_mut().ingest_dropped(files);
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_paste_attachment(move || core.borrow_mut().paste_clipboard(true));
    }
    {
        let core = Rc::clone(&core);
        window.on_save_display_name(move |name| core.borrow_mut().set_nick(name.into()));
    }
    {
        let core = Rc::clone(&core);
        window.on_save_appearance(move |theme, size, previews, layout, clock| {
            let mut core = core.borrow_mut();
            core.settings_font_size = size.clamp(10.0, 24.0);
            core.show_previews = previews;
            if ["bubble", "modern", "compact"].contains(&layout.as_str()) {
                core.message_layout = layout.into();
            }
            if ["24h", "12h"].contains(&clock.as_str()) {
                core.time_format = clock.into();
            }
            core.settings_theme = theme.into();
            core.save_preferences();
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_save_bubble_colors(move |own, other| {
            let mut core = core.borrow_mut();
            // Empty means "use the theme"; anything else must be a colour we can parse.
            let valid = |hex: &str| hex.is_empty() || crate::theme::parse_hex(hex).is_some();
            if valid(&own) && valid(&other) {
                core.own_bubble_color = own.into();
                core.other_bubble_color = other.into();
                core.save_preferences();
            }
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_save_room_list(move |sort, grouped| {
            let mut core = core.borrow_mut();
            if ["activity", "unread", "name"].contains(&sort.as_str()) {
                core.room_sort = sort.into();
            }
            core.group_rooms = grouped;
            core.save_preferences();
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_save_embed_rule(move |index, enabled, hosts, open_with, api| {
            let mut core = core.borrow_mut();
            let Some(rule) = core.embed_rules.get_mut(index.max(0) as usize) else {
                return;
            };
            rule.enabled = enabled;
            rule.hosts = hosts
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_lowercase)
                .collect();
            rule.open_with = open_with.into();
            rule.api = api.into();
            core.embeds.clear();
            core.embed_images.clear();
            core.save_preferences();
        });
    }
    {
        let core = Rc::clone(&core);
        let media = Rc::clone(&media);
        let emoji_model = Rc::clone(&emoji_model);
        let weak = window.as_weak();
        window.on_search_emoji(move |query, tab| {
            let core = core.borrow();
            let category = weak
                .upgrade()
                .map(|window| window.get_emoji_category().to_string())
                .unwrap_or_default();
            let next = emoji_views(
                &core,
                &mut media.borrow_mut(),
                query.as_str(),
                tab.as_str(),
                &category,
            );
            reconcile(&emoji_model, next);
        });
    }
    {
        let core = Rc::clone(&core);
        window.on_pick_emoji(move |value, sticker| {
            if sticker {
                core.borrow_mut()
                    .send_sticker(value.trim_matches(':').to_owned());
            } else {
                core.borrow_mut().note_recent_emoji(value.as_str());
            }
        });
    }
    {
        let core = Rc::clone(&core);
        let weak = window.as_weak();
        window.on_change_theme(move |name| {
            let name = name.to_string();
            let Ok(mut theme) = crate::theme::ThemeFile::load_builtin(&name) else {
                return;
            };
            let mut core = core.borrow_mut();
            theme.font.mono_size = Some(core.settings_font_size);
            core.theme = theme;
            core.config_theme = name.clone();
            core.settings_theme = name;
            core.save_preferences();
            if let Some(window) = weak.upgrade() {
                apply_theme(&window, &core.theme);
            }
        });
    }

    let weak = window.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        let Some(window) = weak.upgrade() else { return };
        let mut core = core.borrow_mut();
        let mut media = media.borrow_mut();
        media.poll();
        if let Some(event_id) = lightbox_target.borrow().as_ref() {
            if let Some(image) = core
                .rows
                .iter()
                .find(|row| &row.id == event_id)
                .and_then(|row| row.image.as_ref())
            {
                let key = format!("original:{}", image.mxc);
                if let (full, true) = media.get(Some(&key)) {
                    window.set_lightbox_image(full);
                }
            }
        }
        while let Some((url, embed)) = core.embed_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            core.embeds.insert(url, embed);
        }
        if let Some(list) = core.ignored_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            core.ignored_users = list;
        }
        let ctx = core.ctx.clone();
        core.poll_login(&ctx);
        if !core.login.sso_url.is_empty() && *opened_sso_url.borrow() != core.login.sso_url {
            let url = core.login.sso_url.clone();
            *opened_sso_url.borrow_mut() = url.clone();
            if let Err(error) = webbrowser::open(&url) {
                core.status = format!("Open this SSO URL in a browser: {url} ({error})");
            }
        }
        core.poll_verify();
        core.poll_send();
        core.poll_dm();
        core.poll_live_sync();
        core.poll_relations();
        core.pump_relations();
        core.poll_history();
        core.pump_history();
        loop {
            let page = core
                .page_rx
                .as_ref()
                .and_then(|receiver| receiver.try_recv().ok());
            let Some((room_id, result)) = page else { break };
            match result {
                Ok((rows, token)) => core.apply_page(room_id, rows, token),
                Err(error) => {
                    core.paginating.remove(&room_id);
                    core.status = error;
                }
            }
        }
        if let Some(result) = core
            .video_rx
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            match result {
                (mxc, Ok(path)) => {
                    core.video = None;
                    core.video_file = Some((mxc, path));
                }
                (_, Err(error)) => {
                    window.set_video_state(format!("Failed · {error}").into());
                }
            }
        }
        if let Some(event_id) = video_target.borrow().clone() {
            if let Some(image) = core
                .rows
                .iter()
                .find(|row| row.id == event_id)
                .and_then(|row| row.image.clone())
            {
                if core.video.is_none() {
                    if let Some(path) = core
                        .video_file
                        .as_ref()
                        .filter(|(mxc, _)| mxc == &image.mxc)
                        .map(|(_, path)| path.clone())
                    {
                        let size = (image.w > 0 && image.h > 0).then_some((image.w, image.h));
                        match crate::video::VideoPlayer::start(&path, playback_now(), size) {
                            Ok(player) => {
                                core.video = Some(player);
                                window.set_video_state("Playing".into());
                                window.set_video_playing(true);
                            }
                            Err(error) => {
                                window.set_video_state(format!("Failed · {error}").into());
                            }
                        }
                    }
                }
                if let Some(player) = core.video.as_mut() {
                    if let Some(frame) = player
                        .frame_image(playback_now())
                        .and_then(color_image_to_slint)
                    {
                        window.set_video_frame(frame);
                        window.set_video_has_frame(true);
                    }
                    window.set_video_playing(!player.is_paused() && !player.finished);
                    window.set_video_state(
                        if player.finished {
                            "Finished"
                        } else if player.is_paused() {
                            "Paused"
                        } else {
                            "Playing"
                        }
                        .into(),
                    );
                }
            }
        }
        let audio_ready: Vec<_> = core
            .audio_rx
            .as_ref()
            .map(|receiver| receiver.try_iter().collect())
            .unwrap_or_default();
        for (event_id, result) in audio_ready {
            core.audio_pending.remove(&event_id);
            match result {
                Ok(ready) => {
                    core.audio_errors.remove(&event_id);
                    core.audio_paths.insert(ready.mxc.clone(), ready.path);
                    if ready.duration_secs > 0.0 {
                        core.audio_lengths
                            .insert(event_id.clone(), ready.duration_secs);
                    }
                    if let Some(waveform) = ready.waveform {
                        core.audio_waves.insert(event_id, waveform);
                    }
                }
                Err(error) => {
                    core.audio_errors.insert(event_id, error);
                }
            }
        }
        let preview_urls: Vec<String> = core
            .rows
            .iter()
            .flat_map(|row| crate::embed::urls_in_text(&row.body))
            .filter(|url| {
                crate::embed::github_endpoint(url).is_some()
                    || crate::embed::card_rule(&core.embed_rules, url).is_some()
            })
            .collect();
        for url in preview_urls {
            core.fetch_embed(url);
        }
        core.ensure_verify_watch(&ctx);
        core.ensure_live_sync(&ctx);
        if let Some((room_id, result)) = core.pinned_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            if core.pinned_room.as_deref() == Some(room_id.as_str()) {
                core.pinned_loading = false;
                match result {
                    Ok(pins) => core.pinned_messages = pins,
                    Err(error) => core.pinned_error = Some(error),
                }
            }
            core.pinned_rx = None;
        }
        for mxc in core
            .rooms
            .iter()
            .filter_map(|room| room.avatar_mxc.as_ref())
            .chain(core.rows.iter().filter_map(|row| row.avatar_mxc.as_ref()))
            .chain(
                core.members
                    .iter()
                    .filter_map(|member| member.avatar_mxc.as_ref()),
            )
            .chain(
                core.pinned_messages
                    .iter()
                    .filter_map(|pin| pin.avatar_mxc.as_ref()),
            )
        {
            media.request(&core, mxc);
        }
        for image in core.rows.iter().filter_map(|row| row.image.as_ref()) {
            media.request_source(&core, &image.mxc, image.source.clone(), (640, 480));
        }
        let rooms: Vec<RoomView> = room_list(
            &core.rooms,
            window.get_room_filter().as_str(),
            core.room_sort.as_str(),
            core.group_rooms,
            &collapsed_sections.borrow(),
        )
        .into_iter()
        .map(|item| match item {
            ListItem::Header {
                section,
                unread,
                collapsed,
            } => RoomView {
                source_index: -1,
                header: true,
                collapsed,
                name: section.into(),
                unread: unread.min(i32::MAX as usize) as i32,
                ..Default::default()
            },
            ListItem::Room(index) => {
                let room = &core.rooms[index];
                RoomView {
                    source_index: index.min(i32::MAX as usize) as i32,
                    avatar: media.get(room.avatar_mxc.as_ref()).0,
                    has_avatar: media.get(room.avatar_mxc.as_ref()).1,
                    initial: initial(&room.name),
                    id: room.room_id.clone().into(),
                    name: room.name.clone().into(),
                    preview: compact_preview(room.preview.as_deref().unwrap_or_default()).into(),
                    unread: room.unread.min(i32::MAX as usize) as i32,
                    mentioned: room.mentioned,
                    selected: index == core.current,
                    is_dm: room.is_dm,
                    typing: core.typing.contains_key(&room.room_id),
                    notify: match room.notify {
                        Some(super::RoomNotify::All) => "all",
                        Some(super::RoomNotify::MentionsOnly) => "mentions",
                        Some(super::RoomNotify::Mute) => "mute",
                        None => "",
                    }
                    .into(),
                    ..Default::default()
                }
            }
        })
        .collect();
        let audio_playback = core.audio.as_mut().map(|(event_id, player)| {
            let position = player.position(playback_now());
            let duration = player.duration;
            (
                event_id.clone(),
                !player.is_paused() && !player.finished,
                if duration > 0.0 {
                    (position / duration).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                },
            )
        });
        // Divider label on the first stamped row of each local day.
        let today = chrono::Local::now().date_naive();
        let mut last_day = None;
        let day_dividers: Vec<String> = core
            .rows
            .iter()
            .map(|row| match super::text::local_day(row.origin_server_ts) {
                Some(day) if last_day != Some(day) => {
                    last_day = Some(day);
                    super::text::day_label(day, today)
                }
                _ => String::new(),
            })
            .collect();
        // First message after the fully-read marker, unless it is our own.
        let first_unread = core
            .unread_marker
            .as_ref()
            .and_then(|marker| core.rows.iter().position(|row| &row.id == marker))
            .map(|marker| marker + 1)
            .filter(|&next| {
                core.rows
                    .get(next)
                    .is_some_and(|row| row.sender != core.own_user)
            });
        window.set_first_unread_id(
            first_unread
                .map(|index| core.rows[index].id.clone())
                .unwrap_or_default()
                .into(),
        );
        let messages: Vec<MessageView> = core
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let reply_target = row
                    .reply_to_id
                    .as_ref()
                    .and_then(|id| core.rows.iter().find(|target| &target.id == id));
                let reply_sender = reply_target
                    .map(|target| target.display_name.clone())
                    .or_else(|| row.reply_to.as_ref().map(|(name, _)| name.clone()))
                    .unwrap_or_default();
                let reply_body = reply_target
                    .map(|target| target.body.clone())
                    .or_else(|| row.reply_to.as_ref().map(|(_, body)| body.clone()))
                    .unwrap_or_default();
                let (reply_avatar, reply_has_avatar) =
                    media.get(reply_target.and_then(|target| target.avatar_mxc.as_ref()));
                let standalone_shortcode = row.body.trim();
                let custom_emoji_mxc = (standalone_shortcode.len() >= 3
                    && standalone_shortcode.starts_with(':')
                    && standalone_shortcode.ends_with(':')
                    && standalone_shortcode[1..standalone_shortcode.len().saturating_sub(1)]
                        .chars()
                        .all(|character| {
                            character.is_alphanumeric() || matches!(character, '_' | '-' | '.')
                        }))
                .then(|| core.packs.resolve(standalone_shortcode))
                .flatten()
                .map(|item| item.mxc_url.clone());
                if let Some(mxc) = custom_emoji_mxc.as_ref() {
                    media.request(&core, mxc);
                }
                let (custom_emoji, has_custom_emoji) = media.get(custom_emoji_mxc.as_ref());
                let embeds: Vec<EmbedView> = crate::embed::urls_in_text(&row.body)
                    .into_iter()
                    .filter_map(|url| core.embeds.get(&url))
                    .map(|embed| {
                        for source in embed.avatar.iter().chain(embed.image.iter()) {
                            media.request_http(&core, source);
                        }
                        let avatar_key = embed.avatar.as_ref().map(|url| format!("http:{url}"));
                        let media_key = embed.image.as_ref().map(|url| format!("http:{url}"));
                        let (avatar, has_avatar) = media.get(avatar_key.as_ref());
                        let (card_media, has_media) = media.get(media_key.as_ref());
                        EmbedView {
                            author: embed.author.clone().into(),
                            handle: embed.handle.clone().into(),
                            text: embed.text.clone().into(),
                            link: embed.link.clone().into(),
                            avatar,
                            has_avatar,
                            media: card_media,
                            has_media,
                        }
                    })
                    .collect();
                // Like Element, your own receipt is not shown.
                let readers: Vec<&String> = row
                    .seen_by
                    .iter()
                    .filter(|mxid| **mxid != core.own_user)
                    .collect();
                let receipts: Vec<ReceiptView> = readers
                    .iter()
                    .copied()
                    .take(5)
                    .map(|mxid| {
                        let (name, avatar_mxc, _) = core.profile_of(mxid);
                        if let Some(mxc) = avatar_mxc.as_ref() {
                            media.request(&core, mxc);
                        }
                        let (avatar, has_avatar) = media.get(avatar_mxc.as_ref());
                        ReceiptView {
                            initial: initial(&name),
                            name: name.into(),
                            avatar,
                            has_avatar,
                        }
                    })
                    .collect();
                let old_message = messages_model
                    .row_data(index)
                    .filter(|old| old.id.as_str() == row.id.as_str());
                let new_blocks = message_blocks(row, &|mxid| {
                    let color = core.nick_color(mxid);
                    format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
                });
                // Widest fixed content (image rects + longest code line), padding
                // included, so the Slint bubble can size to it instead of only to
                // the plain-text measure. Audio/embeds are sized Slint-side.
                let mut content_width: i32 = 0;
                if row.is_sticker {
                    content_width = content_width.max(140);
                } else if row.image.is_some() {
                    content_width = content_width.max(320);
                }
                for block in new_blocks.iter().filter(|b| b.kind.as_str() == "code") {
                    let mut longest = 0usize;
                    for i in 0..block.lines.row_count() {
                        if let Some(line) = block.lines.row_data(i) {
                            let mut len = 0usize;
                            for j in 0..line.tokens.row_count() {
                                if let Some(token) = line.tokens.row_data(j) {
                                    len += token.text.chars().count();
                                }
                            }
                            longest = longest.max(len);
                        }
                    }
                    content_width =
                        content_width.max((longest.min(120) as i32) * 7 + 44);
                }
                let blocks_model = reuse_or_new(
                    old_message.as_ref().map(|old| old.blocks.clone()),
                    new_blocks,
                );
                let embeds_model = reuse_or_new(
                    old_message.as_ref().map(|old| old.embeds.clone()),
                    embeds,
                );
                let receipts_model = reuse_or_new(
                    old_message.as_ref().map(|old| old.receipts.clone()),
                    receipts,
                );
                let old_reactions = old_message.as_ref().map(|old| old.reactions.clone());
                MessageView {
                    sender_id: row.sender.clone().into(),
                    avatar: media.get(row.avatar_mxc.as_ref()).0,
                    has_avatar: media.get(row.avatar_mxc.as_ref()).1,
                    initial: initial(&row.display_name),
                    id: row.id.clone().into(),
                    sender: row.display_name.clone().into(),
                    body: row.body.clone().into(),
                    blocks: blocks_model,
                    time: stamp(row.origin_server_ts, &row.ts, core.twelve_hour_clock()).into(),
                    grouped: !row.edited
                        && !row.is_sticker
                        && day_dividers[index].is_empty()
                        && index > 0
                        && super::text::continues_group(&core.rows[index - 1], row),
                    sender_color: egui_to_slint(core.nick_color(&row.sender)),
                    day_divider: day_dividers[index].clone().into(),
                    unread_marker: first_unread == Some(index),
                    own: row.sender == core.own_user,
                    edited: row.edited,
                    sticker: row.is_sticker,
                    reply: row
                        .reply_to_id
                        .as_ref()
                        .and_then(|id| core.rows.iter().find(|target| &target.id == id))
                        .map(|target| {
                            format!("Replying to {} · {}", target.display_name, target.body)
                        })
                        .or_else(|| {
                            row.reply_to
                                .as_ref()
                                .map(|(name, body)| format!("Replying to {name} · {body}"))
                        })
                        .unwrap_or_default()
                        .into(),
                    reply_sender: reply_sender.into(),
                    reply_body: reply_body.into(),
                    reply_avatar,
                    reply_has_avatar,
                    embeds: embeds_model,
                    receipts: receipts_model,
                    seen_by: seen_by_text(
                        &readers
                            .iter()
                            .map(|mxid| core.profile_of(mxid).0)
                            .collect::<Vec<_>>(),
                    )
                    .into(),
                    reply_id: row.reply_to_id.clone().unwrap_or_default().into(),
                    reactions: {
                        let mut reaction_views = Vec::with_capacity(row.reactions.len());
                        for reaction in row.reactions.iter() {
                                let custom_mxc = if reaction.key.starts_with("mxc://") {
                                    Some(reaction.key.clone())
                                } else if reaction.key.starts_with(':') {
                                    core.packs
                                        .resolve(&reaction.key)
                                        .map(|item| item.mxc_url.clone())
                                } else {
                                    None
                                };
                                let image_key = custom_mxc
                                    .clone()
                                    .unwrap_or_else(|| format!("unicode:{}", reaction.key));
                                if let Some(mxc) = custom_mxc {
                                    media.request(&core, &mxc);
                                } else if !media.ready.contains_key(&image_key) {
                                    if let Some(image) = core
                                        .emoji_font
                                        .decode(&reaction.key)
                                        .and_then(color_image_to_slint)
                                    {
                                        media.ready.insert(image_key.clone(), image);
                                    }
                                }
                                let (image, has_image) = media.get(Some(&image_key));
                                let mut reactor_names = reaction
                                    .senders
                                    .iter()
                                    .map(|sender| core.profile_of(&sender.user).0)
                                    .collect::<Vec<_>>();
                                let unlisted = reaction.count().saturating_sub(reactor_names.len());
                                if unlisted > 0 {
                                    reactor_names.push(format!(
                                        "{unlisted} other{}",
                                        if unlisted == 1 { "" } else { "s" }
                                    ));
                                }
                                let reactors = reactor_names.join(", ");
                                let reactor_users = reaction
                                    .senders
                                    .iter()
                                    .take(6)
                                    .map(|sender| {
                                        let (name, avatar_mxc, _) = core.profile_of(&sender.user);
                                        if let Some(mxc) = avatar_mxc.as_ref() {
                                            media.request(&core, mxc);
                                        }
                                        let (avatar, has_avatar) = media.get(avatar_mxc.as_ref());
                                        ReceiptView {
                                            initial: initial(&name),
                                            name: name.into(),
                                            avatar,
                                            has_avatar,
                                        }
                                    })
                                    .collect::<Vec<_>>();
                                let display = if reaction.key.starts_with("mxc://") {
                                    core.packs
                                        .packs()
                                        .iter()
                                        .flat_map(|pack| &pack.images)
                                        .find(|item| item.mxc_url == reaction.key)
                                        .map(|item| format!(":{}:", item.shortcode))
                                        .unwrap_or_else(|| "custom emoji".into())
                                } else {
                                    reaction.key.clone()
                                };
                                let old_users = old_reactions.as_ref().and_then(|model| {
                                    (0..model.row_count()).find_map(|i| {
                                        let existing = model.row_data(i)?;
                                        if existing.key.as_str() == reaction.key.as_str() {
                                            Some(existing.reactor_users.clone())
                                        } else {
                                            None
                                        }
                                    })
                                });
                                let users_model = reuse_or_new(old_users, reactor_users);
                                reaction_views.push(ReactionView {
                                    key: reaction.key.clone().into(),
                                    display: display.into(),
                                    count: reaction.count().min(i32::MAX as usize) as i32,
                                    reactors: reactors.into(),
                                    reactor_users: users_model,
                                    image,
                                    has_image,
                                    owned: reaction.owns(&core.own_user),
                                });
                            }
                        reuse_or_new(old_reactions, reaction_views)
                    },
                    media: media.get(row.image.as_ref().map(|image| &image.mxc)).0,
                    has_media: media.get(row.image.as_ref().map(|image| &image.mxc)).1,
                    media_label: if let Some(audio) = &row.audio {
                        if audio.is_voice {
                            "Voice message".into()
                        } else {
                            audio.name.clone()
                        }
                    } else if let Some(image) = row.image.as_ref() {
                        image.name.clone()
                    } else {
                        String::new()
                    }
                    .into(),
                    media_kind: if row.audio.is_some() {
                        "audio"
                    } else if row.image.as_ref().is_some_and(|image| image.is_video) {
                        "video"
                    } else if row.image.is_some() {
                        "image"
                    } else {
                        ""
                    }
                    .into(),
                    media_state: if row.audio.is_some() {
                        if let Some(error) = core.audio_errors.get(&row.id) {
                            format!("Failed · {error}")
                        } else if core.audio_pending.contains(&row.id) {
                            "Loading…".into()
                        } else if let Some((_, playing, _)) =
                            audio_playback.as_ref().filter(|(id, _, _)| id == &row.id)
                        {
                            if *playing {
                                "Playing".into()
                            } else {
                                "Paused".into()
                            }
                        } else if row
                            .audio
                            .as_ref()
                            .is_some_and(|audio| core.audio_paths.contains_key(&audio.mxc))
                        {
                            "Ready".into()
                        } else {
                            "Tap to load".into()
                        }
                    } else {
                        String::new()
                    }
                    .into(),
                    media_playing: audio_playback
                        .as_ref()
                        .is_some_and(|(id, playing, _)| id == &row.id && *playing),
                    media_progress: audio_playback
                        .as_ref()
                        .filter(|(id, _, _)| id == &row.id)
                        .map(|(_, _, progress)| *progress)
                        .unwrap_or_default(),
                    custom_emoji,
                    has_custom_emoji,
                    custom_emoji_name: standalone_shortcode.into(),
                    content_width,
                }
            })
            .collect();
        let member_query = window.get_member_query().trim().to_lowercase();
        let mut listed: Vec<&super::Member> = core
            .members
            .iter()
            .filter(|member| {
                member_query.is_empty()
                    || member.display.to_lowercase().contains(&member_query)
                    || member.mxid.to_lowercase().contains(&member_query)
            })
            .collect();
        listed.sort_by(|a, b| {
            b.power
                .cmp(&a.power)
                .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
        });
        let members: Vec<MemberView> = listed
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let group = member_group(member.power);
                MemberView {
                    avatar: media.get(member.avatar_mxc.as_ref()).0,
                    has_avatar: media.get(member.avatar_mxc.as_ref()).1,
                    initial: initial(&member.display),
                    name: member.display.clone().into(),
                    id: member.mxid.clone().into(),
                    presence: core.presence_of(member).unwrap_or_default().into(),
                    group: group.into(),
                    group_start: index == 0 || member_group(listed[index - 1].power) != group,
                }
            })
            .collect();
        let mention_members: Vec<MemberView> =
            super::text::mention_prefix(window.get_composer_text().as_str())
                .map(|prefix| core.mention_hits(prefix))
                .unwrap_or_default()
                .into_iter()
                .map(|member| MemberView {
                    avatar: media.get(member.avatar_mxc.as_ref()).0,
                    has_avatar: media.get(member.avatar_mxc.as_ref()).1,
                    initial: initial(&member.display),
                    name: member.display.into(),
                    id: member.mxid.into(),
                    ..Default::default()
                })
                .collect();
        let pins: Vec<PinView> = core
            .pinned_messages
            .iter()
            .map(|pin| {
                if let Some(image) = pin.timeline_row.as_ref().and_then(|row| row.image.as_ref()) {
                    media.request_source(&core, &image.mxc, image.source.clone(), (640, 480));
                }
                let row = pin.timeline_row.as_ref();
                let image = row.and_then(|row| row.image.as_ref());
                let (pin_media, pin_has_media) = media.get(image.map(|image| &image.mxc));
                PinView {
                    avatar: media.get(pin.avatar_mxc.as_ref()).0,
                    has_avatar: media.get(pin.avatar_mxc.as_ref()).1,
                    initial: initial(&pin.display_name),
                    id: pin.id.clone().into(),
                    sender: pin.display_name.clone().into(),
                    body: pin.body.clone().into(),
                    time: stamp(pin.origin_server_ts, &pin.ts, core.twelve_hour_clock()).into(),
                    media: pin_media,
                    has_media: pin_has_media,
                    media_kind: if row.is_some_and(|row| row.audio.is_some()) {
                        "audio"
                    } else if image.is_some_and(|image| image.is_video) {
                        "video"
                    } else if image.is_some() {
                        "image"
                    } else {
                        ""
                    }
                    .into(),
                    media_label: row
                        .and_then(|row| {
                            row.audio
                                .as_ref()
                                .map(|audio| audio.name.clone())
                                .or_else(|| row.image.as_ref().map(|image| image.name.clone()))
                        })
                        .unwrap_or_default()
                        .into(),
                }
            })
            .collect();
        reconcile(&rooms_model, rooms);
        reconcile(&messages_model, messages);
        reconcile(&members_model, members);
        window.set_member_total(core.members.len().min(i32::MAX as usize) as i32);
        reconcile(&mention_model, mention_members);
        reconcile(&pins_model, pins);
        window.set_pins_loading(core.pinned_loading);
        window.set_pins_error(core.pinned_error.clone().unwrap_or_default().into());
        let emoji = emoji_views(
            &core,
            &mut media,
            window.get_emoji_query().as_str(),
            window.get_picker_tab().as_str(),
            window.get_emoji_category().as_str(),
        );
        reconcile(&emoji_model, emoji);
        let pack_views: Vec<PackView> = core
            .packs
            .packs()
            .iter()
            .map(|pack| {
                let images: Vec<EmojiView> = pack
                    .images
                    .iter()
                    .map(|item| {
                        media.request(&core, &item.mxc_url);
                        let (image, has_image) = media.get(Some(&item.mxc_url));
                        EmojiView {
                            value: format!(":{}:", item.shortcode).into(),
                            name: item.shortcode.clone().into(),
                            pack: pack.display_name.clone().into(),
                            image,
                            has_image,
                            custom: true,
                            sticker: item.is_sticker,
                        }
                    })
                    .collect();
                PackView {
                    name: pack.display_name.clone().into(),
                    source: pack
                        .address
                        .as_ref()
                        .map(|address| address.room_id.clone())
                        .unwrap_or_else(|| "Account".into())
                        .into(),
                    count: pack.images.len().min(i32::MAX as usize) as i32,
                    images: ModelRc::new(VecModel::from(images)),
                }
            })
            .collect();
        reconcile(&packs_model, pack_views);
        let categories = std::iter::once(("Frequently used", "🕘"))
            .chain(
                crate::emoji::CATEGORIES
                    .iter()
                    .map(|category| (category.name, category.icon)),
            )
            .map(|(name, icon)| {
                let key = format!("unicode:{icon}");
                if !media.ready.contains_key(&key) {
                    if let Some(image) = core.emoji_font.decode(icon).and_then(color_image_to_slint)
                    {
                        media.ready.insert(key.clone(), image);
                    }
                }
                let (image, has_image) = media.get(Some(&key));
                EmojiCategoryView {
                    name: name.into(),
                    symbol: icon.into(),
                    image,
                    has_image,
                }
            })
            .collect();
        reconcile(&emoji_categories_model, categories);
        let tones = super::SKIN_TONES
            .iter()
            .map(|(modifier, label)| {
                let symbol = format!("✋{modifier}");
                let key = format!("unicode:{symbol}");
                if !media.ready.contains_key(&key) {
                    if let Some(image) = core
                        .emoji_font
                        .decode(&symbol)
                        .and_then(color_image_to_slint)
                    {
                        media.ready.insert(key.clone(), image);
                    }
                }
                let (image, has_image) = media.get(Some(&key));
                EmojiView {
                    value: (*modifier).into(),
                    name: (*label).into(),
                    pack: "Skin tone".into(),
                    image,
                    has_image,
                    custom: false,
                    sticker: false,
                }
            })
            .collect();
        reconcile(&skin_tones_model, tones);
        let uploads = core
            .pending_uploads
            .iter()
            .map(|upload| UploadView {
                name: upload.name.clone().into(),
                mime: upload.mime.clone().into(),
                size: if upload.bytes.len() >= 1024 * 1024 {
                    format!("{:.1} MB", upload.bytes.len() as f64 / (1024.0 * 1024.0))
                } else {
                    format!("{} KB", (upload.bytes.len() + 1023) / 1024)
                }
                .into(),
            })
            .collect();
        reconcile(&uploads_model, uploads);
        let composer_text = window.get_composer_text();
        let command_results: Vec<CommandView> = composer_text
            .strip_prefix('/')
            .map(|rest| rest.split_whitespace().next().unwrap_or("").to_lowercase())
            .filter(|verb| {
                !super::SLASH_COMMANDS
                    .iter()
                    .any(|(command, _)| command[1..] == *verb)
            })
            .map(|verb| {
                super::SLASH_COMMANDS
                    .iter()
                    .filter(|(command, _)| command[1..].starts_with(&verb))
                    .take(8)
                    .map(|(command, description)| CommandView {
                        command: (*command).into(),
                        description: (*description).into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        reconcile(&commands_model, command_results);
        reconcile(
            &ignored_model,
            core.ignored_users.iter().cloned().map(Into::into).collect(),
        );
        let rules = core
            .embed_rules
            .iter()
            .map(|rule| EmbedRuleView {
                name: rule.name.clone().into(),
                enabled: rule.enabled,
                hosts: rule.hosts.join(", ").into(),
                open_with: rule.open_with.clone().into(),
                api: rule.api.clone().into(),
            })
            .collect();
        reconcile(&embed_model, rules);
        let devices: Vec<DeviceView> = core
            .devices
            .iter()
            .map(|device| DeviceView {
                user_id: device.user_id.clone().into(),
                device_id: device.device_id.clone().into(),
                name: device
                    .display_name
                    .clone()
                    .unwrap_or_else(|| device.device_id.clone())
                    .into(),
                verified: device.verified,
                own: device.is_own,
            })
            .collect();
        reconcile(&devices_model, devices);
        let sas_items: Vec<SasEmojiView> = core
            .sas
            .as_ref()
            .map(|sas| {
                sas.emojis
                    .iter()
                    .map(|(symbol, description)| {
                        let key = format!("unicode:{symbol}");
                        if !media.ready.contains_key(&key) {
                            if let Some(image) = core
                                .emoji_font
                                .decode(symbol)
                                .and_then(color_image_to_slint)
                            {
                                media.ready.insert(key.clone(), image);
                            }
                        }
                        let (image, has_image) = media.get(Some(&key));
                        SasEmojiView {
                            symbol: symbol.clone().into(),
                            description: description.clone().into(),
                            image,
                            has_image,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        reconcile(&sas_model, sas_items);
        window.set_logged_in(core.client.is_some());
        window.set_busy(core.login.busy);
        window.set_login_error(core.login.error.clone().into());
        window.set_status(core.status.clone().into());
        window.set_ui_font_size(core.settings_font_size);
        window.global::<Theme>().set_chat_body(core.settings_font_size);
        window.set_show_room_previews(core.show_previews);
        window.set_message_layout(core.message_layout.clone().into());
        window.set_time_format(core.time_format.clone().into());
        window.set_room_sort(core.room_sort.clone().into());
        window.set_group_rooms(core.group_rooms);
        window.set_own_bubble_choice(core.own_bubble_color.clone().into());
        window.set_other_bubble_choice(core.other_bubble_color.clone().into());
        window.set_own_bubble_custom(slint_color(Some(&core.own_bubble_color), (0, 0, 0)));
        window.set_other_bubble_custom(slint_color(Some(&core.other_bubble_color), (0, 0, 0)));
        if window.get_display_name().is_empty() {
            window.set_display_name(core.settings_display_name.clone().into());
        }
        window.set_account_id(
            core.client
                .as_ref()
                .and_then(|client| client.user_id())
                .map(ToString::to_string)
                .unwrap_or_default()
                .into(),
        );
        let account_id = core
            .client
            .as_ref()
            .and_then(|client| client.user_id())
            .map(ToString::to_string)
            .unwrap_or_default();
        window.set_account_server(
            account_id
                .split_once(':')
                .map(|(_, server)| server)
                .unwrap_or_default()
                .into(),
        );
        let (_, own_avatar_mxc, _) = core.profile_of(&account_id);
        window.set_account_initial(initial(&core.display_for(&account_id)));
        if let Some(mxc) = own_avatar_mxc.as_ref() {
            media.request(&core, mxc);
        }
        let (account_avatar, account_has_avatar) = media.get(own_avatar_mxc.as_ref());
        window.set_account_avatar(account_avatar);
        window.set_account_has_avatar(account_has_avatar);
        if core.verify_user_input.trim().is_empty() || core.verify_user_input == account_id {
            own_unverified.set(
                core.devices
                    .iter()
                    .any(|device| device.is_own && !device.verified),
            );
        }
        if core.client.is_none() {
            own_unverified.set(false);
        }
        window.set_session_unverified(own_unverified.get());
        window.set_sas_ready(core.sas.is_some());
        window.set_sas_decimals(
            core.sas
                .as_ref()
                .map(|sas| {
                    format!(
                        "{:03}  ·  {:03}  ·  {:03}",
                        sas.decimals.0, sas.decimals.1, sas.decimals.2
                    )
                })
                .unwrap_or_default()
                .into(),
        );
        window.set_incoming_verification(core.incoming.is_some());
        window.set_incoming_verification_user(
            core.incoming
                .as_ref()
                .map(|request| request.user_id.clone())
                .unwrap_or_default()
                .into(),
        );
        let composer_context = if let Some(id) = &core.editing {
            core.rows
                .iter()
                .find(|row| &row.id == id)
                .map(|row| format!("Editing {}", row.body))
                .unwrap_or_else(|| "Editing message".into())
        } else if let Some(id) = &core.replying_to {
            core.rows
                .iter()
                .find(|row| &row.id == id)
                .map(|row| format!("Replying to {}: {}", row.display_name, row.body))
                .unwrap_or_else(|| "Replying to message".into())
        } else {
            String::new()
        };
        window.set_composer_context(composer_context.into());
        window.set_current_dm_peer("".into());
        window.set_current_room_pin_count(0);
        if let Some(room) = core.rooms.get(core.current) {
            window.set_room_title(room.name.clone().into());
            window.set_current_room_id(room.room_id.clone().into());
            window.set_current_room_is_dm(room.is_dm);
            if room.is_dm {
                if let Some(peer) = core
                    .members
                    .iter()
                    .find(|member| member.mxid != core.own_user)
                {
                    window.set_current_dm_peer(peer.mxid.clone().into());
                }
            }
            if let Some(count) = core
                .client
                .as_ref()
                .and_then(|client| {
                    matrix_sdk::ruma::OwnedRoomId::try_from(room.room_id.as_str())
                        .ok()
                        .and_then(|id| client.get_room(&id))
                })
                .and_then(|room| room.pinned_event_ids())
                .map(|ids| ids.len().min(i32::MAX as usize) as i32)
            {
                window.set_current_room_pin_count(count);
            }
            let (avatar, has_avatar) = media.get(room.avatar_mxc.as_ref());
            window.set_current_room_avatar(avatar);
            window.set_current_room_has_avatar(has_avatar);
            window.set_current_room_initial(initial(&room.name));
            window.set_current_room_topic(room.meta.topic.clone().unwrap_or_default().into());
            let typing: Vec<String> = core
                .typing
                .get(&room.room_id)
                .map(|users| users.iter().map(|user| core.profile_of(user).0).collect())
                .unwrap_or_default();
            window.set_typing_text(typing_text(&typing).into());
        }
        if let Some(room_id) = core.current_room_id() {
            let loaded = core.history_queue.is_loaded(&room_id);
            let failed = core.history_queue.has_failed(&room_id);
            window.set_history_loading(!loaded && !failed);
            window.set_history_failed(failed);
            window.set_history_paginating(core.paginating.contains(&room_id));
            window.set_history_start(loaded && !core.back_tokens.contains_key(&room_id));
        } else {
            window.set_history_loading(false);
            window.set_history_paginating(false);
            window.set_history_start(false);
            window.set_history_failed(false);
        }
        if window.get_show_pins() && core.pinned_room.is_none() && core.client.is_some() {
            core.open_pins();
        } else if !window.get_show_pins() && core.pinned_room.is_some() {
            core.pinned_room = None;
        }
    });
    window.run()
}
