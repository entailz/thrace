/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Sidebar | timeline | members. Chrome is ascii/unicode only; emoji live in picker + content.

use crate::matrix::PackStore;
use crate::theme::{self, ThemeFile};
use std::future::Future;
use std::sync::Arc;

/// Parallel media downloads; fills a screen of avatars without per-image connections.
const MEDIA_CONCURRENCY: usize = 6;
/// Texture uploads per frame; caps bursts that would stutter.
const MEDIA_UPLOADS_PER_FRAME: usize = 8;

/// Image attachment (`m.image` / `m.sticker` via mxc).
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub mxc: String,
    pub source: matrix_sdk::ruma::events::room::MediaSource,
    pub thumbnail_source: Option<matrix_sdk::ruma::events::room::MediaSource>,
    pub name: String,
    pub w: u32,
    pub h: u32,
    /// Video: renders as poster with play overlay; click opens player.
    pub is_video: bool,
    /// Clip length in ms, when reported.
    pub duration_ms: Option<u64>,
}

/// Timeline row with reply/thread support.
#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub id: String,
    pub ts: String,
    pub sender: String,
    pub display_name: String,
    pub body: String,
    /// Raw `formatted_body` HTML, if present.
    pub formatted: Option<String>,
    /// Sender avatar mxc from member event, if set.
    pub avatar_mxc: Option<String>,
    /// (sender display, snippet) from the plain-text reply fallback.
    pub reply_to: Option<(String, String)>,
    /// Replied-to event id; resolves the live target row, falling back to `reply_to`.
    pub reply_to_id: Option<String>,
    pub thread_count: usize,
    pub image: Option<ImageAttachment>,
    /// Grouped reactions: key + senders. Redaction names `Reactor.event_id`.
    pub reactions: Vec<Reaction>,
    pub is_sticker: bool,
    /// Client txn id for our sends; sync echoes match on `unsigned.transaction_id`.
    pub txn_id: Option<String>,
    /// True once an `m.replace` rewrote this message.
    pub edited: bool,
    /// Mxids whose latest `m.read` points here; avatar dots under the message.
    pub seen_by: Vec<String>,
}

/// One reaction group: emoji key + sender mxids.
#[derive(Debug, Clone)]
pub struct Reaction {
    pub key: String,
    pub senders: Vec<Reactor>,
}

/// One person's reaction: who, and the event to redact to take it back.
/// `event_id` is empty for a local echo until sync confirms it.
#[derive(Debug, Clone)]
pub struct Reactor {
    pub user: String,
    pub event_id: String,
}

impl Reaction {
    pub fn count(&self) -> usize {
        self.senders.len()
    }
    pub fn owns(&self, user: &str) -> bool {
        self.senders.iter().any(|s| s.user == user)
    }
    /// The event id of `user`'s reaction, if we know it yet.
    pub fn event_of(&self, user: &str) -> Option<&str> {
        self.senders
            .iter()
            .find(|s| s.user == user)
            .map(|s| s.event_id.as_str())
            .filter(|id| !id.is_empty())
    }
}

/// Outgoing send result (worker → UI): status text to show.
#[derive(Debug)]
enum SendResult {
    Done(String),
    Failed(String),
}

/// App state.
pub struct ThraceApp {
    theme: ThemeFile,
    rooms: Vec<RoomEntry>,
    current: usize,
    /// Timeline rows behind `Rc` so render can hold them across `&mut self` calls.
    rows: std::rc::Rc<Vec<TimelineRow>>,
    /// Per-room timelines.
    timelines: std::collections::HashMap<String, Vec<TimelineRow>>,
    history_queue: crate::history_queue::HistoryQueue,
    history_tasks: std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
    history_rx: Option<std::sync::mpsc::Receiver<InitialHistory>>,
    history_tx: Option<std::sync::mpsc::Sender<InitialHistory>>,
    /// Scrollback token per room; missing means history start reached.
    back_tokens: std::collections::HashMap<String, String>,
    /// Room fetching older messages; guards one request at a time.
    paginating: Option<String>,
    /// Older pages arriving from the worker.
    page_rx: Option<std::sync::mpsc::Receiver<HistoryPage>>,
    page_tx: Option<std::sync::mpsc::Sender<HistoryPage>>,
    /// Height before a prepend; holds scroll steady on insert above.
    pending_scroll_fix: Option<f32>,
    /// Timeline content height last frame.
    last_content_height: f32,
    /// Set by jump button; consumed inside the timeline scroll area.
    jump_to_latest: bool,
    /// Jump animation deadline; keeps frames requested while running.
    jump_until: f64,
    members: Vec<Member>,
    /// Per-room member lists.
    members_by_room: std::collections::HashMap<String, Vec<Member>>,
    input: String,
    replying_to: Option<String>,
    /// Event id of the message being edited (Up arrow on an empty composer).
    editing: Option<String>,
    /// Staged-upload thumbs by file name; `None` means nothing to show, don't retry.
    upload_thumbs: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    /// Files queued for upload; shown as staging strip above input.
    pending_uploads: Vec<PendingUpload>,
    show_picker: bool,
    /// Frame the emoji picker was opened on — see `react_target`.
    picker_opened_frame: u64,
    picker_tab: PickerTab,
    picker_query: String,
    /// Skin-tone modifier for emoji ("", light..dark).
    skin_tone: String,
    /// Recently picked emoji, most recent first.
    recent_emoji: Vec<String>,
    emoji_usage: crate::recent_emoji::RecentEmoji,
    emoji_save_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::recent_emoji::RecentEmoji>>,
    emoji_save_task: Option<tokio::task::JoinHandle<()>>,
    /// Category the tab bar asked to jump to, consumed on the next frame.
    scroll_to_category: Option<usize>,
    /// Event to scroll to — set by clicking a reply's "in reply to" line.
    scroll_to_event: Option<String>,
    /// (event, deadline) flash marking a jump landing.
    highlight_event: Option<(String, f64)>,
    /// React target: (event id, anchor, opened frame). Keyed by id so sync batches can't drift it.
    react_target: Option<(String, egui::Pos2, u64)>,
    slash_selected: usize,
    /// Highlighted row in the @-mention dropdown.
    mention_selected: usize,
    packs: PackStore,
    /// mxc → texture cache (avatars, timeline images, custom emoji).
    media: crate::media_cache::MediaCache,
    /// Colour emoji, decoded from the CBDT bitmaps in NotoColorEmoji.
    emoji_font: crate::emoji_font::EmojiFont,
    /// Bytes from worker downloads, ingested each frame.
    media_rx: Option<std::sync::mpsc::Receiver<crate::media_cache::MediaBytes>>,
    media_tx: Option<std::sync::mpsc::Sender<crate::media_cache::MediaBytes>>,
    /// Fetch requests (UI → worker); non-blocking.
    media_req_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::media_cache::MediaFetch>>,
    /// Image or video opened from a timeline thumbnail.
    image_preview: Option<ImageAttachment>,
    /// Running video playback, if the preview is a video.
    video: Option<crate::video::VideoPlayer>,
    /// Temp file the video is being played from, plus the mxc it came from.
    video_file: Option<(String, std::path::PathBuf)>,
    /// Downloaded video files arriving from the worker.
    video_rx: Option<std::sync::mpsc::Receiver<(String, Result<std::path::PathBuf, String>)>>,
    video_tx: Option<std::sync::mpsc::Sender<(String, Result<std::path::PathBuf, String>)>>,
    /// Sidebar room menu: (room id, anchor, frame it opened on).
    room_menu: Option<(String, egui::Pos2, u64)>,
    /// Rooms we have tagged as favourites, for the menu's toggle state.
    favourites: std::collections::HashSet<String>,
    /// Profile card: (mxid, anchor). Opened by clicking an avatar or name in
    /// the timeline or the member list.
    profile_target: Option<(String, egui::Pos2, u64)>,
    /// Pending outgoing sends (echoed locally, confirmed on Sent).
    send_rx: Option<std::sync::mpsc::Receiver<SendResult>>,
    send_tx: Option<std::sync::mpsc::Sender<SendResult>>,
    /// Live sync: fresh timeline events (worker → UI), drained every frame.
    sync_rx: Option<std::sync::mpsc::Receiver<SyncBatch>>,
    /// Live sync runs once per login; re-login spawns a fresh loop.
    sync_running: bool,
    /// Live-sync task; aborted on logout/re-login so loops never overlap.
    sync_task: Option<tokio::task::JoinHandle<()>>,
    /// Last read receipt sent; stops re-sending per sync batch.
    last_receipt: Option<String>,
    /// Own mxid: mention detection, skip self unread badges.
    own_user: String,
    status: String,
    login: LoginForm,
    client: Option<matrix_sdk::Client>,
    login_rx: Option<std::sync::mpsc::Receiver<LoginMsg>>,
    session_metadata: Option<crate::session_store::SessionMetadata>,
    session_warning: Option<String>,
    session_save_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    session_save_task: Option<tokio::task::JoinHandle<()>>,
    pending_sso_client: Option<(matrix_sdk::Client, String)>,
    // Verification (SAS).
    show_security: bool,
    devices: Vec<crate::verify::DeviceInfo>,
    verify_rx: Option<std::sync::mpsc::Receiver<crate::verify::VerifyEvent>>,
    /// Watcher channel for incoming requests; coexists with one-shot verify_rx flows.
    watch_rx: Option<std::sync::mpsc::Receiver<crate::verify::IncomingRequest>>,
    pending_flow: Option<String>,
    sas: Option<crate::verify::SasEmojis>,
    verify_user_input: String,
    /// Incoming verification offer; accept/decline popup.
    incoming: Option<crate::verify::IncomingRequest>,
    /// Surfaced flow ids; stops poll re-popup.
    seen_incoming: std::collections::HashSet<String>,
    /// Watcher runs while logged in: syncs + polls for incoming requests.
    verify_watch_running: bool,
    // Settings.
    config_file: crate::config::ConfigFile,
    /// Saved theme choice, independent of a temporary CLI override.
    config_theme: String,
    show_settings: bool,
    /// Builtin theme name.
    settings_theme: String,
    /// Which settings section is showing.
    settings_tab: SettingsTab,
    /// Display name draft; committed with Save.
    settings_display_name: String,
    /// Per-platform link embedding rules.
    embed_rules: Vec<crate::embed::EmbedRule>,
    /// Fetched link cards.
    embeds: crate::embed::EmbedCache,
    embed_rx: Option<std::sync::mpsc::Receiver<(String, Option<crate::embed::Embed>)>>,
    embed_tx: Option<std::sync::mpsc::Sender<(String, Option<crate::embed::Embed>)>>,
    /// Thumbnails for link cards, which are https URLs rather than mxc.
    embed_images: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    /// Per-rule host edit buffers; survive across frames.
    embed_host_bufs: Vec<String>,
    embed_img_rx: Option<std::sync::mpsc::Receiver<(String, Option<egui::ColorImage>)>>,
    embed_img_tx: Option<std::sync::mpsc::Sender<(String, Option<egui::ColorImage>)>>,
    /// People we have ignored, read from `m.ignored_user_list`.
    ignored_users: Vec<String>,
    ignored_tx: Option<std::sync::mpsc::Sender<Vec<String>>>,
    ignored_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    /// Font size slider (10–24pt), applied live.
    settings_font_size: f32,
    /// Show newest message under each room name.
    show_previews: bool,
    /// Room sidebar (rooms + DMs) hidden; the toggle lives in the titlebar.
    sidebar_collapsed: bool,
    // Runtime.
    /// Shared tokio runtime for all async tasks.
    rt: Arc<tokio::runtime::Runtime>,
    /// Drag-and-drop hint for this backend.
    drop_hint: String,
    /// Repaint handle for workers; results need `request_repaint` to show.
    ctx: egui::Context,
}

/// One live-sync batch: rows per room + verification notices; reactions fold into targets.
#[derive(Debug, Clone, Default)]
struct SyncBatch {
    emoji_usage: Option<crate::recent_emoji::RecentEmoji>,
    rows: Vec<(String, TimelineRow)>,
    reactions: Vec<(String, SyncReaction)>,
    verify_flows: Vec<crate::verify::IncomingRequest>,
    /// (room, event, user) read receipts.
    receipts: Vec<(String, String, String)>,
    /// (room, target, body, formatted) `m.replace` edits.
    edits: Vec<(String, String, String, Option<String>)>,
    /// (room, redacted event) `m.room.redaction`s.
    redactions: Vec<(String, String)>,
    /// True when at least one row landed in the currently viewed room.
    had_current_rows: bool,
}

impl SyncBatch {
    /// True when nothing here changes the UI.
    fn is_empty(&self) -> bool {
        self.emoji_usage.is_none()
            && self.rows.is_empty()
            && self.reactions.is_empty()
            && self.verify_flows.is_empty()
            && self.receipts.is_empty()
            && self.redactions.is_empty()
            && self.edits.is_empty()
    }
}

/// One `m.reaction`: target + key + sender.
#[derive(Debug, Clone)]
struct SyncReaction {
    /// The message being reacted to.
    target: String,
    key: String,
    sender: String,
    /// Own event id; named by a later redaction.
    event_id: String,
}

/// Skin-tone modifiers, applied on insert.
const SKIN_TONES: &[(&str, &str)] = &[
    ("", "none"),
    ("🏻", "light"),
    ("🏼", "medium-light"),
    ("🏽", "medium"),
    ("🏾", "medium-dark"),
    ("🏿", "dark"),
];

/// Slash commands; every entry is dispatched in `send_current_input`.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/me", "emote — /me waves"),
    ("/shrug", "append ¯\\_(ツ)_/¯"),
    ("/plain", "send without markdown"),
    ("/spoiler", "send as a spoiler"),
    ("/react", "react to the last message — /react 👍"),
    ("/reply", "reply to the last message"),
    ("/sticker", "send a sticker — /sticker party"),
    ("/join", "join a room — /join #room:hs"),
    ("/part", "leave this room"),
    ("/invite", "invite — /invite @user:hs"),
    ("/kick", "remove someone — /kick @user:hs [reason]"),
    ("/ban", "ban — /ban @user:hs [reason]"),
    ("/unban", "unban — /unban @user:hs"),
    ("/op", "set power level — /op @user:hs [50]"),
    ("/deop", "reset power level to 0 — /deop @user:hs"),
    ("/ignore", "ignore a user everywhere"),
    ("/unignore", "stop ignoring a user"),
    ("/topic", "set the room topic"),
    ("/roomname", "set the room name"),
    ("/nick", "set your display name (all rooms)"),
    ("/help", "list these commands"),
];

#[derive(Debug, Clone)]
struct RoomEntry {
    room_id: String,
    name: String,
    unread: usize,
    mentioned: bool,
    is_dm: bool,
    /// Room avatar mxc (for a DM, the other person's avatar).
    avatar_mxc: Option<String>,
    /// Newest "sender: text" preview.
    preview: Option<String>,
}

#[derive(Debug, Clone)]
struct RoomInfo {
    room_id: String,
    name: String,
    is_dm: bool,
    avatar_mxc: Option<String>,
}

#[derive(Debug, Clone)]
struct Member {
    display: String,
    mxid: String,
    online: bool,
    avatar_mxc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerTab {
    Emoji,
    Custom,
    Stickers,
}

/// Staged upload: bytes + name + mime. 25 MiB cap.
#[derive(Debug, Clone)]
struct PendingUpload {
    name: String,
    mime: String,
    bytes: Vec<u8>,
}

impl PendingUpload {
    const MAX_BYTES: usize = 25 * 1024 * 1024;

    fn from_bytes(name: String, bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() > Self::MAX_BYTES {
            return Err(format!(
                "{} too large (>{} MiB)",
                name,
                Self::MAX_BYTES / 1024 / 1024
            ));
        }
        let mime = mime_guess::from_path(&name)
            .first_or_octet_stream()
            .to_string();
        Ok(Self { name, mime, bytes })
    }

    fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}
#[derive(Debug, Clone, Default)]
struct LoginForm {
    homeserver: String,
    username: String,
    password: String,
    sso_token_input: String,
    sso_url: String,
    error: String,
    busy: bool,
    show_sso_token: bool,
}

struct LoggedIn {
    client: matrix_sdk::Client,
    user_id: String,
    rooms: Vec<RoomInfo>,
    packs: crate::matrix::PackStore,
    emoji_usage: crate::recent_emoji::RecentEmoji,
    session_metadata: Option<crate::session_store::SessionMetadata>,
    persistence_warning: Option<String>,
}

/// Initial history and cached members, delivered independently for each room.
struct InitialHistory {
    room_id: String,
    result: Result<(Vec<TimelineRow>, Option<String>, Vec<Member>), String>,
}

enum LoginMsg {
    PasswordDone(Result<LoggedIn, String>),
    SsoUrlReady(Result<(matrix_sdk::Client, String, String), String>),
    SsoDone(Result<LoggedIn, String>),
}

/// One scrollback page: room, older rows, token for the page before (`None` at start).
type HistoryPage = (String, Result<(Vec<TimelineRow>, Option<String>), String>);

/// Settings sections.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Account,
    Appearance,
    Notifications,
    Sessions,
    Privacy,
    Emoji,
    Links,
}

impl SettingsTab {
    const ALL: [SettingsTab; 7] = [
        SettingsTab::Account,
        SettingsTab::Appearance,
        SettingsTab::Notifications,
        SettingsTab::Sessions,
        SettingsTab::Privacy,
        SettingsTab::Emoji,
        SettingsTab::Links,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Account => "Account",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Notifications => "Notifications",
            SettingsTab::Sessions => "Sessions",
            SettingsTab::Privacy => "Security & Privacy",
            SettingsTab::Emoji => "Emoji & Stickers",
            SettingsTab::Links => "Link previews",
        }
    }

    fn icon(self) -> &'static str {
        use crate::ui::icons;
        match self {
            SettingsTab::Account => icons::ACCOUNT,
            SettingsTab::Appearance => icons::PALETTE,
            SettingsTab::Notifications => icons::ALERT,
            SettingsTab::Sessions => icons::VERIFIED,
            SettingsTab::Privacy => icons::SHIELD,
            SettingsTab::Emoji => icons::EMOJI,
            SettingsTab::Links => icons::LINK,
        }
    }
}

/// Right-click actions on a room in the sidebar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoomMenuAction {
    MarkRead,
    MarkUnread,
    Favourite(bool),
    Notify(RoomNotify),
    Invite,
    CopyLink,
    Leave,
}

/// Per-room notification mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoomNotify {
    All,
    MentionsOnly,
    Mute,
}

impl RoomNotify {
    fn label(self) -> &'static str {
        match self {
            RoomNotify::All => "All messages",
            RoomNotify::MentionsOnly => "Mentions only",
            RoomNotify::Mute => "Mute",
        }
    }

    fn sdk(self) -> matrix_sdk::notification_settings::RoomNotificationMode {
        match self {
            RoomNotify::All => matrix_sdk::notification_settings::RoomNotificationMode::AllMessages,
            RoomNotify::MentionsOnly => {
                matrix_sdk::notification_settings::RoomNotificationMode::MentionsAndKeywordsOnly
            }
            RoomNotify::Mute => matrix_sdk::notification_settings::RoomNotificationMode::Mute,
        }
    }
}

/// Buttons on the profile card.
#[derive(Clone, Copy)]
enum ProfileAction {
    Message,
    Mention,
    Verify,
    CopyId,
}

/// Moderation verbs that take `@user:hs [extra]`.
#[derive(Clone, Copy)]
enum Moderation {
    Kick,
    Ban,
    Unban,
    /// `/op` power level, defaults to 50.
    Power,
    Ignore,
    Unignore,
}

/// Room membership / topic slash actions.
#[derive(Clone, Copy)]
enum RoomAction {
    Join,
    Invite,
    Topic,
    Leave,
}

impl ThraceApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config_file: crate::config::ConfigFile,
        cli_theme: Option<String>,
    ) -> Self {
        let config = config_file.saved.clone();
        let theme_name = cli_theme.unwrap_or_else(|| config.theme.clone());
        let mut theme = ThemeFile::load_builtin(&theme_name).unwrap_or_else(|_| ThemeFile {
            name: theme_name.clone(),
            parent: "dark".into(),
            palette: Default::default(),
            timeline: Default::default(),
            font: Default::default(),
        });
        theme.font.mono_size = Some(config.font_size);
        theme::apply_theme(&cc.egui_ctx, &theme);

        let packs = PackStore::new();

        // Sync + media are I/O bound; tasks give concurrency, pool takes decodes.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(4)
            .thread_name("thrace")
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        Self {
            rt: Arc::new(rt),
            ctx: cc.egui_ctx.clone(),
            drop_hint: "drag files here to attach".into(),
            theme,
            rooms: Vec::new(),
            current: 0,
            rows: std::rc::Rc::new(Vec::new()),
            members: Vec::new(),
            input: String::new(),
            replying_to: None,
            editing: None,
            pending_uploads: Vec::new(),
            upload_thumbs: std::collections::HashMap::new(),
            show_picker: false,
            picker_opened_frame: 0,
            picker_tab: PickerTab::Emoji,
            picker_query: String::new(),
            skin_tone: String::new(),
            recent_emoji: Vec::new(),
            emoji_usage: Default::default(),
            emoji_save_tx: None,
            emoji_save_task: None,
            scroll_to_category: None,
            scroll_to_event: None,
            highlight_event: None,
            react_target: None,
            slash_selected: 0,
            mention_selected: 0,
            packs,
            media: crate::media_cache::MediaCache::new(),
            emoji_font: crate::emoji_font::EmojiFont::new(),
            media_rx: None,
            media_tx: None,
            media_req_tx: None,
            image_preview: None,
            video: None,
            video_file: None,
            video_rx: None,
            video_tx: None,
            profile_target: None,
            room_menu: None,
            favourites: std::collections::HashSet::new(),
            send_rx: None,
            send_tx: None,
            sync_rx: None,
            sync_running: false,
            sync_task: None,
            last_receipt: None,
            own_user: String::new(),
            timelines: std::collections::HashMap::new(),
            history_queue: Default::default(),
            history_tasks: Default::default(),
            history_rx: None,
            history_tx: None,
            back_tokens: std::collections::HashMap::new(),
            paginating: None,
            page_rx: None,
            page_tx: None,
            pending_scroll_fix: None,
            last_content_height: 0.0,
            jump_to_latest: false,
            jump_until: 0.0,
            members_by_room: std::collections::HashMap::new(),
            status: "not logged in — password or SSO below".into(),
            login: LoginForm {
                homeserver: "https://matrix.org".into(),
                ..Default::default()
            },
            client: None,
            login_rx: None,
            session_metadata: None,
            session_warning: None,
            session_save_rx: None,
            session_save_task: None,
            pending_sso_client: None,
            show_security: false,
            devices: Vec::new(),
            verify_rx: None,
            watch_rx: None,
            pending_flow: None,
            sas: None,
            verify_user_input: String::new(),
            incoming: None,
            seen_incoming: std::collections::HashSet::new(),
            verify_watch_running: false,
            show_settings: false,
            config_file,
            config_theme: config.theme,
            settings_theme: theme_name.clone(),
            settings_tab: SettingsTab::Account,
            settings_display_name: String::new(),
            ignored_users: Vec::new(),
            embed_rules: config.embeds.rules,
            embeds: crate::embed::EmbedCache::default(),
            embed_rx: None,
            embed_tx: None,
            embed_images: std::collections::HashMap::new(),
            embed_host_bufs: Vec::new(),
            embed_img_rx: None,
            embed_img_tx: None,
            ignored_tx: None,
            ignored_rx: None,
            settings_font_size: config.font_size,
            show_previews: config.show_previews,
            sidebar_collapsed: false,
        }
    }

    /// Record window backend for the drag-drop hint.
    pub fn set_backend(&mut self, backend: &str, drops_supported: bool) {
        self.drop_hint = if drops_supported {
            "or drag files onto the window".into()
        } else {
            format!("dragging files in is unavailable on {backend} — paste with Ctrl+V instead")
        };
    }

    /// Read receipts as overlapping circular avatars; overflow collapses to a count.
    fn render_seen_by(&mut self, ui: &mut egui::Ui, seen: &[String]) {
        /// Avatars shown before overflow count.
        const MAX_SHOWN: usize = 5;
        let size = 16.0;
        // Avatars overlap by ~40%.
        let step = size * 0.62;
        let shown = seen.len().min(MAX_SHOWN);
        let overflow = seen.len() - shown;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let width = step * shown.saturating_sub(1) as f32 + size;
            let (area, _) = ui.allocate_exact_size(egui::vec2(width, size), egui::Sense::hover());
            let ring = ui.visuals().panel_fill;
            for (i, mxid) in seen.iter().take(shown).enumerate() {
                let (name, avatar, _) = self.profile_of(mxid);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(area.left() + step * i as f32, area.top()),
                    egui::vec2(size, size),
                );
                self.paint_avatar_circle(ui, rect, &avatar, &name, ring);
                // Hover names each reader.
                ui.interact(
                    rect,
                    ui.id().with(("seen", i, mxid.as_str())),
                    egui::Sense::hover(),
                )
                .on_hover_text(&name);
            }
            if overflow > 0 {
                let rest: Vec<String> = seen
                    .iter()
                    .skip(shown)
                    .map(|m| self.display_for(m))
                    .collect();
                ui.label(egui::RichText::new(format!("+{overflow}")).small().weak())
                    .on_hover_text(rest.join(", "));
            }
        });
    }

    /// Display name for an mxid.
    fn display_for(&self, mxid: &str) -> String {
        self.members
            .iter()
            .find(|m| m.mxid == mxid)
            .map(|m| m.display.clone())
            .unwrap_or_else(|| {
                mxid.trim_start_matches('@')
                    .split(':')
                    .next()
                    .unwrap_or(mxid)
                    .to_owned()
            })
    }

    /// One emoji at `size`: colour bitmap, else text glyph. Returns response for clicks.
    fn emoji_widget(&mut self, ui: &mut egui::Ui, emoji: &str, size: f32) -> egui::Response {
        if let Some(handle) = self.emoji_font.texture_at_size(ui.ctx(), emoji, size) {
            return ui.add(
                egui::Image::new(&handle)
                    .fit_to_exact_size(egui::vec2(size, size))
                    .sense(egui::Sense::click()),
            );
        }
        // Same size as the image so a missing bitmap keeps line height.
        crate::ui::clickable(
            ui.add(
                egui::Label::new(egui::RichText::new(emoji).size(size * 0.9))
                    .selectable(false)
                    .sense(egui::Sense::click()),
            ),
        )
    }

    /// Text with colour emoji at an explicit position, for painted sidebar rows.
    fn paint_text_with_emoji(
        &mut self,
        ui: &mut egui::Ui,
        pos: egui::Pos2,
        text: &str,
        size: f32,
        colour: egui::Color32,
        max_width: f32,
    ) {
        let font = egui::FontId::proportional(size);
        let mut x = pos.x;
        let limit = pos.x + max_width;
        let mut buf = String::new();
        // Flush pending text; returns its width.
        let flush = |ui: &mut egui::Ui, buf: &mut String, x: &mut f32| {
            if buf.is_empty() {
                return;
            }
            let galley = ui
                .painter()
                .layout_no_wrap(std::mem::take(buf), font.clone(), colour);
            let w = galley.size().x;
            ui.painter().galley(
                egui::pos2(*x, pos.y - galley.size().y * 0.5),
                galley,
                colour,
            );
            *x += w;
        };
        let mut rest = text;
        while !rest.is_empty() {
            if x > limit {
                break;
            }
            let Some(len) = crate::emoji::emoji_cluster_len(rest) else {
                let ch = rest.chars().next().expect("non-empty");
                buf.push(ch);
                rest = &rest[ch.len_utf8()..];
                continue;
            };
            let cluster = rest[..len].to_owned();
            rest = &rest[len..];
            flush(ui, &mut buf, &mut x);
            if x > limit {
                break;
            }
            let side = size * 1.15;
            let rect = egui::Rect::from_center_size(
                egui::pos2(x + side * 0.5, pos.y),
                egui::vec2(side, side),
            );
            match self.emoji_font.texture_at_size(ui.ctx(), &cluster, side) {
                Some(handle) => {
                    ui.painter().image(
                        handle.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    // No bitmap: use the glyph.
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &cluster,
                        font.clone(),
                        colour,
                    );
                }
            }
            x += side;
        }
        flush(ui, &mut buf, &mut x);
    }

    /// Short text with colour emoji, for previews and banners.
    fn emoji_text(&mut self, ui: &mut egui::Ui, text: &str, size: f32, weak: bool) {
        let style = |rt: egui::RichText| if weak { rt.weak() } else { rt };
        // Wrapped so long text can't run past the window.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut buf = String::new();
            let mut rest = text;
            while !rest.is_empty() {
                match crate::emoji::emoji_cluster_len(rest) {
                    Some(len) => {
                        if !buf.is_empty() {
                            let pending = std::mem::take(&mut buf);
                            ui.label(style(egui::RichText::new(pending).size(size)));
                        }
                        self.emoji_widget(ui, &rest[..len], size * 1.15);
                        rest = &rest[len..];
                    }
                    None => {
                        let ch = rest.chars().next().expect("non-empty");
                        buf.push(ch);
                        rest = &rest[ch.len_utf8()..];
                    }
                }
            }
            if !buf.is_empty() {
                ui.label(style(egui::RichText::new(buf).size(size)));
            }
        });
    }

    /// Text run split into colour-emoji images and plain text.
    fn render_text_with_emoji(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        span: &crate::markdown::Span,
        size: f32,
    ) {
        if !self.emoji_font.available() {
            self.render_plain_run(ui, text, span);
            return;
        }
        // Walk clusters so modifiers/ZWJ stay attached.
        let mut buf = String::new();
        let mut rest = text;
        while !rest.is_empty() {
            match crate::emoji::emoji_cluster_len(rest) {
                Some(len) => {
                    if !buf.is_empty() {
                        let pending = std::mem::take(&mut buf);
                        self.render_plain_run(ui, &pending, span);
                    }
                    let cluster = &rest[..len];
                    self.emoji_widget(ui, cluster, size)
                        .on_hover_text(crate::emoji::name_of(cluster));
                    rest = &rest[len..];
                }
                None => {
                    let ch = rest.chars().next().expect("non-empty");
                    buf.push(ch);
                    rest = &rest[ch.len_utf8()..];
                }
            }
        }
        if !buf.is_empty() {
            self.render_plain_run(ui, &buf, span);
        }
    }

    /// Save client preferences without runtime state.
    fn save_preferences(&mut self) {
        let config = crate::config::Config {
            theme: self.config_theme.clone(),
            font_size: self.settings_font_size,
            show_previews: self.show_previews,
            embeds: crate::config::Embeds {
                rules: self.embed_rules.clone(),
            },
        };
        if let Err(error) = self.config_file.save(config) {
            self.status = format!("Could not save settings: {error:#}");
        }
    }

    /// Remember a pick for the "frequently used" row.
    fn note_recent_emoji(&mut self, emoji: &str) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.emoji_usage.record(emoji);
        self.recent_emoji = self.emoji_usage.frequent(18);
        if self.emoji_save_tx.is_none() {
            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::recent_emoji::RecentEmoji>();
            let errors = self.send_channel();
            let ctx = self.ctx.clone();
            self.emoji_save_task = Some(self.rt.spawn(async move {
                while let Some(mut usage) = rx.recv().await {
                    // Serialize writes and coalesce rapid picks so an older save cannot win.
                    while let Ok(newer) = rx.try_recv() {
                        usage = newer;
                    }
                    if let Err(error) = usage.save(&client).await {
                        let _ = errors.send(SendResult::Failed(format!(
                            "Could not save emoji usage: {error}"
                        )));
                        ctx.request_repaint();
                    }
                }
            }));
            self.emoji_save_tx = Some(tx);
        }
        let _ = self
            .emoji_save_tx
            .as_ref()
            .unwrap()
            .send(self.emoji_usage.clone());
    }

    fn stop_emoji_saving(&mut self) {
        self.emoji_save_tx = None;
        if let Some(task) = self.emoji_save_task.take() {
            task.abort();
        }
        self.emoji_usage = Default::default();
        self.recent_emoji.clear();
    }

    /// Emoji grid: a row of category jumps over the scrolled sections. Returns this frame's
    /// pick. Search comes from the caller's field, so the grid adds no second box.
    fn emoji_grid(&mut self, ui: &mut egui::Ui, height: f32) -> Option<String> {
        let mut picked: Option<String> = None;
        let recents = self.recent_emoji.clone();

        ui.horizontal_wrapped(|ui| {
            // Category jumps read as separate buttons only with room between them.
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 6.0);
            if !recents.is_empty() {
                // Clock icon jumps to recents.
                if self
                    .emoji_widget(ui, "\u{1F551}", 17.0)
                    .on_hover_text("Frequently used")
                    .clicked()
                {
                    self.scroll_to_category = Some(usize::MAX);
                }
            }
            for (i, cat) in crate::emoji::CATEGORIES.iter().enumerate() {
                if self
                    .emoji_widget(ui, cat.icon, 17.0)
                    .on_hover_text(cat.name)
                    .clicked()
                {
                    self.scroll_to_category = Some(i);
                }
            }
            ui.add_space(4.0);
            self.skin_tone_menu(ui);
        });
        ui.separator();

        let query = self.picker_query.trim().to_lowercase();
        let jump = self.scroll_to_category.take();
        egui::ScrollArea::vertical()
            .max_height(height)
            .show(ui, |ui| {
                // Search collapses categories into one ranked list.
                if !query.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for (_, e, skin) in crate::emoji::search(&query) {
                            let label = self.emoji_with_skin(e, skin);
                            if self
                                .emoji_widget(ui, &label, 22.0)
                                .on_hover_text(crate::emoji::name_of(e))
                                .clicked()
                            {
                                picked = Some(label);
                            }
                        }
                    });
                    return;
                }

                if !recents.is_empty() {
                    let header = ui.label(
                        egui::RichText::new("Frequently used")
                            .small()
                            .weak()
                            .strong(),
                    );
                    if jump == Some(usize::MAX) {
                        header.scroll_to_me(Some(egui::Align::TOP));
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for e in &recents {
                            if self
                                .emoji_widget(ui, e, 22.0)
                                .on_hover_text(crate::emoji::name_of(e))
                                .clicked()
                            {
                                picked = Some(e.clone());
                            }
                        }
                    });
                }

                for (i, cat) in crate::emoji::CATEGORIES.iter().enumerate() {
                    let header = ui.label(egui::RichText::new(cat.name).small().weak().strong());
                    if jump == Some(i) {
                        header.scroll_to_me(Some(egui::Align::TOP));
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for (e, skin) in cat.emojis.iter() {
                            let label = self.emoji_with_skin(e, *skin);
                            if self
                                .emoji_widget(ui, &label, 22.0)
                                .on_hover_text(crate::emoji::name_of(e))
                                .clicked()
                            {
                                picked = Some(label);
                            }
                        }
                    });
                }
            });

        if let Some(e) = &picked {
            self.note_recent_emoji(e);
        }
        picked
    }

    /// Skin-tone chooser, collapsed into a dropdown. The button is the hand in the current
    /// tone and the menu is the same hand in all six, so the colour carries the choice and
    /// the tone names are left to hover text.
    fn skin_tone_menu(&mut self, ui: &mut egui::Ui) {
        const SWATCH: f32 = 18.0;
        let current = format!("\u{270B}{}", self.skin_tone);
        let chevron = egui::RichText::new(crate::ui::icons::CHEVRON_DOWN).size(12.0);
        let button = match self.emoji_font.texture_at_size(ui.ctx(), &current, SWATCH) {
            Some(handle) => ui.add(egui::Button::new((
                egui::Image::new(&handle).fit_to_exact_size(egui::Vec2::splat(SWATCH)),
                chevron,
            ))),
            // No colour bitmap: the text glyph still shows the tone.
            None => ui.add(egui::Button::new((
                egui::RichText::new(&current).size(SWATCH * 0.9),
                chevron,
            ))),
        }
        .on_hover_text("Skin tone");
        egui::Popup::menu(&button).show(|ui| {
            ui.horizontal(|ui| {
                for (modifier, label) in SKIN_TONES {
                    let swatch = format!("\u{270B}{modifier}");
                    let resp = self.emoji_widget(ui, &swatch, 22.0).on_hover_text(*label);
                    if self.skin_tone == *modifier {
                        // Underline the active tone; image widgets carry no selection state.
                        let r = resp.rect;
                        ui.painter().line_segment(
                            [
                                egui::pos2(r.left(), r.bottom() + 1.0),
                                egui::pos2(r.right(), r.bottom() + 1.0),
                            ],
                            egui::Stroke::new(2.0, self.theme.gold()),
                        );
                    }
                    if resp.clicked() {
                        self.skin_tone = (*modifier).to_owned();
                        ui.close();
                    }
                }
            });
        });
    }

    // Settings sections.

    /// Profile and credentials.
    fn settings_account(&mut self, ui: &mut egui::Ui) {
        ui.heading("Account");
        let mxid = self
            .client
            .as_ref()
            .and_then(|c| c.user_id())
            .map(|u| u.to_string());
        let Some(mxid) = mxid else {
            ui.label("Not signed in.");
            return;
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
            let ring = ui.visuals().panel_fill;
            // Our own avatar, taken from whichever room has us in its
            // member list — no extra request needed.
            let avatar = self.profile_of(&mxid).1;
            let name = self.display_for(&mxid);
            self.paint_avatar_circle(ui, rect, &avatar, &name, ring);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(&mxid).strong());
                let homeserver = mxid.split(':').nth(1).unwrap_or("?").to_owned();
                ui.label(egui::RichText::new(homeserver).small().weak());
            });
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Display name").small().weak());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.settings_display_name).desired_width(220.0),
            );
            if ui.button("Save").clicked() {
                let name = self.settings_display_name.clone();
                self.set_nick(name);
            }
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Changing your password signs out your other sessions on some servers — \
                                      do it from your homeserver's account page.",
            )
            .small()
            .weak(),
        );
    }

    /// Theme, font size, message previews.
    fn settings_appearance(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let mut pick = None;
        ui.heading("Appearance");
        ui.label(egui::RichText::new("Theme").small().weak());
        ui.horizontal_wrapped(|ui| {
            for name in ["dark", "midnight", "bbs-amber", "win98"] {
                if ui
                    .selectable_label(self.settings_theme == name, name)
                    .clicked()
                {
                    pick = Some(name.to_owned());
                }
            }
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!("Font size — {:.0}pt", self.settings_font_size))
                .small()
                .weak(),
        );
        ui.add(egui::Slider::new(&mut self.settings_font_size, 10.0..=24.0).step_by(1.0));
        ui.add_space(8.0);
        ui.checkbox(
            &mut self.show_previews,
            "Show the newest message under each room",
        );
        ui.add_space(10.0);
        ui.separator();
        // CC-BY 4.0 attribution for bundled emoji artwork.
        ui.label(
            egui::RichText::new(
                "Emoji: Twemoji © Twitter/X, CC-BY 4.0 — font build by \
                 mozilla/twemoji-colr (Apache-2.0)",
            )
            .small()
            .weak(),
        );
        pick
    }

    /// Default notification behaviour.
    fn settings_notifications(&mut self, ui: &mut egui::Ui) {
        ui.heading("Notifications");
        ui.label(
            egui::RichText::new(
                "Defaults for new rooms. Per-room settings live in the room's \
                 right-click menu and override these.",
            )
            .small()
            .weak(),
        );
        ui.add_space(8.0);
        let mut chosen: Option<(bool, RoomNotify)> = None;
        for (one_to_one, heading) in [(false, "Group rooms"), (true, "Direct messages")] {
            ui.label(egui::RichText::new(heading).strong());
            ui.horizontal_wrapped(|ui| {
                for mode in [RoomNotify::All, RoomNotify::MentionsOnly, RoomNotify::Mute] {
                    if ui.button(mode.label()).clicked() {
                        chosen = Some((one_to_one, mode));
                    }
                }
            });
            ui.add_space(6.0);
        }
        if let Some((one_to_one, mode)) = chosen {
            self.set_default_notifications(one_to_one, mode);
        }
    }

    /// Signed-in devices.
    fn settings_sessions(&mut self, ui: &mut egui::Ui) {
        ui.heading("Sessions");
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Verified sessions can read your encrypted history.")
                    .small()
                    .weak(),
            );
            if crate::ui::icon_button(ui, crate::ui::icons::REFRESH, "Refresh").clicked() {
                self.refresh_devices();
            }
        });
        ui.add_space(6.0);
        if self.devices.is_empty() {
            ui.label(
                egui::RichText::new("No sessions loaded yet.")
                    .small()
                    .weak(),
            );
        }
        let devices = self.devices.clone();
        for d in devices {
            ui.horizontal(|ui| {
                let (icon, tint) = if d.verified {
                    (crate::ui::icons::VERIFIED, self.theme.gold())
                } else {
                    (crate::ui::icons::ALERT, ui.visuals().error_fg_color)
                };
                crate::ui::icon_label(ui, icon, tint);
                ui.vertical(|ui| {
                    let name = d
                        .display_name
                        .clone()
                        .unwrap_or_else(|| d.device_id.clone());
                    ui.label(if d.is_own {
                        egui::RichText::new(format!("{name} (this session)")).strong()
                    } else {
                        egui::RichText::new(name)
                    });
                    ui.label(egui::RichText::new(&d.device_id).small().weak());
                });
                if !d.is_own && !d.verified && ui.button("Verify").clicked() {
                    self.start_verify_device(&d.user_id, &d.device_id);
                }
            });
            ui.separator();
        }
    }

    /// Ignored users and encryption state.
    fn settings_privacy(&mut self, ui: &mut egui::Ui) {
        ui.heading("Security & Privacy");
        ui.label(egui::RichText::new("Ignored users").strong());
        ui.label(
            egui::RichText::new("You will not see messages from these people.")
                .small()
                .weak(),
        );
        if self.ignored_users.is_empty() {
            ui.label(egui::RichText::new("Nobody is ignored.").small().weak());
        }
        let ignored = self.ignored_users.clone();
        for mxid in ignored {
            ui.horizontal(|ui| {
                ui.label(&mxid);
                if ui.button("Un-ignore").clicked() {
                    self.moderate(mxid.clone(), Moderation::Unignore);
                    self.ignored_users.retain(|u| *u != mxid);
                }
            });
        }
        ui.add_space(10.0);
        ui.separator();
        ui.label(egui::RichText::new("Encryption").strong());
        ui.label(
            egui::RichText::new(
                "Messages in encrypted rooms are decrypted on this device. Verify \
                 your other sessions so they can read history too.",
            )
            .small()
            .weak(),
        );
        if ui.button("Open device verification").clicked() {
            self.show_security = true;
            self.refresh_devices();
        }
    }

    /// Custom emoji and sticker packs.
    fn settings_emoji(&mut self, ui: &mut egui::Ui) {
        ui.heading("Emoji & Stickers");
        let packs = self.packs.packs().to_vec();
        if packs.is_empty() {
            ui.label(
                egui::RichText::new(
                    "No packs. Packs come from your account (im.ponies.user_emotes) \
                     and from rooms you are in.",
                )
                .small()
                .weak(),
            );
            return;
        }
        ui.label(
            egui::RichText::new(format!(
                "{} pack(s), {} images",
                packs.len(),
                packs.iter().map(|p| p.images.len()).sum::<usize>()
            ))
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        for pack in packs {
            egui::CollapsingHeader::new(&pack.display_name)
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for img in &pack.images {
                            let tip = format!(":{}:", img.shortcode);
                            match self
                                .media
                                .texture_for("emoji", &img.mxc_url, Some((32, 32)))
                            {
                                Some(h) => {
                                    ui.add(egui::Image::new(&h).max_height(28.0))
                                        .on_hover_text(&tip);
                                }
                                None => {
                                    ui.label(egui::RichText::new(&tip).small().weak());
                                }
                            }
                        }
                    });
                });
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Packs are read-only here for now: adding and removing images writes \
                 account data and room state, which is not wired up yet.",
            )
            .small()
            .weak(),
        );
    }

    /// Load ignored users from account data.
    fn refresh_ignored(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.ignored_tx();
        self.rt.spawn(async move {
            use matrix_sdk::ruma::events::ignored_user_list::IgnoredUserListEventContent;
            let list = client
                .account()
                .account_data::<IgnoredUserListEventContent>()
                .await
                .ok()
                .flatten()
                .and_then(|raw| raw.deserialize().ok())
                .map(|c| c.ignored_users.keys().map(|u| u.to_string()).collect())
                .unwrap_or_default();
            let _ = tx.send(list);
        });
    }

    /// Lazily create the ignored-list channel.
    fn ignored_tx(&mut self) -> std::sync::mpsc::Sender<Vec<String>> {
        if self.ignored_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.ignored_tx = Some(tx);
            self.ignored_rx = Some(rx);
        }
        self.ignored_tx.clone().unwrap()
    }

    /// Per-platform link embedding.
    fn settings_links(&mut self, ui: &mut egui::Ui) {
        ui.heading("Link previews");
        ui.label(
            egui::RichText::new(
                "Links to sites that block previews can be opened through a \
                 front end instead, and shown inline.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);
        // Privacy trade-off; kept visible on purpose.
        ui.label(
            egui::RichText::new(
                "Fetching a preview asks that front end for a link someone \
                 sent you, which tells its operator your IP address and what \
                 you are reading. Rewriting alone sends nothing until you \
                 click. Both are off until you turn them on.",
            )
            .small()
            .color(ui.visuals().warn_fg_color),
        );
        ui.add_space(8.0);

        // Seed edit buffers on first use / rule added.
        if self.embed_host_bufs.len() != self.embed_rules.len() {
            self.embed_host_bufs = self
                .embed_rules
                .iter()
                .map(|r| r.hosts.join(", "))
                .collect();
        }
        let mut changed = false;
        for i in 0..self.embed_rules.len() {
            let name = self.embed_rules[i].name.clone();
            egui::CollapsingHeader::new(&name)
                .default_open(true)
                .show(ui, |ui| {
                    let rule = &mut self.embed_rules[i];
                    changed |= ui
                        .checkbox(&mut rule.enabled, "Enabled")
                        .on_hover_text(
                            "rewrite links, and show cards when the front end has an API",
                        )
                        .changed();
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Open links with").small().weak());
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut rule.open_with)
                                .hint_text("fxtwitter.com")
                                .desired_width(220.0),
                        )
                        .changed();
                    ui.label(
                        egui::RichText::new("Preview API (blank for rewrite only)")
                            .small()
                            .weak(),
                    );
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut rule.api)
                                .hint_text("https://api.fxtwitter.com")
                                .desired_width(220.0),
                        )
                        .changed();
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Hostnames to match, comma separated — add the \
                             front ends people post from, since those are the \
                             same links.",
                        )
                        .small()
                        .weak(),
                    );
                    // Buffer persists in state so typed separators survive re-parse.
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.embed_host_bufs[i])
                                .desired_width(280.0),
                        )
                        .changed()
                    {
                        rule.hosts = self.embed_host_bufs[i]
                            .split(',')
                            .map(|h| h.trim().to_lowercase())
                            .filter(|h| !h.is_empty())
                            .collect();
                        changed = true;
                    }
                });
        }
        if changed {
            // Cards depend on the front end; refetch after rule changes.
            self.embeds.clear();
            self.embed_images.clear();
        }
    }

    /// Set the default notification mode for group rooms or DMs.
    fn set_default_notifications(&mut self, one_to_one: bool, mode: RoomNotify) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::notification_settings::{IsEncrypted, IsOneToOne};
            let settings = client.notification_settings().await;
            let one = if one_to_one {
                IsOneToOne::Yes
            } else {
                IsOneToOne::No
            };
            // Covers encrypted and unencrypted rooms alike.
            let mut last = Ok(());
            for enc in [IsEncrypted::Yes, IsEncrypted::No] {
                last = settings
                    .set_default_room_notification_mode(enc, one, mode.sdk())
                    .await;
                if last.is_err() {
                    break;
                }
            }
            match last {
                Ok(()) => SendResult::Done(format!("default: {}", mode.label())),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// Apply selected skin tone on insert.
    fn emoji_with_skin(&self, base: &str, supports_skin: bool) -> String {
        if supports_skin && !self.skin_tone.is_empty() {
            format!("{base}{}", self.skin_tone)
        } else {
            base.to_owned()
        }
    }

    /// Retry remembering an in-memory login after the user starts or unlocks a wallet.
    fn retry_session_save(&mut self) {
        if self.session_save_rx.is_some() {
            return;
        }
        let Some(metadata) = self.session_metadata.clone() else {
            return;
        };
        let Some(session) = self
            .client
            .as_ref()
            .and_then(|client| client.matrix_auth().session())
        else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.session_save_rx = Some(rx);
        let ctx = self.ctx.clone();
        self.session_save_task = Some(self.rt.spawn(async move {
            let result = crate::session_store::save(&session_path(), &metadata, &session)
                .await
                .map_err(|error| format!("Login is not saved: {error:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        }));
    }

    /// Restore cached session on startup; reports via PasswordDone.
    pub fn try_restore_cached(&mut self) {
        if self.client.is_some() || self.login_rx.is_some() {
            return;
        }
        if !session_path().exists() {
            return;
        }
        self.login.busy = true;
        self.status = "restoring cached session …".into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        self.spawn_blocking_task(move || {
            let res = restore_blocking();
            let _ = tx.send(LoginMsg::PasswordDone(res));
        });
    }

    fn nick_color(&self, nick: &str) -> egui::Color32 {
        let colors = &self.theme.timeline.nick_colors;
        if colors.is_empty() {
            return egui::Color32::LIGHT_BLUE;
        }
        let h: usize = nick.bytes().fold(5381usize, |a, b| {
            a.wrapping_mul(33).wrapping_add(b as usize)
        });
        theme::parse_hex(&colors[h % colors.len()]).unwrap_or(egui::Color32::LIGHT_BLUE)
    }
    fn avatar_initial(name: &str) -> String {
        name.chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or("?".into())
    }

    /// Avatar at `size`: cached mxc texture, else coloured initial. Clickable.
    fn render_avatar_sized(
        &mut self,
        ui: &mut egui::Ui,
        mxc: &Option<String>,
        name: &str,
        size: f32,
    ) -> egui::Response {
        // Request near drawn size to avoid upscaled thumbnails.
        let request = (size.ceil() as u32).max(32);
        if let Some(uri) = mxc {
            let kind = if request > 64 { "avatar-lg" } else { "avatar" };
            if let Some(handle) = self.media.texture_for(kind, uri, Some((request, request))) {
                return ui.add(
                    egui::Image::new(&handle)
                        .max_size(egui::vec2(size, size))
                        .sense(egui::Sense::click()),
                );
            }
        }
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
        let col = self.nick_color(name);
        ui.painter()
            .rect_filled(rect, size * 0.21, col.gamma_multiply(0.25));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            Self::avatar_initial(name),
            egui::FontId::monospace(size * 0.46),
            col,
        );
        resp
    }

    /// Circular avatar in an explicit rect; ring separates overlaps.
    fn paint_avatar_circle(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        mxc: &Option<String>,
        name: &str,
        ring: egui::Color32,
    ) {
        let centre = rect.center();
        let radius = rect.width() * 0.5;
        let texture = mxc.as_ref().and_then(|uri| {
            let request = (rect.width().ceil() as u32).max(32);
            self.media
                .texture_for("avatar", uri, Some((request, request)))
        });
        match texture {
            Some(handle) => {
                let mut mesh = egui::Mesh::with_texture(handle.id());
                const SEGMENTS: usize = 24;
                mesh.vertices.push(egui::epaint::Vertex {
                    pos: centre,
                    uv: egui::pos2(0.5, 0.5),
                    color: egui::Color32::WHITE,
                });
                for i in 0..=SEGMENTS {
                    let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
                    let (sin, cos) = angle.sin_cos();
                    mesh.vertices.push(egui::epaint::Vertex {
                        pos: centre + egui::vec2(cos * radius, sin * radius),
                        uv: egui::pos2(0.5 + cos * 0.5, 0.5 + sin * 0.5),
                        color: egui::Color32::WHITE,
                    });
                }
                for i in 1..=SEGMENTS as u32 {
                    mesh.indices.extend_from_slice(&[0, i, i + 1]);
                }
                ui.painter().add(egui::Shape::mesh(mesh));
            }
            None => {
                let colour = self.nick_color(name);
                ui.painter()
                    .circle_filled(centre, radius, colour.gamma_multiply(0.35));
                ui.painter().text(
                    centre,
                    egui::Align2::CENTER_CENTER,
                    Self::avatar_initial(name),
                    egui::FontId::proportional(radius * 0.95),
                    colour,
                );
            }
        }
        // Ring last so it overlays neighbours.
        ui.painter()
            .circle_stroke(centre, radius, egui::Stroke::new(1.5, ring));
    }

    /// Avatar into an explicit rect; no layout. For fixed room-list geometry.
    fn paint_avatar_at(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        mxc: &Option<String>,
        name: &str,
    ) {
        let size = rect.width();
        if let Some(uri) = mxc {
            let request = (size.ceil() as u32).max(32);
            if let Some(handle) = self
                .media
                .texture_for("avatar", uri, Some((request, request)))
            {
                let tint = egui::Color32::WHITE;
                ui.painter().image(
                    handle.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    tint,
                );
                return;
            }
        }
        let col = self.nick_color(name);
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(7), col.gamma_multiply(0.25));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            Self::avatar_initial(name),
            egui::FontId::proportional(size * 0.46),
            col,
        );
    }

    fn render_avatar(
        &mut self,
        ui: &mut egui::Ui,
        mxc: &Option<String>,
        name: &str,
    ) -> egui::Response {
        self.render_avatar_sized(ui, mxc, name, 28.0)
    }

    /// Known user info without a round trip.
    fn profile_of(&self, mxid: &str) -> (String, Option<String>, bool) {
        if let Some(m) = self.members.iter().find(|m| m.mxid == mxid) {
            return (m.display.clone(), m.avatar_mxc.clone(), m.online);
        }
        // Absent from member list: fall back to their latest message.
        let avatar = self
            .rows
            .iter()
            .rev()
            .find(|r| r.sender == mxid)
            .and_then(|r| r.avatar_mxc.clone());
        (self.display_for(mxid), avatar, false)
    }

    /// How many of the rooms we have loaded this user is also in.
    fn shared_rooms(&self, mxid: &str) -> usize {
        count_shared_rooms(&self.members, &self.members_by_room, mxid)
    }

    /// Timeline image ≤320px wide; placeholder while downloading, error + retry.
    fn render_image(&mut self, ui: &mut egui::Ui, img: &ImageAttachment) {
        let cache_key = format!("thumbnail:{}", img.mxc);
        if let Some(handle) = self.media.texture_for_source(
            &cache_key,
            img.thumbnail_source
                .clone()
                .unwrap_or_else(|| img.source.clone()),
            Some((320, 320)),
        ) {
            let size = handle.size_vec2();
            let w = size.x.min(320.0);
            let h = if size.x > 0.0 {
                size.y * (w / size.x)
            } else {
                180.0
            };
            let hint = if img.is_video {
                "Play video"
            } else {
                "Open full-size image"
            };
            let resp = ui
                .add(
                    egui::Image::new(&handle)
                        .max_size(egui::vec2(w, h.min(320.0)))
                        .sense(egui::Sense::click()),
                )
                .on_hover_text(hint);
            if img.is_video {
                // Play badge marks it as video, not a still.
                let centre = resp.rect.center();
                ui.painter()
                    .circle_filled(centre, 22.0, egui::Color32::from_black_alpha(140));
                ui.painter().text(
                    centre,
                    egui::Align2::CENTER_CENTER,
                    "\u{25B6}",
                    egui::FontId::proportional(22.0),
                    egui::Color32::WHITE,
                );
                if let Some(ms) = img.duration_ms {
                    let secs = ms / 1000;
                    ui.painter().text(
                        resp.rect.right_bottom() - egui::vec2(6.0, 6.0),
                        egui::Align2::RIGHT_BOTTOM,
                        format!("{}:{:02}", secs / 60, secs % 60),
                        egui::FontId::proportional(11.0),
                        egui::Color32::WHITE,
                    );
                }
            }
            if resp.clicked() {
                self.image_preview = Some(img.clone());
            }
            return;
        }
        match self.media.status(&cache_key) {
            Some(entry) if entry.error().is_some() => {
                let e = entry.error().unwrap_or_default().to_owned();
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        crate::ui::icon_label(
                            ui,
                            crate::ui::icons::BROKEN,
                            ui.visuals().error_fg_color,
                        );
                        ui.label(egui::RichText::new(truncate_name(&img.name, 28)).small());
                    });
                    ui.label(egui::RichText::new(&e).small().weak());
                    if crate::ui::icon_button(ui, crate::ui::icons::REFRESH, "Retry download")
                        .clicked()
                    {
                        self.media.retry(&cache_key);
                    }
                });
            }
            _ => {
                ui.horizontal(|ui| {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(180.0, 110.0), egui::Sense::click());
                    ui.painter()
                        .rect_filled(r, 4.0, egui::Color32::from_gray(30));
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        crate::ui::icons::IMAGE,
                        egui::FontId::proportional(28.0),
                        egui::Color32::from_gray(90),
                    );
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(truncate_name(&img.name, 28)).small());
                        ui.label(
                            egui::RichText::new(format!("{}×{}", img.w, img.h))
                                .small()
                                .weak(),
                        );
                    });
                });
            }
        }
    }

    fn render_image_preview(&mut self, ctx: &egui::Context) {
        let Some(image) = self.image_preview.clone() else {
            return;
        };
        let mut open = true;
        crate::ui::popup(ctx, &truncate_name(&image.name, 40))
            .open(&mut open)
            .resizable(true)
            .default_size(egui::vec2(760.0, 600.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if crate::ui::icon_button(ui, crate::ui::icons::DOWNLOAD, "Save the original")
                        .clicked()
                    {
                        self.download_image(&image);
                    }
                    ui.label(
                        egui::RichText::new(format!("{}×{}", image.w, image.h))
                            .small()
                            .weak(),
                    );
                });
                ui.separator();
                if image.is_video {
                    self.render_video(ui, &image);
                    return;
                }
                let cache_key = format!("original:{}", image.mxc);
                if let Some(handle) = self
                    .media
                    .original_for_source(&cache_key, image.source.clone())
                {
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::Image::new(&handle).shrink_to_fit());
                    });
                } else if let Some(error) = self.media.status(&cache_key).and_then(|e| e.error()) {
                    ui.colored_label(egui::Color32::RED, error);
                    if crate::ui::icon_button(ui, crate::ui::icons::REFRESH, "Retry").clicked() {
                        self.media.retry(&cache_key);
                    }
                } else {
                    ui.spinner();
                    ui.monospace("loading original…");
                }
            });
        if !open {
            self.image_preview = None;
            // Dropping the player kills ffmpeg so audio stops.
            self.video = None;
        }
    }

    /// Play video in the preview window; downloads to temp file first.
    fn render_video(&mut self, ui: &mut egui::Ui, image: &ImageAttachment) {
        let ready = self
            .video_file
            .as_ref()
            .filter(|(mxc, _)| *mxc == image.mxc)
            .map(|(_, path)| path.clone());

        let Some(path) = ready else {
            self.fetch_video(image);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Downloading video…").small().weak());
            });
            return;
        };

        let now = ui.ctx().input(|i| i.time);
        if self.video.is_none() {
            // Sender dims are display size (rotation accounted).
            let hint = (image.w > 0 && image.h > 0).then_some((image.w, image.h));
            match crate::video::VideoPlayer::start(&path, now, hint) {
                Ok(player) => self.video = Some(player),
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Cannot play this video: {e}"),
                    );
                    ui.label(
                        egui::RichText::new(
                            "ffmpeg is required for in-app playback — install it, \
                             or save the file and open it elsewhere.",
                        )
                        .small()
                        .weak(),
                    );
                    return;
                }
            }
        }

        let Some(player) = self.video.as_mut() else {
            return;
        };
        let frame = player.frame(ui.ctx(), now);
        let position = player.position(now);
        let finished = player.finished;
        let paused = player.is_paused();

        match frame {
            Some(handle) => {
                ui.add(egui::Image::new(&handle).shrink_to_fit());
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Decoding…").small().weak());
                });
            }
        }
        let mut toggle = false;
        let mut restart = false;
        ui.horizontal(|ui| {
            use crate::ui::icons;
            // Finished clips offer replay; nothing left to resume.
            if finished {
                if crate::ui::icon_button(ui, icons::REFRESH, "Replay").clicked() {
                    restart = true;
                }
            } else {
                let (icon, tip) = if paused {
                    ("\u{25B6}", "Play")
                } else {
                    ("\u{23F8}", "Pause")
                };
                if ui
                    .add(egui::Button::new(egui::RichText::new(icon).size(15.0)))
                    .on_hover_text(tip)
                    .clicked()
                {
                    toggle = true;
                }
                if crate::ui::icon_button(ui, icons::REFRESH, "Start again").clicked() {
                    restart = true;
                }
            }
            let total = image.duration_ms.map(|ms| ms as f64 / 1000.0);
            let shown = match total {
                Some(t) => format!(
                    "{}:{:02} / {}:{:02}",
                    position as u64 / 60,
                    position as u64 % 60,
                    t as u64 / 60,
                    t as u64 % 60
                ),
                None => format!("{}:{:02}", position as u64 / 60, position as u64 % 60),
            };
            ui.label(egui::RichText::new(shown).small().weak());
        });
        if toggle {
            if let Some(player) = self.video.as_mut() {
                player.toggle_pause(now);
            }
        }
        if restart {
            // No seek on a stream; restart is a fresh decode.
            self.video = None;
        }
        // Repaint while running; paused clips need no frames.
        if !paused {
            ui.ctx().request_repaint();
        }
    }

    /// Download a video to a temp file so ffmpeg can read it.
    fn fetch_video(&mut self, image: &ImageAttachment) {
        // One download at a time; stale mxc results are ignored.
        if self.video_rx.is_some() && self.video_file.is_none() {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.video_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.video_tx = Some(tx);
            self.video_rx = Some(rx);
        }
        let tx = self.video_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        let source = image.source.clone();
        let mxc = image.mxc.clone();
        let name = image.name.clone();
        self.rt.spawn(async move {
            let result = match crate::media_cache::fetch_mxc(&client, source).await {
                Ok(bytes) => {
                    let mut path = std::env::temp_dir();
                    path.push(format!("thrace-{}", sanitise(&name)));
                    std::fs::write(&path, bytes)
                        .map(|()| path)
                        .map_err(|e| format!("write: {e}"))
                }
                Err(e) => Err(e),
            };
            let _ = tx.send((mxc, result));
            ctx.request_repaint();
        });
    }

    fn download_image(&mut self, image: &ImageAttachment) {
        let Some(client) = self.client.clone() else {
            self.status = "log in to download media".into();
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .set_file_name(&image.name)
            .save_file()
        else {
            return;
        };
        let source = image.source.clone();
        self.status = format!("downloading {} …", image.name);
        self.spawn_send(async move {
            match crate::media_cache::fetch_mxc(&client, source).await {
                Ok(bytes) => match std::fs::write(&path, bytes) {
                    Ok(()) => SendResult::Done(format!("saved {}", path.display())),
                    Err(error) => SendResult::Failed(format!("save: {error}")),
                },
                Err(error) => SendResult::Failed(error),
            }
        });
    }

    /// Rich body: markdown/HTML spans with custom emoji resolved.
    fn render_body(&mut self, ui: &mut egui::Ui, body: &str, formatted: Option<&str>) {
        let msg = crate::markdown::render_message(body, formatted);
        for block in &msg.blocks {
            match block {
                crate::markdown::Block::Spans(spans) => {
                    ui.horizontal_wrapped(|ui| {
                        // No inter-widget gap; runs carry their own spaces.
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for span in spans {
                            self.render_span(ui, span);
                        }
                    });
                }
                crate::markdown::Block::Code(cb) => {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        if !cb.lang.is_empty() {
                            ui.monospace(egui::RichText::new(cb.lang.clone()).small().weak());
                        }
                        ui.monospace(&cb.code);
                    });
                }
            }
        }
    }

    /// One styled span: links, accent code, inline custom emoji.
    fn render_span(&mut self, ui: &mut egui::Ui, span: &crate::markdown::Span) {
        // Links/code are never `:shortcode:`; don't split them.
        if span.link.is_some() || span.code {
            self.render_text_run(ui, &span.text, span);
            return;
        }
        // Split `:shortcode:` runs for inline emoji images.
        let mut rest = span.text.as_str();
        let mut first = true;
        while let Some(start) = rest.find(':') {
            let (before, after_start) = rest.split_at(start);
            if !before.is_empty() || first {
                if !before.is_empty() {
                    self.render_text_run(ui, before, span);
                }
                first = false;
            }
            if let Some(end) = after_start[1..].find(':') {
                let sc = &after_start[..end + 2];
                if self.packs.resolve(sc).is_some() {
                    self.render_custom_emoji(ui, sc);
                } else {
                    self.render_text_run(ui, sc, span);
                }
                rest = &after_start[end + 2..];
            } else {
                self.render_text_run(ui, after_start, span);
                rest = "";
                break;
            }
        }
        if !rest.is_empty() {
            self.render_text_run(ui, rest, span);
        }
    }

    fn render_text_run(&mut self, ui: &mut egui::Ui, text: &str, span: &crate::markdown::Span) {
        // Only prose gets emoji lifted into colour images.
        if span.link.is_some() || span.code {
            self.render_plain_run(ui, text, span);
            return;
        }
        // Emoji render slightly larger than body text.
        let size = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
        self.render_text_with_emoji(ui, text, span, size);
    }

    /// Render one run as text, with no emoji substitution.
    fn render_plain_run(&mut self, ui: &mut egui::Ui, text: &str, span: &crate::markdown::Span) {
        if text == "\n" {
            ui.end_row();
            return;
        }
        // Prose is proportional; code and chrome stay monospace.
        let mut rt = if span.code {
            egui::RichText::new(text).monospace()
        } else {
            egui::RichText::new(text)
        };
        if span.bold {
            rt = rt.strong();
        }
        if span.italic {
            rt = rt.italics();
        }
        if span.code {
            rt = rt.background_color(egui::Color32::from_gray(40));
        }
        if let Some(link) = &span.link {
            let link = link.clone();
            // Mention pills open the profile card, not a browser.
            if let Some(mxid) = mention_mxid(&link) {
                let display = self.display_for(&mxid);
                // Resolve bare linkified `@user:server` to a display name.
                let label = if text == mxid {
                    format!("@{display}")
                } else {
                    text.to_owned()
                };
                let colour = self.nick_color(&display);
                let resp = crate::ui::clickable(
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(label)
                                .color(colour)
                                .strong()
                                .background_color(colour.gamma_multiply(0.18)),
                        )
                        .selectable(false)
                        .sense(egui::Sense::click()),
                    ),
                )
                .on_hover_text(format!("{mxid} — view profile"));
                if resp.clicked() {
                    let anchor = ui
                        .ctx()
                        .pointer_latest_pos()
                        .unwrap_or_else(|| resp.rect.right_top());
                    self.profile_target = Some((mxid, anchor, ui.ctx().cumulative_pass_nr()));
                }
                return;
            }
            // Rewrite through the claiming front end; leaks nothing until clicked.
            let target = crate::embed::rewrite(&self.embed_rules, &link).unwrap_or(link);
            ui.hyperlink_to(rt.underline(), target);
        } else if span.code {
            ui.colored_label(egui::Color32::from_rgb(255, 203, 107), rt);
        } else {
            ui.label(rt);
        }
    }

    /// Custom `:shortcode:` emoji as inline image; fetches on first sight.
    fn render_custom_emoji(&mut self, ui: &mut egui::Ui, shortcode: &str) {
        let mxc = self
            .packs
            .resolve(shortcode)
            .map(|i| i.mxc_url.clone())
            .unwrap_or_default();
        if mxc.is_empty() {
            ui.label(egui::RichText::new(shortcode));
            return;
        }
        // Namespaced: same mxc at different sizes stays distinct.
        let key = format!("emoji:{mxc}");
        match (
            self.media.texture_for("emoji", &mxc, Some((24, 24))),
            self.media.status(&key),
        ) {
            (Some(handle), _) => {
                // Hover shows shortcode for discoverability.
                ui.add(egui::Image::new(&handle).max_height(20.0))
                    .on_hover_text(shortcode);
            }
            (None, Some(entry)) if entry.error().is_some() => {
                ui.colored_label(egui::Color32::RED, egui::RichText::new(shortcode).small())
                    .on_hover_text("emoji download failed — click to retry");
            }
            (None, _) => {
                crate::ui::icon_label(ui, crate::ui::icons::IMAGE, self.theme.gold())
                    .on_hover_text(format!("loading {shortcode}…"));
            }
        }
    }
    // Login: password + SSO/OIDC.
    fn start_login(&mut self) {
        if self.login.busy {
            return;
        }
        let hs = self.login.homeserver.trim().to_owned();
        let user = self.login.username.trim().to_owned();
        let pass = self.login.password.clone();
        if hs.is_empty() || user.is_empty() || pass.is_empty() {
            self.login.error = "fill homeserver + username + password, or use SSO".into();
            return;
        }
        self.login.busy = true;
        self.login.error.clear();
        self.status = format!("connecting to {hs} as {user} …");
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        self.spawn_blocking_task(move || {
            let res = login_blocking(hs, user, pass);
            let _ = tx.send(LoginMsg::PasswordDone(res));
        });
    }

    fn start_sso(&mut self, _ctx: &egui::Context) {
        if self.login.busy {
            return;
        }
        let hs = self.login.homeserver.trim().to_owned();
        if hs.is_empty() {
            self.login.error = "set homeserver first".into();
            return;
        }
        self.login.busy = true;
        self.login.error.clear();
        self.login.show_sso_token = false;
        self.status = format!("SSO (PocketID) on {hs} — opening browser …");
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        // One-shot auto flow: worker binds loopback, finishes login, sends Done.
        self.spawn_blocking_task(move || {
            sso_auto_flow(hs, tx);
        });
    }

    fn poll_login(&mut self, ctx: &egui::Context) {
        // Drain pending; auto flow sends UrlReady then Done.
        loop {
            let msg: Option<LoginMsg> = match &self.login_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(msg) = msg else { break };
            match msg {
                LoginMsg::PasswordDone(res) => {
                    self.login.busy = false;
                    self.login_rx = None;
                    match res {
                        Ok(logged) => self.apply_logged_in(logged),
                        Err(e) => {
                            self.login.error = e.clone();
                            self.status = format!("login failed: {e}");
                        }
                    }
                }
                LoginMsg::SsoUrlReady(res) => {
                    match res {
                        Ok((client, url, store_dir)) => {
                            self.pending_sso_client = Some((client, store_dir));
                            self.login.sso_url = url.clone();
                            self.status = "SSO — finish in browser, auto-completing …".into();
                            ctx.open_url(egui::OpenUrl { url, new_tab: true });
                            // Keep rx open; Done follows.
                        }
                        Err(e) => {
                            self.login.busy = false;
                            self.login_rx = None;
                            self.login.error = e.clone();
                            self.status = format!("SSO failed: {e}");
                        }
                    }
                }
                LoginMsg::SsoDone(res) => {
                    self.login.busy = false;
                    self.login_rx = None;
                    match res {
                        Ok(logged) => {
                            self.pending_sso_client = None;
                            self.apply_logged_in(logged);
                        }
                        Err(e) => {
                            self.login.error = e.clone();
                            self.status = format!("SSO failed: {e} — token paste fallback below");
                            self.login.show_sso_token = true;
                        }
                    }
                }
            }
        }
    }

    fn finish_sso_manual(&mut self) {
        // Fallback: paste token when loopback is blocked.
        let Some((client, store_dir)) = self.pending_sso_client.clone() else {
            self.login.error = "hit [sso] first".into();
            return;
        };
        let token = extract_login_token(&self.login.sso_token_input);
        if token.is_empty() {
            self.login.error = "paste loginToken or full callback URL".into();
            return;
        }
        self.login.busy = true;
        let (tx, rx) = std::sync::mpsc::channel();
        self.login_rx = Some(rx);
        self.spawn_blocking_task(move || {
            let res = sso_finish_blocking(client, store_dir, token);
            let _ = tx.send(LoginMsg::SsoDone(res));
        });
    }

    /// Apply theme: reload builtin TOML, apply live.
    fn apply_settings_theme(&mut self, name: &str, ctx: &egui::Context) {
        match ThemeFile::load_builtin(name) {
            Ok(theme) => {
                crate::theme::apply_theme(ctx, &theme);
                self.theme = theme;
                self.settings_theme = name.to_owned();
                self.config_theme = name.to_owned();
                self.status = format!("theme → {name}");
            }
            Err(e) => {
                self.status = format!("theme {name} failed: {e}");
            }
        }
    }

    /// Apply font size via `theme.font.mono_size` + re-apply.
    fn apply_settings_font(&mut self, ctx: &egui::Context) {
        let size = self.settings_font_size.clamp(10.0, 24.0);
        if (self.theme.font.mono_size.unwrap_or(14.0) - size).abs() > f32::EPSILON {
            self.theme.font.mono_size = Some(size);
            crate::theme::apply_theme(ctx, &self.theme);
        }
    }

    /// Logout: stop sync, drop client, wipe session cache.
    fn logout(&mut self) {
        if let Some(task) = self.session_save_task.take() {
            task.abort();
        }
        self.session_save_rx = None;
        self.session_warning = None;
        let client = self.client.clone();
        let metadata = self.session_metadata.take();
        let forget_error = crate::session_store::forget_file(&session_path()).err();
        self.stop_live_sync();
        self.stop_history_loading();
        self.stop_emoji_saving();
        self.stop_media_worker();
        self.last_receipt = None;
        self.own_user.clear();
        self.verify_watch_running = false;
        self.watch_rx = None;
        self.client = None;
        std::rc::Rc::make_mut(&mut self.rows).clear();
        self.members.clear();
        self.login.password.clear();
        self.login.busy = false;
        self.login.error.clear();
        self.show_security = false;
        self.devices.clear();
        self.sas = None;
        self.pending_flow = None;
        self.incoming = None;
        self.status = "logged out — log in to sync".into();
        if let Some(error) = forget_error {
            self.status = format!("Logged out, but could not remove saved metadata: {error:#}");
        }
        self.spawn_send(async move {
            // Wallet cleanup must not wait for a slow or unreachable homeserver.
            let (server, wallet) = tokio::join!(
                async {
                    if let Some(client) = client {
                        tokio::time::timeout(std::time::Duration::from_secs(30), client.logout())
                            .await
                            .map_err(|_| "server logout timed out".to_owned())?
                            .map_err(|error| format!("server logout: {error}"))?;
                    }
                    Ok::<(), String>(())
                },
                async {
                    if let Some(metadata) = metadata {
                        crate::session_store::delete_tokens(&metadata)
                            .await
                            .map_err(|error| format!("wallet cleanup: {error:#}"))?;
                    }
                    Ok::<(), String>(())
                },
            );
            let errors: Vec<_> = [server, wallet]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            if errors.is_empty() {
                SendResult::Done(String::new())
            } else {
                SendResult::Failed(errors.join("; "))
            }
        });
    }

    fn apply_logged_in(&mut self, logged: LoggedIn) {
        self.stop_history_loading();
        self.stop_emoji_saving();
        self.status = logged
            .persistence_warning
            .clone()
            .unwrap_or_else(|| format!("{} — {} rooms", logged.user_id, logged.rooms.len()));
        self.session_metadata = logged.session_metadata;
        self.session_warning = logged.persistence_warning;
        self.login.homeserver = logged.client.homeserver().to_string();
        self.own_user = logged.user_id.clone();
        self.emoji_usage = logged.emoji_usage;
        self.recent_emoji = self.emoji_usage.frequent(18);
        // Display names + DM flag; sort rooms then DMs.
        let mut rooms: Vec<RoomEntry> = logged
            .rooms
            .iter()
            .map(|r| RoomEntry {
                room_id: r.room_id.clone(),
                name: if r.name.is_empty() {
                    short_room(&r.room_id)
                } else {
                    r.name.clone()
                },
                unread: 0,
                mentioned: false,
                is_dm: r.is_dm,
                avatar_mxc: r.avatar_mxc.clone(),
                preview: None,
            })
            .collect();
        rooms.sort_by(|a, b| {
            a.is_dm
                .cmp(&b.is_dm)
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if rooms.is_empty() {
            self.status = "no rooms yet — join one with /join #alias:hs".into();
        }
        self.rooms = rooms;
        self.current = 0;
        self.timelines.clear();
        self.back_tokens.clear();
        self.members_by_room.clear();
        self.members.clear();
        self.rows = Default::default();
        self.history_queue = crate::history_queue::HistoryQueue::new(
            self.rooms.iter().map(|room| room.room_id.clone()),
        );
        // Merge real packs after demo pack (first-wins).
        for pack in logged.packs.packs().iter().cloned() {
            self.packs.upsert_pack(pack);
        }
        // Prime media queue with custom emoji so picker/chat don't stall.
        for pack in self.packs.packs() {
            for img in &pack.images {
                self.media
                    .texture_for("emoji", &img.mxc_url, Some((24, 24)));
            }
        }
        self.client = Some(logged.client);
        // Fresh login: reset watcher + de-dupe set.
        self.verify_watch_running = false;
        self.watch_rx = None;
        self.seen_incoming.clear();
        self.incoming = None;
        // Fresh login: restart live-sync loop.
        self.stop_live_sync();
        // Media worker holds the old client; restart it too.
        self.stop_media_worker();
        self.last_receipt = None;
    }

    /// Cancel requests and discard old channels when changing accounts.
    fn stop_history_loading(&mut self) {
        for (_, task) in self.history_tasks.drain() {
            task.abort();
        }
        self.history_queue = Default::default();
        self.history_rx = None;
        self.history_tx = None;
        self.page_rx = None;
        self.page_tx = None;
        self.paginating = None;
        self.pending_scroll_fix = None;
    }

    /// Fill a small background queue, keeping a slot for the selected room.
    fn pump_history(&mut self) {
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
    fn poll_history(&mut self) {
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
            if is_current {
                self.send_current_read_receipt();
            }
        }
    }

    /// Abort the live-sync task and drop its channel.
    fn stop_live_sync(&mut self) {
        if let Some(task) = self.sync_task.take() {
            task.abort();
        }
        self.sync_running = false;
        self.sync_rx = None;
    }

    /// Drop media worker channels; `ensure_media_worker` rebuilds it.
    fn stop_media_worker(&mut self) {
        self.media_tx = None;
        self.media_rx = None;
        self.media_req_tx = None;
    }

    /// Live sync loop: decode `/sync` batches off-thread, pump to `sync_rx`.
    fn ensure_live_sync(&mut self, _ctx: &egui::Context) {
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

    /// Drain live-sync batches into rooms; bump unread for background rooms.
    fn poll_live_sync(&mut self) {
        loop {
            let batch = match &self.sync_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(batch) = batch else { break };
            self.apply_sync_batch(batch);
        }
    }

    fn apply_sync_batch(&mut self, mut batch: SyncBatch) {
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
                batch.had_current_rows = true;
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
        // New rows in view: mark read.
        if batch.had_current_rows {
            self.send_current_read_receipt();
        }
    }

    /// Rewrite a message in place from an `m.replace`.
    fn apply_edit(&mut self, room_id: &str, target: &str, body: String, formatted: Option<String>) {
        let cur_id = self.rooms.get(self.current).map(|r| r.room_id.clone());
        if cur_id.as_deref() == Some(room_id) {
            edit_in(
                std::rc::Rc::make_mut(&mut self.rows).as_mut_slice(),
                target,
                body,
                formatted,
            );
        } else if let Some(tl) = self.timelines.get_mut(room_id) {
            edit_in(tl, target, body, formatted);
        }
    }

    /// Apply one `m.room.redaction`: drop reaction or tombstone message.
    fn apply_redaction(&mut self, room_id: &str, redacted: &str) {
        let cur_id = self.rooms.get(self.current).map(|r| r.room_id.clone());
        if cur_id.as_deref() == Some(room_id) {
            redact_in(
                std::rc::Rc::make_mut(&mut self.rows).as_mut_slice(),
                redacted,
            );
        } else if let Some(tl) = self.timelines.get_mut(room_id) {
            redact_in(tl, redacted);
        }
    }

    fn bump_unread(&mut self, room_id: &str, mentioned: bool) {
        if let Some(room) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
            room.unread += 1;
            room.mentioned |= mentioned;
        }
    }

    /// Refresh a room's one-line preview from its newest real message.
    fn refresh_preview(&mut self, room_id: &str, row: &TimelineRow) {
        if row.sender == "system" {
            return;
        }
        let text = format!("{}: {}", row.display_name, snippet(&row.body, 48));
        if let Some(room) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
            room.preview = Some(text);
        }
    }

    /// Fold one wire reaction into its target; ignore unknown targets.
    fn apply_sync_reaction(&mut self, room_id: &str, react: SyncReaction) {
        let cur_id = self.rooms.get(self.current).map(|r| r.room_id.clone());
        if cur_id.as_deref() == Some(room_id) {
            fold_reaction(std::rc::Rc::make_mut(&mut self.rows).as_mut_slice(), &react);
        } else if let Some(tl) = self.timelines.get_mut(room_id) {
            fold_reaction(tl, &react);
        }
    }

    /// Fold one `m.read` receipt into its target; receipts move forward.
    fn apply_sync_receipt(&mut self, room_id: &str, event_id: &str, user_id: &str) {
        let cur_id = self.rooms.get(self.current).map(|r| r.room_id.clone());
        if cur_id.as_deref() == Some(room_id) {
            seen_in(
                std::rc::Rc::make_mut(&mut self.rows).as_mut_slice(),
                event_id,
                user_id,
            );
        } else if let Some(tl) = self.timelines.get_mut(room_id) {
            seen_in(tl, event_id, user_id);
        }
    }

    /// Send read receipt for the newest real row in view.
    fn send_current_read_receipt(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        let Some(target) = self
            .rows
            .iter()
            .rev()
            .find(|r| !r.id.starts_with("local-") && r.sender != "system" && r.id.starts_with('$'))
            .map(|r| r.id.clone())
        else {
            return;
        };
        // One receipt per target event, not per batch.
        if self.last_receipt.as_deref() == Some(target.as_str()) {
            return;
        }
        self.last_receipt = Some(target.clone());
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
    fn switch_room(&mut self, idx: usize) {
        if idx >= self.rooms.len() {
            return;
        }
        // Drop unused DM stubs so misclicks leave no dead entries.
        let idx = match self.take_empty_dm_stub(idx) {
            Some(adjusted) => adjusted,
            None => return,
        };
        // Stash outgoing timeline + members.
        if let Some(cur) = self.rooms.get(self.current).map(|r| r.room_id.clone()) {
            self.timelines.insert(
                cur.clone(),
                std::mem::take(std::rc::Rc::make_mut(&mut self.rows)),
            );
            self.members_by_room
                .insert(cur, std::mem::take(&mut self.members));
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
        // Viewing marks read.
        self.send_current_read_receipt();
    }

    /// Drop unused DM stub; return `idx` adjusted (`None` if target vanished).
    fn take_empty_dm_stub(&mut self, idx: usize) -> Option<usize> {
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

    /// Load our most recent real message into the composer for editing.
    fn start_edit_last(&mut self) {
        let own = self.own_user.clone();
        let Some(row) = self.rows.iter().rev().find(|r| {
            r.sender == own && r.id.starts_with('$') && r.image.is_none() && !r.is_sticker
        }) else {
            self.status = "nothing of yours to edit yet".into();
            return;
        };
        self.input = row.body.clone();
        self.editing = Some(row.id.clone());
        self.replying_to = None;
    }

    /// Send an `m.replace` for one of our messages.
    fn send_edit(&mut self, target: String, text: String) {
        // Local echo; sync confirms.
        if let Some(row) = std::rc::Rc::make_mut(&mut self.rows)
            .iter_mut()
            .find(|r| r.id == target)
        {
            row.body = text.clone();
            row.formatted = None;
            row.edited = true;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(target) else {
                return SendResult::Failed("bad event id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            // `body` is the fallback; `m.new_content` is the replacement.
            let content = serde_json::json!({
                "msgtype": "m.text",
                "body": format!("* {text}"),
                "m.new_content": { "msgtype": "m.text", "body": text },
                "m.relates_to": { "rel_type": "m.replace", "event_id": eid },
            });
            match room.send_raw("m.room.message", content).await {
                Ok(_) => SendResult::Done("edited".into()),
                Err(e) => SendResult::Failed(format!("edit: {e}")),
            }
        });
    }

    /// Click a user → open existing DM or stub a new one.
    fn open_dm(&mut self, mxid: &str, display: &str) {
        if let Some(idx) = self
            .rooms
            .iter()
            .position(|r| r.is_dm && (r.name == display || r.name == mxid || r.room_id == mxid))
        {
            self.switch_room(idx);
            return;
        }
        // No DM room: stub locally so chat switches now.
        // TODO: create_dm(mxid), replace stub with real room id.
        let stub_id = format!("dm:{mxid}");
        self.timelines.insert(
            self.rooms[self.current].room_id.clone(),
            std::mem::take(std::rc::Rc::make_mut(&mut self.rows)),
        );
        self.members_by_room.insert(
            self.rooms[self.current].room_id.clone(),
            std::mem::take(&mut self.members),
        );
        self.rooms.push(RoomEntry {
            room_id: stub_id.clone(),
            name: format!("@{display}"),
            unread: 0,
            mentioned: false,
            is_dm: true,
            // Search every loaded room; profile cards appear anywhere.
            avatar_mxc: self
                .members
                .iter()
                .chain(self.members_by_room.values().flatten())
                .find(|m| m.mxid == mxid)
                .and_then(|m| m.avatar_mxc.clone()),
            preview: None,
        });
        self.current = self.rooms.len() - 1;
        self.rows = std::rc::Rc::new(vec![TimelineRow {
            id: "dm-new".into(),
            ts: format_ts(now_millis()),
            sender: "system".into(),
            display_name: "system".into(),
            body: format!("Direct message with {display} ({mxid}) — say hi"),
            formatted: None,
            avatar_mxc: None,
            reply_to: None,
            reply_to_id: None,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: None,
            reactions: vec![],
            is_sticker: false,
            txn_id: None,
        }]);
        self.members = vec![Member {
            display: display.into(),
            mxid: mxid.into(),
            online: true,
            avatar_mxc: None,
        }];
    }

    // Verification (SAS).
    fn refresh_devices(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let user = if self.verify_user_input.trim().is_empty() {
            match client.user_id() {
                Some(u) => u.to_owned(),
                None => return,
            }
        } else {
            match matrix_sdk::ruma::OwnedUserId::try_from(self.verify_user_input.trim()) {
                Ok(u) => u,
                Err(_) => {
                    self.status = "bad @user:hs".into();
                    return;
                }
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.verify_rx = Some(rx);
        self.status = format!("loading devices for {user} …");
        self.spawn_task(async move {
            match crate::verify::list_devices(&client, &user).await {
                Ok(devs) => {
                    let _ = tx.send(crate::verify::VerifyEvent::Devices(devs));
                }
                Err(e) => {
                    let _ = tx.send(crate::verify::VerifyEvent::Error(e));
                }
            }
        });
    }

    fn start_verify_device(&mut self, user_id: &str, device_id: &str) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.verify_rx = Some(rx);
        self.sas = None;
        self.pending_flow = None;
        self.status =
            format!("requesting verification {user_id}:{device_id} … accept on other device");
        let user: matrix_sdk::ruma::OwnedUserId = match user_id.try_into() {
            Ok(u) => u,
            Err(_) => {
                self.status = "bad user id".into();
                return;
            }
        };
        let dev: matrix_sdk::ruma::OwnedDeviceId = device_id.into();
        self.spawn_task(async move {
            if let Err(e) = crate::verify::outgoing_verify_to_device(client, &user, &dev, tx).await
            {
                // Forward error explicitly.
                let _ = std::sync::mpsc::channel::<crate::verify::VerifyEvent>()
                    .0
                    .send(crate::verify::VerifyEvent::Error(e));
            }
        });
    }

    /// Background watcher: poll for incoming verification requests every 5s.
    fn ensure_verify_watch(&mut self, ctx: &egui::Context) {
        if self.verify_watch_running || self.client.is_none() {
            return;
        }
        self.verify_watch_running = true;
        let client = self.client.clone().unwrap();
        // Dedicated watch_rx coexists with one-shot verify_rx flows.
        let (tx, rx) = std::sync::mpsc::channel();
        self.watch_rx = Some(rx);
        let ctx = ctx.clone();
        self.spawn_task(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                // Client replaced: exit; fresh watcher starts.
                if client.user_id().is_none() {
                    break;
                }
                match crate::verify::poll_incoming_request(&client).await {
                    Some(req) => {
                        let _ = tx.send(crate::verify::IncomingRequest {
                            flow_id: req.flow_id,
                            user_id: req.user_id,
                            device_id: req.device_id,
                        });
                        ctx.request_repaint();
                    }
                    None => continue,
                }
            }
        });
    }

    fn accept_incoming(&mut self) {
        let Some(req) = self.incoming.clone() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let Ok(user) = matrix_sdk::ruma::OwnedUserId::try_from(req.user_id.clone()) else {
            self.status = "bad user id in verification request".into();
            return;
        };
        self.incoming = None;
        self.show_security = true;
        self.sas = None;
        self.pending_flow = None;
        self.status = format!("accepting verification from {} …", req.user_id);
        let (tx, rx) = std::sync::mpsc::channel();
        self.verify_rx = Some(rx);
        let flow = req.flow_id.clone();
        self.spawn_task(async move {
            if let Err(e) =
                crate::verify::accept_incoming_request(client, &user, &flow, tx.clone()).await
            {
                let _ = tx.send(crate::verify::VerifyEvent::Error(e));
            }
        });
    }

    fn decline_incoming(&mut self) {
        let Some(req) = self.incoming.take() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        self.status = format!("declined verification from {}", req.user_id);
        let flow = req.flow_id.clone();
        let user_s = req.user_id.clone();
        self.spawn_task(async move {
            if let Ok(user) = matrix_sdk::ruma::OwnedUserId::try_from(user_s) {
                let _ = crate::verify::decline_incoming_request(&client, &user, &flow).await;
            }
        });
    }

    fn poll_verify(&mut self) {
        // Watcher speaks IncomingRequest on its own channel.
        loop {
            let req = match &self.watch_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(req) = req else { break };
            // De-dupe re-scanned flows.
            if self.seen_incoming.insert(req.flow_id.clone()) {
                self.incoming = Some(req);
                self.status = "incoming verification — accept?".into();
            }
        }
        // Drain pending; one-shot flows share verify_rx.
        loop {
            let msg = match &self.verify_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(msg) = msg else { break };
            match msg {
                crate::verify::VerifyEvent::Devices(devs) => {
                    self.devices = devs;
                    self.status = format!("{} device(s)", self.devices.len());
                }
                crate::verify::VerifyEvent::SasReady { flow_id, sas } => {
                    self.pending_flow = Some(flow_id);
                    self.sas = Some(sas);
                    self.show_security = true;
                    self.status = "SAS ready — compare emojis on BOTH devices".into();
                }
                crate::verify::VerifyEvent::SasDone { .. } => {
                    self.sas = None;
                    self.pending_flow = None;
                    self.status = "verification complete — shields green".into();
                    self.refresh_devices();
                }
                crate::verify::VerifyEvent::Incoming(req) => {
                    // Legacy path; kept for forward-compat.
                    if self.seen_incoming.insert(req.flow_id.clone()) {
                        self.incoming = Some(req);
                        self.status = "incoming verification — accept?".into();
                    }
                }
                crate::verify::VerifyEvent::IncomingAccepted { flow_id } => {
                    self.status = format!("accepted {flow_id} — waiting for SAS …");
                }
                crate::verify::VerifyEvent::Error(e) => {
                    self.status = format!("verify: {e}");
                }
            }
        }
    }
    fn verify_confirm(&mut self, matched: bool) {
        let Some(flow) = self.pending_flow.clone() else {
            return;
        };
        let Some(sas) = crate::verify::take_pending_sas(&flow) else {
            self.status = "SAS expired — restart verify".into();
            return;
        };
        self.status = if matched {
            "confirming match …".into()
        } else {
            "reporting mismatch …".into()
        };
        self.spawn_task(async move {
            let _ = if matched {
                sas.confirm().await
            } else {
                sas.mismatch().await
            };
        });
        self.sas = None;
        self.pending_flow = None;
    }

    fn send_current_input(&mut self) {
        // In-progress edit consumes Enter; rewrites instead of sending.
        if let Some(target) = self.editing.take() {
            let text = self.input.trim().to_owned();
            self.input.clear();
            if !text.is_empty() {
                self.send_edit(target, text);
            }
            return;
        }
        // Uploads send first, then text; one Enter sends all.
        if !self.pending_uploads.is_empty() {
            let uploads = std::mem::take(&mut self.pending_uploads);
            // Sent: drop staging thumbnails.
            self.upload_thumbs.clear();
            for up in uploads {
                self.send_upload(up);
            }
        }
        let text = self.input.trim().to_owned();
        if text.is_empty() {
            self.input.clear();
            self.slash_selected = 0;
            return;
        }
        // Slash commands echo effects only, never raw text.
        if let Some(cmd) = text.strip_prefix('/') {
            let mut parts = cmd.splitn(2, ' ');
            let verb = parts.next().unwrap_or("").to_lowercase();
            let args = parts.next().unwrap_or("").to_owned();
            match verb.as_str() {
                "me" => self.send_emote(args),
                "react" => {
                    let key = if args.is_empty() { "+1".into() } else { args };
                    self.send_reaction_to_last(key);
                }
                "reply" => {
                    let target = self.rows.last().map(|r| r.id.clone());
                    if let Some(id) = target {
                        self.replying_to = Some(id);
                        if !args.is_empty() {
                            self.send_text(args);
                        }
                    }
                }
                "sticker" => self.send_sticker(args),
                "shrug" => self.send_text(format!("{} ¯\\_(ツ)_/¯", args)),
                "join" => self.room_action(args, RoomAction::Join),
                "invite" => self.room_action(args, RoomAction::Invite),
                "topic" => self.room_action(args, RoomAction::Topic),
                "nick" => self.set_nick(args),
                "leave" | "part" => self.room_action(String::new(), RoomAction::Leave),
                "roomname" => self.set_room_name(args),
                "plain" => self.send_plain(args),
                "spoiler" => self.send_spoiler(args),
                "kick" => self.moderate(args, Moderation::Kick),
                "ban" => self.moderate(args, Moderation::Ban),
                "unban" => self.moderate(args, Moderation::Unban),
                "op" => self.moderate(args, Moderation::Power),
                "deop" => self.moderate(format!("{args} 0"), Moderation::Power),
                "ignore" => self.moderate(args, Moderation::Ignore),
                "unignore" => self.moderate(args, Moderation::Unignore),
                "help" => {
                    let n = self.rows.len();
                    std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
                        id: format!("local-{n}"),
                        ts: format_ts(now_millis()),
                        sender: "system".into(),
                        display_name: "system".into(),
                        body: SLASH_COMMANDS
                            .iter()
                            .map(|(c, h)| format!("{c} — {h}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        formatted: None,
                        avatar_mxc: None,
                        reply_to: None,
                        reply_to_id: None,
                        thread_count: 0,
                        edited: false,
                        seen_by: Vec::new(),
                        image: None,
                        reactions: vec![],
                        is_sticker: false,
                        txn_id: None,
                    });
                }
                _ => {
                    let n = self.rows.len();
                    std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
                        id: format!("local-{n}"),
                        ts: format_ts(now_millis()),
                        sender: "system".into(),
                        display_name: "system".into(),
                        body: format!("Unknown command /{verb} — try /help"),
                        formatted: None,
                        avatar_mxc: None,
                        reply_to: None,
                        reply_to_id: None,
                        thread_count: 0,
                        edited: false,
                        seen_by: Vec::new(),
                        image: None,
                        reactions: vec![],
                        is_sticker: false,
                        txn_id: None,
                    });
                }
            }
        } else {
            self.send_text(text);
        }
        self.input.clear();
        self.slash_selected = 0;
    }

    /// Pick files via native dialog; stage above input.
    fn render_embeds(&mut self, ui: &mut egui::Ui, body: &str) {
        // Bail out fast when there are no links.
        if !body.contains("http") {
            return;
        }
        let urls: Vec<String> = body
            .split_whitespace()
            .filter(|w| w.starts_with("http"))
            .map(|w| w.trim_end_matches(['.', ',', ')', ']']).to_owned())
            .collect();
        for url in urls {
            if crate::embed::card_rule(&self.embed_rules, &url).is_none() {
                continue;
            }
            match self.embeds.get(&url).cloned() {
                Some(embed) => self.render_embed_card(ui, &embed),
                None => {
                    self.fetch_embed(url);
                }
            }
        }
    }

    /// One link card: author, text and a thumbnail.
    fn render_embed_card(&mut self, ui: &mut egui::Ui, embed: &crate::embed::Embed) {
        let accent = self.theme.gold();
        let surface = ui.visuals().widgets.hovered.bg_fill;
        ui.add_space(3.0);
        egui::Frame::new()
            .fill(surface)
            .corner_radius(egui::CornerRadius::same(9))
            .inner_margin(egui::Margin::symmetric(10, 9))
            .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.28)))
            .show(ui, |ui| {
                // Bound to available width so narrow windows don't overflow.
                ui.set_max_width(ui.available_width().min(430.0));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    // Author avatar, fetched like other card images.
                    let avatar = embed
                        .avatar
                        .as_ref()
                        .and_then(|src| self.embed_image(ui.ctx(), src));
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    match avatar {
                        Some(handle) => {
                            let mut mesh = egui::Mesh::with_texture(handle.id());
                            // Circular, like other avatars.
                            let (c, r) = (rect.center(), rect.width() * 0.5);
                            mesh.vertices.push(egui::epaint::Vertex {
                                pos: c,
                                uv: egui::pos2(0.5, 0.5),
                                color: egui::Color32::WHITE,
                            });
                            const SEG: usize = 20;
                            for i in 0..=SEG {
                                let a = i as f32 / SEG as f32 * std::f32::consts::TAU;
                                let (s, co) = a.sin_cos();
                                mesh.vertices.push(egui::epaint::Vertex {
                                    pos: c + egui::vec2(co * r, s * r),
                                    uv: egui::pos2(0.5 + co * 0.5, 0.5 + s * 0.5),
                                    color: egui::Color32::WHITE,
                                });
                            }
                            for i in 1..=SEG as u32 {
                                mesh.indices.extend_from_slice(&[0, i, i + 1]);
                            }
                            ui.painter().add(egui::Shape::mesh(mesh));
                        }
                        None => {
                            ui.painter().circle_filled(
                                rect.center(),
                                rect.width() * 0.5,
                                accent.gamma_multiply(0.25),
                            );
                        }
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(egui::RichText::new(&embed.author).strong().size(13.5));
                        // Handle uses theme accent.
                        ui.label(egui::RichText::new(&embed.handle).size(11.5).color(accent));
                    });
                });
                ui.add_space(5.0);
                let text = embed.text.clone();
                self.emoji_text(ui, &text, 13.0, false);
                if let Some(src) = embed.image.clone() {
                    if let Some(handle) = self.embed_image(ui.ctx(), &src) {
                        ui.add_space(6.0);
                        // Bound to card width for narrow layouts.
                        let w = ui.available_width().min(410.0);
                        ui.add(
                            egui::Image::new(&handle)
                                .max_size(egui::vec2(w, 280.0))
                                .corner_radius(egui::CornerRadius::same(6)),
                        );
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    crate::ui::icon_label(
                        ui,
                        crate::ui::icons::LINK,
                        ui.visuals().weak_text_color(),
                    );
                    ui.hyperlink_to(
                        egui::RichText::new("Open original").small().weak(),
                        embed.link.clone(),
                    );
                });
            });
    }

    /// Start fetching a card, once per URL.
    fn fetch_embed(&mut self, url: String) {
        if !self.embeds.claim(&url) {
            return;
        }
        let Some(rule) = crate::embed::card_rule(&self.embed_rules, &url).cloned() else {
            return;
        };
        if self.embed_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.embed_tx = Some(tx);
            self.embed_rx = Some(rx);
        }
        let tx = self.embed_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let result = crate::embed::fetch(&rule, &url).await.ok();
            let _ = tx.send((url, result));
            ctx.request_repaint();
        });
    }

    /// Thumbnail for a card. These are https URLs, not mxc, so they bypass
    /// the Matrix media cache entirely.
    fn embed_image(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        if let Some(cached) = self.embed_images.get(url) {
            return cached.clone();
        }
        // Mark in flight: fetch once, don't retry failures per frame.
        self.embed_images.insert(url.to_owned(), None);
        if self.embed_img_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.embed_img_tx = Some(tx);
            self.embed_img_rx = Some(rx);
        }
        let tx = self.embed_img_tx.clone()?;
        let ctx = ctx.clone();
        let target = url.to_owned();
        self.rt.spawn(async move {
            let image = crate::embed::fetch_image(&target).await.ok();
            let _ = tx.send((target, image));
            ctx.request_repaint();
        });
        None
    }

    /// Staged-upload thumbnail, decoded once and cached.
    fn upload_thumb(
        &mut self,
        ctx: &egui::Context,
        up: &PendingUpload,
    ) -> Option<egui::TextureHandle> {
        if let Some(cached) = self.upload_thumbs.get(&up.name) {
            return cached.clone();
        }
        let decoded = up.is_image().then(|| {
            // Small budget: 28px chip from possibly huge source.
            crate::media_cache::decode_image(&up.bytes, 160_000).map(|img| {
                ctx.load_texture(
                    format!("upload:{}", up.name),
                    img,
                    egui::TextureOptions::LINEAR,
                )
            })
        });
        let handle = decoded.flatten();
        self.upload_thumbs.insert(up.name.clone(), handle.clone());
        handle
    }

    /// Stage clipboard image/files as uploads. Reads clipboard directly (egui paste is text-only).
    fn paste_clipboard(&mut self, announce_empty: bool) {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                if announce_empty {
                    self.status = format!("clipboard unavailable: {e}");
                }
                return;
            }
        };
        // Raw bitmap first.
        if let Ok(img) = clipboard.get_image() {
            let (w, h) = (img.width, img.height);
            match encode_clipboard_png(&img) {
                Ok((name, bytes)) => match PendingUpload::from_bytes(name, bytes) {
                    Ok(up) => {
                        self.status = format!("pasted {} ({w}×{h})", up.name);
                        self.pending_uploads.push(up);
                    }
                    Err(e) => self.status = e.to_string(),
                },
                Err(e) => self.status = format!("paste: {e}"),
            }
            return;
        }
        // Else `file://` URIs / paths as text (file-manager copies).
        if let Ok(text) = clipboard.get_text() {
            if self.stage_clipboard_paths(&text) > 0 {
                return;
            }
        }
        if announce_empty {
            self.status = "nothing to paste — clipboard has no image or file".into();
        }
    }

    /// Stage `file://` URIs / absolute paths from clipboard text.
    fn stage_clipboard_paths(&mut self, text: &str) -> usize {
        let mut staged = 0;
        for line in text.lines() {
            let line = line.trim();
            let path = if line.starts_with("file://") {
                match url::Url::parse(line)
                    .ok()
                    .and_then(|u| u.to_file_path().ok())
                {
                    Some(p) => p,
                    None => continue,
                }
            } else if line.starts_with('/') {
                std::path::PathBuf::from(line)
            } else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_owned();
            match std::fs::read(&path) {
                Ok(bytes) => match PendingUpload::from_bytes(name.clone(), bytes) {
                    Ok(up) => {
                        self.pending_uploads.push(up);
                        staged += 1;
                    }
                    Err(e) => self.status = e.to_string(),
                },
                Err(e) => self.status = format!("cannot read {name}: {e}"),
            }
        }
        if staged > 0 {
            self.status = format!("pasted {staged} file(s)");
        }
        staged
    }

    fn pick_files(&mut self) {
        let paths = rfd::FileDialog::new().pick_files().unwrap_or_default();
        for path in paths {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_owned();
            match std::fs::read(&path) {
                Ok(bytes) => match PendingUpload::from_bytes(name.clone(), bytes) {
                    Ok(up) => self.pending_uploads.push(up),
                    Err(e) => self.status = e.to_string(),
                },
                Err(e) => self.status = format!("cannot read {name}: {e}"),
            }
        }
    }

    /// OS drag-and-drop files; same cap + staging as picker.
    fn ingest_dropped(&mut self, files: Vec<egui::DroppedFile>) {
        for f in files {
            let name = f
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if f.name.is_empty() {
                        "dropped-file".into()
                    } else {
                        f.name.clone()
                    }
                });
            let bytes = if let Some(path) = &f.path {
                match std::fs::read(path) {
                    Ok(b) => b,
                    Err(e) => {
                        self.status = format!("cannot read {name}: {e}");
                        continue;
                    }
                }
            } else if let Some(bytes) = f.bytes.as_ref() {
                bytes.to_vec()
            } else {
                self.status = format!("cannot read {name}: no data");
                continue;
            };
            match PendingUpload::from_bytes(name.clone(), bytes) {
                Ok(up) => self.pending_uploads.push(up),
                Err(e) => self.status = e.to_string(),
            }
        }
    }

    /// Upload one staged file via `send_attachment`; images as `m.image`, rest as `m.file`.
    fn send_upload(&mut self, up: PendingUpload) {
        // Txn-stamped echo; sync arrival replaces it.
        let txn = format!("rs{}", Self::uuid_txn());
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: txn.clone(),
            ts: format_ts(now_millis()),
            sender: "@you:hs".into(),
            display_name: "you".into(),
            body: format!("▲ {} ({} KB) …", up.name, up.bytes.len() / 1024),
            formatted: None,
            avatar_mxc: None,
            reply_to: None,
            reply_to_id: None,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: None,
            reactions: vec![],
            is_sticker: false,
            txn_id: Some(txn.clone()),
        });
        let Some(client) = self.client.clone() else {
            self.status = "log in to upload".into();
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            self.status = "log in to upload".into();
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedRoomId, UInt};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let mime: mime::Mime = up.mime.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM);
            let config = if up.is_image() {
                // Probe dims for `info`; failure still sends.
                match image::load_from_memory(&up.bytes) {
                    Ok(img) => {
                        let (w, h) = (img.width(), img.height());
                        matrix_sdk::attachment::AttachmentConfig::new().info(
                            matrix_sdk::attachment::AttachmentInfo::Image(
                                matrix_sdk::attachment::BaseImageInfo {
                                    width: Some(UInt::new(w as u64).unwrap_or(UInt::MAX)),
                                    height: Some(UInt::new(h as u64).unwrap_or(UInt::MAX)),
                                    size: Some(
                                        UInt::new(up.bytes.len() as u64).unwrap_or(UInt::MAX),
                                    ),
                                    ..Default::default()
                                },
                            ),
                        )
                    }
                    Err(_) => matrix_sdk::attachment::AttachmentConfig::new(),
                }
            } else {
                matrix_sdk::attachment::AttachmentConfig::new()
            };
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.clone().into();
            match room
                .send_attachment(&up.name, &mime, up.bytes, config.txn_id(txn_id))
                .await
            {
                Ok(_) => SendResult::Done(format!("sent {}", up.name)),
                Err(e) => SendResult::Failed(format!("upload {}: {e}", up.name)),
            }
        });
    }

    /// Current room id (None when logged out / no rooms yet).
    fn current_room_id(&self) -> Option<String> {
        let id = self.rooms.get(self.current)?.room_id.clone();
        if id.starts_with("dm:") && self.client.is_none() {
            return None;
        }
        Some(id)
    }

    /// Lazily create the send channel.
    fn send_channel(&mut self) -> std::sync::mpsc::Sender<SendResult> {
        if self.send_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.send_tx = Some(tx.clone());
            self.send_rx = Some(rx);
        }
        self.send_tx.clone().unwrap()
    }

    /// Queue a send worker; failures surface in status (no unsend).
    fn spawn_send<F>(&mut self, fut: F)
    where
        F: Future<Output = SendResult> + Send + 'static,
    {
        let tx = self.send_channel();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let res = fut.await;
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    /// Run a future on the runtime; repaint when done.
    fn spawn_task<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            fut.await;
            ctx.request_repaint();
        });
    }

    /// Run a blocking closure on the blocking pool; repaint after.
    fn spawn_blocking_task<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            f();
            ctx.request_repaint();
        });
    }

    fn poll_send(&mut self) {
        loop {
            let msg = match &self.send_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(msg) = msg else { break };
            match msg {
                SendResult::Done(t) => {
                    if !t.is_empty() {
                        self.status = t;
                    }
                }
                SendResult::Failed(e) => {
                    self.status = format!("send failed: {e}");
                }
            }
        }
    }

    /// Lazily create the media channel + spawn the download worker.
    fn ensure_media_worker(&mut self) {
        if self.media_tx.is_some() || self.client.is_none() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.media_tx = Some(tx);
        self.media_rx = Some(rx);
        // Worker input arrives over a second channel.
        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::media_cache::MediaFetch>();
        self.media_req_tx = Some(req_tx);
        let client = self.client.clone().unwrap();
        let out = self.media_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            // Bounded fan-out; slow requests hold one permit and time out.
            let permits = Arc::new(tokio::sync::Semaphore::new(MEDIA_CONCURRENCY));
            while let Some(fetch) = req_rx.recv().await {
                let Ok(permit) = permits.clone().acquire_owned().await else {
                    break;
                };
                let client = client.clone();
                let out = out.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let msg = crate::media_cache::fetch_and_decode(&client, fetch).await;
                    if out.send(msg).is_ok() {
                        // Wake UI; texture upload happens in `poll_media`.
                        ctx.request_repaint();
                    }
                });
            }
        });
        // Re-queue `Pending` from a dead worker so images don't stick as grey boxes.
        self.media.requeue_pending();
    }
    /// Forward queued downloads; ingest finished bytes as textures.
    fn poll_media(&mut self, ctx: &egui::Context) {
        // No worker: return drained queue rather than dropping it.
        let now_ms = (ctx.input(|i| i.time) * 1000.0) as u32;
        if self.media.tick(now_ms) {
            // Animating: keep frames coming.
            ctx.request_repaint();
        }
        self.ensure_media_worker();
        // Re-queue failures whose backoff elapsed.
        self.media.retry_due();
        let queued = self.media.take_queued();
        if !queued.is_empty() {
            match &self.media_req_tx {
                Some(tx) => {
                    for f in queued {
                        let _ = tx.send(f);
                    }
                }
                None => self.media.unqueue(queued),
            }
        }
        let mut uploaded = 0usize;
        loop {
            let msg = match &self.media_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some(msg) = msg else { break };
            self.media.ingest(ctx, msg);
            uploaded += 1;
            if uploaded >= MEDIA_UPLOADS_PER_FRAME {
                // Spread bursts over frames; ask for the next one.
                ctx.request_repaint();
                break;
            }
        }
    }

    /// Nanos-based client txn id (unique per send in this process).
    fn uuid_txn() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{n:x}")
    }

    /// Plain message (or reply): local echo + `room.send` on a worker.
    fn send_text(&mut self, text: String) {
        let reply = self.replying_to.take().map(|id| ("you".into(), id));
        let (plain, html) = self.packs.proxied_bodies(&text);
        let formatted = html.clone();
        // Txn id on echo and send; sync echo replaces this row.
        let txn = format!("rs{}", Self::uuid_txn());
        self.push_local(
            "@you:hs",
            "you",
            text,
            reply.clone(),
            formatted.clone(),
            Some(txn.clone()),
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            self.status = "log in to send".into();
            return;
        };
        let reply_id = reply.map(|(_, id)| id);
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
            let mut content = match html {
                Some(h) => RoomMessageEventContent::text_html(plain, h),
                None => RoomMessageEventContent::text_plain(plain),
            };
            if let Some(target) = reply_id {
                if let (Ok(target_id), Some(own)) = (
                    OwnedEventId::try_from(target),
                    client.user_id().map(|u| u.to_owned()),
                ) {
                    content = content.make_reply_to(
                        matrix_sdk::ruma::events::room::message::ReplyMetadata::new(
                            &target_id, &own, None,
                        ),
                        matrix_sdk::ruma::events::room::message::ForwardThread::Yes,
                        matrix_sdk::ruma::events::room::message::AddMentions::No,
                    );
                }
            }
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.clone().into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    fn send_emote(&mut self, text: String) {
        let (plain, html) = self.packs.proxied_bodies(&text);
        let txn = format!("rs{}", Self::uuid_txn());
        self.push_local(
            "* you",
            "you",
            plain.clone(),
            None,
            html.clone(),
            Some(txn.clone()),
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            let Ok(rid) = matrix_sdk::ruma::OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
            let content = match html {
                Some(h) => RoomMessageEventContent::emote_html(plain, h),
                None => RoomMessageEventContent::emote_plain(plain),
            };
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.clone().into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    fn send_reaction_to_last(&mut self, key: String) {
        let Some(target) = self.rows.last().map(|r| r.id.clone()) else {
            return;
        };
        let own = self
            .client
            .as_ref()
            .and_then(|c| c.user_id())
            .map(|u| u.to_string())
            .unwrap_or("you".into());
        if let Some(last) = std::rc::Rc::make_mut(&mut self.rows).last_mut() {
            Self::bump_reaction(last, key.clone(), &own);
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(target) else {
                return SendResult::Done("reaction sent".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            use matrix_sdk::ruma::events::reaction::ReactionEventContent;
            use matrix_sdk::ruma::events::relation::Annotation;
            let content = ReactionEventContent::new(Annotation::new(eid, key));
            match room.send(content).await {
                Ok(_) => SendResult::Done("reaction sent".into()),
                Err(e) => SendResult::Failed(format!("reaction: {e}")),
            }
        });
    }

    /// Reaction toggle from the timeline.
    fn send_reaction(&mut self, ri: usize, key: String) {
        let Some(target) = self.rows.get(ri).map(|r| r.id.clone()) else {
            return;
        };
        // MSC4027: custom-emoji reactions travel as mxc URI + shortcode label.
        let custom = self
            .packs
            .resolve(&key)
            .map(|i| (i.mxc_url.clone(), key.trim_matches(':').to_owned()));
        let (key, shortcode) = match custom {
            Some((mxc, sc)) => (mxc, Some(sc)),
            None => (key, None),
        };
        let own = self
            .client
            .as_ref()
            .and_then(|c| c.user_id())
            .map(|u| u.to_string())
            .unwrap_or("you".into());
        // Already reacted: this click takes it back (redact own event).
        let mine = self
            .rows
            .get(ri)
            .and_then(|r| r.reactions.iter().find(|g| g.key == key))
            .and_then(|g| g.event_of(&own).map(str::to_owned));
        let owns_pending = self
            .rows
            .get(ri)
            .and_then(|r| r.reactions.iter().find(|g| g.key == key))
            .is_some_and(|g| g.owns(&own));
        if owns_pending && mine.is_none() {
            self.status = "reaction still sending — try again in a moment".into();
            return;
        }
        if let Some(reaction_event) = mine {
            self.unreact(ri, &key, &own, reaction_event);
            return;
        }
        if let Some(row) = std::rc::Rc::make_mut(&mut self.rows).get_mut(ri) {
            Self::bump_reaction(row, key.clone(), &own);
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(target) else {
                return SendResult::Done("reaction sent".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            // Raw send: ruma has no room for the MSC4027 `shortcode` field.
            let mut content = serde_json::json!({
                "m.relates_to": {
                    "rel_type": "m.annotation",
                    "event_id": eid,
                    "key": key,
                }
            });
            if let Some(sc) = shortcode {
                content["shortcode"] = serde_json::Value::String(sc);
            }
            match room.send_raw("m.reaction", content).await {
                Ok(_) => SendResult::Done("reaction sent".into()),
                Err(e) => SendResult::Failed(format!("reaction: {e}")),
            }
        });
    }

    fn send_sticker(&mut self, shortcode: String) {
        let sc = if shortcode.is_empty() {
            "party".into()
        } else {
            shortcode
        };
        let mxc = self
            .packs
            .resolve(&format!(":{sc}:"))
            .map(|i| i.mxc_url.clone());
        // Keep target id only; renderer resolves sender + snippet from live row.
        let reply_to_id = self.replying_to.take();
        let n = self.rows.len();
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: format!("local-{n}"),
            ts: format_ts(now_millis()),
            sender: "@you:hs".into(),
            display_name: "you".into(),
            body: format!("Sticker :{sc}:"),
            formatted: None,
            avatar_mxc: None,
            reply_to: None,
            reply_to_id,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: mxc.clone().map(|m| ImageAttachment {
                source: plain_media_source(&m),
                thumbnail_source: None,
                is_video: false,
                duration_ms: None,
                mxc: m,
                name: sc.clone(),
                w: 0,
                h: 0,
            }),
            reactions: vec![],
            is_sticker: true,
            txn_id: None,
        });
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        let Some(mxc) = mxc else {
            self.status = format!("no sticker :{sc}: in any pack");
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedMxcUri, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id.clone()) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let url: OwnedMxcUri = mxc.clone().into();
            use matrix_sdk::ruma::events::sticker::StickerEventContent;
            let content = StickerEventContent::new(sc.clone(), Default::default(), url);
            match room.send(content).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(format!("sticker: {e}")),
            }
        });
    }

    fn room_action(&mut self, args: String, action: RoomAction) {
        let Some(client) = self.client.clone() else {
            self.status = "log in first".into();
            return;
        };
        let cur = self.current_room_id();
        self.status = "working …".into();
        self.spawn_send(async move {
            let res: Result<String, String> = async {
                match action {
                    RoomAction::Join => {
                        use matrix_sdk::ruma::OwnedRoomOrAliasId;
                        let Ok(id) = OwnedRoomOrAliasId::try_from(args.clone()) else {
                            return Err("usage: /join #alias:hs or !room:hs".into());
                        };
                        client
                            .join_room_by_id_or_alias(&id, &[])
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok("joined".into())
                    }
                    RoomAction::Invite => {
                        let Some(rid_s) = cur else {
                            return Err("no room".into());
                        };
                        use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
                        let rid = OwnedRoomId::try_from(rid_s).map_err(|e| e.to_string())?;
                        let uid = OwnedUserId::try_from(args.clone())
                            .map_err(|_| "usage: /invite @u:hs".to_string())?;
                        let Some(room) = client.get_room(&rid) else {
                            return Err("room not found".into());
                        };
                        room.invite_user_by_id(&uid)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok("invited".into())
                    }
                    RoomAction::Topic => {
                        let Some(rid_s) = cur else {
                            return Err("no room".into());
                        };
                        use matrix_sdk::ruma::OwnedRoomId;
                        let rid = OwnedRoomId::try_from(rid_s).map_err(|e| e.to_string())?;
                        let Some(room) = client.get_room(&rid) else {
                            return Err("room not found".into());
                        };
                        room.set_room_topic(&args)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok("topic set".into())
                    }
                    RoomAction::Leave => {
                        let Some(rid_s) = cur else {
                            return Err("no room".into());
                        };
                        use matrix_sdk::ruma::OwnedRoomId;
                        let rid = OwnedRoomId::try_from(rid_s).map_err(|e| e.to_string())?;
                        let Some(room) = client.get_room(&rid) else {
                            return Err("room not found".into());
                        };
                        room.leave().await.map_err(|e| e.to_string())?;
                        Ok("left".into())
                    }
                }
            }
            .await;
            match res {
                Ok(t) => SendResult::Done(t),
                Err(e) => SendResult::Failed(e),
            }
        });
    }

    /// Run a moderation command (real server call).
    fn moderate(&mut self, args: String, what: Moderation) {
        let mut parts = args.trim().splitn(2, ' ');
        let target = parts.next().unwrap_or("").to_owned();
        let extra = parts.next().unwrap_or("").trim().to_owned();
        if target.is_empty() {
            self.status = "usage: /<command> @user:server [reason or level]".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.status = format!("working on {target} …");
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
            let Ok(user) = OwnedUserId::try_from(target.clone()) else {
                return SendResult::Failed(format!("{target} is not a user id"));
            };
            let reason = (!extra.is_empty()).then_some(extra.as_str());
            if let Moderation::Ignore | Moderation::Unignore = what {
                let res = match what {
                    Moderation::Ignore => client.account().ignore_user(&user).await,
                    _ => client.account().unignore_user(&user).await,
                };
                return match res {
                    Ok(()) => SendResult::Done(format!("done — {target}")),
                    Err(e) => SendResult::Failed(format!("{e}")),
                };
            }
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let res = match what {
                Moderation::Kick => room.kick_user(&user, reason).await,
                Moderation::Ban => room.ban_user(&user, reason).await,
                Moderation::Unban => room.unban_user(&user, reason).await,
                Moderation::Power => {
                    // `/op` with no level means moderator.
                    let level: i64 = extra.parse().unwrap_or(50);
                    // `Int` converts fallibly, not via `From`.
                    let Ok(level) = matrix_sdk::ruma::Int::try_from(level) else {
                        return SendResult::Failed("power level out of range".into());
                    };
                    room.update_power_levels(vec![(&user, level)])
                        .await
                        .map(|_| ())
                }
                _ => unreachable!("ignore handled above"),
            };
            match res {
                Ok(()) => SendResult::Done(format!("done — {target}")),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// `/roomname` — rename the current room.
    fn set_room_name(&mut self, name: String) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.status = "usage: /roomname New name".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            match room.set_name(name).await {
                Ok(_) => SendResult::Done("room renamed".into()),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// `/plain` — send text with markdown left literal.
    fn send_plain(&mut self, text: String) {
        if text.trim().is_empty() {
            self.status = "usage: /plain some *literal* text".into();
            return;
        }
        self.send_text(text);
    }

    /// `/spoiler` — send hidden behind a spoiler.
    fn send_spoiler(&mut self, text: String) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            self.status = "usage: /spoiler the secret".into();
            return;
        }
        let html = format!("<span data-mx-spoiler>{}</span>", html_escape(&text));
        let txn = format!("rs{}", Self::uuid_txn());
        self.push_local(
            "@you:hs",
            "you",
            text.clone(),
            None,
            Some(html.clone()),
            Some(txn.clone()),
        );
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
            use matrix_sdk::ruma::OwnedRoomId;
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let content = RoomMessageEventContent::text_html(text, html);
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = txn.into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => SendResult::Done("sent".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    fn set_nick(&mut self, nick: String) {
        if nick.trim().is_empty() {
            self.status = "usage: /nick NewName".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.status = "setting display name …".into();
        self.spawn_send(async move {
            match client.account().set_display_name(Some(&nick)).await {
                Ok(_) => SendResult::Done("display name set".into()),
                Err(e) => SendResult::Failed(e.to_string()),
            }
        });
    }

    fn push_local(
        &mut self,
        sender: &str,
        display: &str,
        body: String,
        reply: Option<(String, String)>,
        formatted: Option<String>,
        txn_id: Option<String>,
    ) {
        // Resolve reply id to snippet.
        let reply_to_id = reply.as_ref().map(|(_, id)| id.clone());
        let reply_to = reply
            .as_ref()
            .and_then(|(_, id)| {
                self.rows
                    .iter()
                    .find(|r| r.id == *id)
                    .map(|r| (r.display_name.clone(), snippet(&r.body, 60)))
            })
            .or(reply);
        let n = self.rows.len();
        std::rc::Rc::make_mut(&mut self.rows).push(TimelineRow {
            id: txn_id.clone().unwrap_or_else(|| format!("local-{n}")),
            ts: format_ts(now_millis()),
            sender: sender.into(),
            display_name: display.into(),
            body,
            formatted,
            avatar_mxc: None,
            reply_to,
            reply_to_id,
            thread_count: 0,
            edited: false,
            seen_by: Vec::new(),
            image: None,
            reactions: vec![],
            is_sticker: false,
            txn_id,
        });
    }

    /// Take back our reaction by redacting its event; echoes locally.
    fn unreact(&mut self, ri: usize, key: &str, own: &str, reaction_event: String) {
        if let Some(row) = std::rc::Rc::make_mut(&mut self.rows).get_mut(ri) {
            if let Some(group) = row.reactions.iter_mut().find(|g| g.key == key) {
                group.senders.retain(|s| s.user != own);
            }
            row.reactions.retain(|g| !g.senders.is_empty());
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(reaction_event) else {
                return SendResult::Failed("bad reaction id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            match room.redact(&eid, None, None).await {
                Ok(_) => SendResult::Done("reaction removed".into()),
                Err(e) => SendResult::Failed(format!("un-react: {e}")),
            }
        });
    }

    /// Local echo for our reaction; add-only, un-react redacts.
    fn bump_reaction(row: &mut TimelineRow, key: String, own_user: &str) {
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
            });
        }
    }

    /// Slash commands matching the typed verb. `None` when the palette stays hidden.
    fn slash_matches(&self) -> Option<(String, Vec<(&'static str, &'static str)>)> {
        let rest = self.input.strip_prefix('/')?;
        let verb = rest.split_whitespace().next().unwrap_or("").to_lowercase();
        let matches: Vec<(&str, &str)> = SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd[1..].starts_with(&verb))
            .take(8)
            .map(|(a, b)| (*a, *b))
            .collect();
        (!matches.is_empty()).then_some((verb, matches))
    }

    /// Replace the typed verb with `cmd`, keeping any arguments.
    ///
    /// Enter must honor the highlight, not send the raw text.
    fn complete_slash(&mut self, cmd: &str) {
        let args = self
            .input
            .split_once(' ')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default();
        self.input = if args.is_empty() {
            format!("{cmd} ")
        } else {
            format!("{cmd} {args}")
        };
        self.slash_selected = 0;
    }

    /// Apply a sidebar room action.
    fn room_menu_action(&mut self, room_id: String, action: RoomMenuAction) {
        // Local echo so the sidebar reacts instantly.
        match action {
            RoomMenuAction::MarkRead => {
                if let Some(r) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
                    r.unread = 0;
                    r.mentioned = false;
                }
            }
            RoomMenuAction::MarkUnread => {
                if let Some(r) = self.rooms.iter_mut().find(|r| r.room_id == room_id) {
                    r.unread = r.unread.max(1);
                }
            }
            RoomMenuAction::CopyLink => {
                // matrix.to is the portable form every client understands.
                let link = format!("https://matrix.to/#/{room_id}");
                self.status =
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(link.clone())) {
                        Ok(()) => format!("copied {link}"),
                        Err(e) => format!("copy failed: {e}"),
                    };
                return;
            }
            RoomMenuAction::Invite => {
                // Needs a user id; route through the composer instead of guessing.
                self.input = "/invite ".into();
                self.status = "type the user id to invite, then Enter".into();
                return;
            }
            RoomMenuAction::Leave => {
                self.rooms.retain(|r| r.room_id != room_id);
                self.timelines.remove(&room_id);
                self.members_by_room.remove(&room_id);
                if self.current >= self.rooms.len() {
                    self.current = self.rooms.len().saturating_sub(1);
                }
                std::rc::Rc::make_mut(&mut self.rows).clear();
                self.members.clear();
            }
            _ => {}
        }

        let Some(client) = self.client.clone() else {
            return;
        };
        self.spawn_send(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            let result = match action {
                RoomMenuAction::MarkRead => room.set_unread_flag(false).await,
                RoomMenuAction::MarkUnread => room.set_unread_flag(true).await,
                RoomMenuAction::Favourite(on) => room.set_is_favourite(on, None).await,
                RoomMenuAction::Leave => room.leave().await,
                RoomMenuAction::Notify(mode) => {
                    return match client
                        .notification_settings()
                        .await
                        .set_room_notification_mode(&rid, mode.sdk())
                        .await
                    {
                        Ok(()) => SendResult::Done(format!("notifications: {}", mode.label())),
                        Err(e) => SendResult::Failed(format!("{e}")),
                    };
                }
                // Handled locally above.
                RoomMenuAction::Invite | RoomMenuAction::CopyLink => {
                    return SendResult::Done(String::new())
                }
            };
            match result {
                Ok(()) => SendResult::Done(String::new()),
                Err(e) => SendResult::Failed(format!("{e}")),
            }
        });
    }

    /// Fetch the next older page once initial history has arrived.
    fn paginate_current(&mut self) {
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        if self.paginating.is_some() || !self.history_queue.is_loaded(&room_id) {
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
        self.paginating = Some(room_id.clone());
        self.rt.spawn(async move {
            use matrix_sdk::ruma::OwnedRoomId;
            let rows_and_token = match OwnedRoomId::try_from(room_id.clone())
                .ok()
                .and_then(|rid| client.get_room(&rid))
            {
                Some(room) => {
                    load_room_history(&room, &avatars, Some(token), INITIAL_HISTORY).await
                }
                None => Err("room not found".into()),
            };
            let _ = tx.send((room_id, rows_and_token));
            ctx.request_repaint();
        });
    }

    /// Prepend an arriving older page above the visible rows.
    fn apply_page(&mut self, room_id: String, mut page: Vec<TimelineRow>, token: Option<String>) {
        self.paginating = None;
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

    /// Delete a message by redacting it; a refused redact surfaces instead of failing silently.
    fn delete_message(&mut self, event_id: String) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(room_id) = self.current_room_id() else {
            return;
        };
        // Local echo; the sync redaction confirms it.
        redact_in(
            std::rc::Rc::make_mut(&mut self.rows).as_mut_slice(),
            &event_id,
        );
        self.spawn_send(async move {
            use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
            let Ok(rid) = OwnedRoomId::try_from(room_id) else {
                return SendResult::Failed("bad room id".into());
            };
            let Ok(eid) = OwnedEventId::try_from(event_id) else {
                return SendResult::Failed("bad event id".into());
            };
            let Some(room) = client.get_room(&rid) else {
                return SendResult::Failed("room not found".into());
            };
            match room.redact(&eid, None, None).await {
                Ok(_) => SendResult::Done("message deleted".into()),
                Err(e) => SendResult::Failed(format!("delete: {e}")),
            }
        });
    }
}

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
fn split_reply_fallback(body: &str) -> (Option<(String, String)>, String) {
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

/// Extract the mxid from a user link: matrix.to pill (often percent-encoded)
/// or `matrix:u/…`. Room and alias links return `None`.
fn mention_mxid(link: &str) -> Option<String> {
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

fn percent_decode(s: &str) -> String {
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

/// Count rooms a user shares with us. `members_by_room` excludes the room on
/// screen, whose list lives in `members`.
fn count_shared_rooms(
    here: &[Member],
    by_room: &std::collections::HashMap<String, Vec<Member>>,
    mxid: &str,
) -> usize {
    usize::from(here.iter().any(|m| m.mxid == mxid))
        + by_room
            .values()
            .filter(|ms| ms.iter().any(|m| m.mxid == mxid))
            .count()
}

/// Fold one wire reaction into its target row. Unknown target: not paged in yet.
fn fold_reaction(rows: &mut [TimelineRow], react: &SyncReaction) -> bool {
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
        }),
    }
    true
}

/// Rewrite one message from an `m.replace`. Unknown target: not paged in, dropped.
fn edit_in(
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

/// Move one person's read receipt onto `event_id`. Unknown target (they read
/// newer than paged in): ignored, so no avatar disappears.
fn seen_in(rows: &mut [TimelineRow], event_id: &str, user_id: &str) -> bool {
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
const DELETED_BODY: &str = "Message deleted";

/// Apply a redaction: drop the named reaction, or tombstone the message.
fn redact_in(rows: &mut [TimelineRow], redacted: &str) {
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
        row.reactions.clear();
    }
}

/// Outcome of folding a synced row into a timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowMerge {
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
fn merge_row(rows: &mut Vec<TimelineRow>, mut row: TimelineRow) -> RowMerge {
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
        if new.reactions.is_empty() {
            new.reactions = old.reactions.clone();
        }
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
fn merge_initial_history(target: &mut Vec<TimelineRow>, history: Vec<TimelineRow>) {
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
fn mentions_user(row: &TimelineRow, own: &str) -> bool {
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

/// Does the clipboard hold a bitmap? Distinguishes the text half of an image
/// copy from a real text paste.
fn clipboard_has_image() -> bool {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_image())
        .is_ok()
}

/// Encode a clipboard bitmap (RGBA8) as PNG for upload.
fn encode_clipboard_png(img: &arboard::ImageData<'_>) -> Result<(String, Vec<u8>), String> {
    use image::ImageEncoder;
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 {
        return Err("clipboard image is empty".into());
    }
    let expected = img.width.saturating_mul(img.height).saturating_mul(4);
    if img.bytes.len() < expected {
        return Err(format!(
            "clipboard image truncated ({} of {expected} bytes)",
            img.bytes.len()
        ));
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &img.bytes[..expected],
            w,
            h,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("encode: {e}"))?;
    Ok((format!("pasted-{}.png", ThraceApp::uuid_txn()), png))
}

/// Label for a reaction key: unicode speaks for itself, mxc (MSC4027) gets
/// the pack shortcode when known.
fn reaction_label(key: &str, packs: &PackStore) -> String {
    if !key.starts_with("mxc://") {
        // Return the name, never the glyph, or the tooltip duplicates the chip image as text.
        let name = crate::emoji::name_of(key);
        return if name.is_empty() {
            key.trim_matches(':').to_owned()
        } else {
            name.to_owned()
        };
    }
    packs
        .packs()
        .iter()
        .flat_map(|p| p.images.iter())
        .find(|i| i.mxc_url == key)
        .map(|i| format!(":{}:", i.shortcode))
        .unwrap_or_else(|| "custom emoji".to_owned())
}

/// Timeline events a single `/sync` may carry per room. The initial sync is
/// unbounded server-side and dominates first-paint cost.
const SYNC_TIMELINE_LIMIT: u32 = 20;

/// Sync filter: lazy-loaded members and a bounded timeline. Without it the
/// server sends every member event per room on initial sync, which we discard.
fn sync_filter() -> matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter {
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

/// Wall-clock now, in the same units the server stamps events with.
fn now_millis() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Format `origin_server_ts` as a local stamp: `HH:MM` today, dated otherwise.
fn format_ts(millis: Option<u64>) -> String {
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
fn mention_prefix(input: &str) -> Option<&str> {
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

/// Escape text for inclusion in `formatted_body`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Make a filename safe for the temp directory. Bodies can carry slashes,
/// `..`, anything the sender chose; none of it may steer the path.
fn sanitise(name: &str) -> String {
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

fn snippet(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Char-boundary-safe truncation with `…`. `snippet` slices raw bytes; names
/// are arbitrary unicode, so count chars.
fn truncate_name(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

fn short_room(room_id: &str) -> String {
    if room_id.starts_with('!') {
        // No name yet; show a compact id prefix until sync fills names.
        let end = room_id.find(':').unwrap_or(room_id.len().min(12));
        room_id[..end.min(room_id.len())].to_owned()
    } else {
        room_id.to_owned()
    }
}

impl eframe::App for ThraceApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Ctrl+V in two halves: a browser image copy puts bitmap + URL on the
        // clipboard, egui-winit emits `Paste`, and the composer would swallow
        // the URL. On the press frame drop `Paste` when an image is present.
        let text_paste = ui
            .ctx()
            .input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        if text_paste && clipboard_has_image() {
            ui.ctx()
                .input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Paste(_))));
        }
        // No `Key::V` press is ever emitted (egui-winit returns early), and an
        // image-only clipboard emits no `Paste`; the release triggers the attach.
        let v_released = ui.ctx().input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command
                )
            })
        });
        if v_released {
            // Quiet: a plain text paste must not overwrite the status bar.
            self.paste_clipboard(false);
        }
        if let Some(rx) = &self.ignored_rx {
            if let Ok(list) = rx.try_recv() {
                self.ignored_users = list;
            }
        }
        // Link cards and thumbnails.
        loop {
            let got = match &self.embed_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some((url, embed)) = got else { break };
            self.embeds.insert(url, embed);
        }
        loop {
            let got = match &self.embed_img_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some((url, image)) = got else { break };
            let handle = image.map(|img| {
                ui.ctx()
                    .load_texture(&url, img, egui::TextureOptions::LINEAR)
            });
            self.embed_images.insert(url, handle);
        }
        // Older pages arriving from scrollback.
        loop {
            let page = match &self.page_rx {
                Some(rx) => rx.try_recv().ok(),
                None => None,
            };
            let Some((room_id, result)) = page else { break };
            match result {
                Ok((rows, token)) => self.apply_page(room_id, rows, token),
                Err(error) => {
                    self.paginating = None;
                    self.status = error;
                }
            }
        }
        // Downloaded video files for ffmpeg.
        if let Some(rx) = &self.video_rx {
            if let Ok((mxc, result)) = rx.try_recv() {
                match result {
                    Ok(path) => {
                        self.video = None;
                        self.video_file = Some((mxc, path));
                    }
                    Err(e) => self.status = format!("video: {e}"),
                }
            }
        }
        self.poll_login(ui.ctx());
        self.poll_verify();
        self.poll_send();
        self.poll_media(ui.ctx());
        self.poll_live_sync();
        self.poll_history();
        self.pump_history();
        // Incoming verification sync; starts once per login.
        self.ensure_verify_watch(ui.ctx());
        self.ensure_live_sync(ui.ctx());
        if let Some(result) = self
            .session_save_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.session_save_rx = None;
            self.session_save_task = None;
            match result {
                Ok(()) => {
                    self.session_warning = None;
                    self.status = "Login saved in wallet".into();
                }
                Err(error) => self.session_warning = Some(error),
            }
        }
        if let Some(warning) = self.session_warning.clone() {
            egui::Panel::top("session_storage_warning").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                    if ui
                        .add_enabled(
                            self.session_save_rx.is_none(),
                            egui::Button::new("Retry saving login"),
                        )
                        .clicked()
                    {
                        self.retry_session_save();
                    }
                });
            });
        }
        if self.client.is_none() {
            egui::Panel::top("login").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.monospace("Login");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.login.homeserver)
                            .hint_text("https://matrix.org")
                            .desired_width(160.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.login.username)
                            .hint_text("@you:hs")
                            .desired_width(130.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.login.password)
                            .hint_text("password")
                            .password(true)
                            .desired_width(110.0),
                    );
                    if ui
                        .add_enabled(!self.login.busy, egui::Button::new("Connect"))
                        .clicked()
                    {
                        self.start_login();
                    }
                    if ui
                        .add_enabled(!self.login.busy, egui::Button::new("Sso"))
                        .clicked()
                    {
                        let ctx = ui.ctx().clone();
                        self.start_sso(&ctx);
                    }
                    if self.login.busy {
                        ui.monospace("… SSO: browser → auto back, no paste");
                    }
                });
                // Manual fallback: only when loopback failed.
                if self.login.show_sso_token {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace("fallback token:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.login.sso_token_input)
                                .hint_text("loginToken or callback URL")
                                .desired_width(300.0),
                        );
                        if ui
                            .add_enabled(!self.login.busy, egui::Button::new("Finish"))
                            .clicked()
                        {
                            self.finish_sso_manual();
                        }
                    });
                }
                if !self.login.error.is_empty() {
                    ui.colored_label(egui::Color32::RED, egui::RichText::new(&self.login.error));
                }
            });
        }

        // Fake titlebar + security + settings.
        egui::Panel::top("titlebar").show(ui, |ui| {
            use crate::ui::icons;
            ui.horizontal(|ui| {
                if crate::ui::icon_button(
                    ui,
                    if self.sidebar_collapsed {
                        icons::CHEVRON_RIGHT
                    } else {
                        icons::CHEVRON_LEFT
                    },
                    if self.sidebar_collapsed {
                        "Expand sidebar"
                    } else {
                        "Collapse sidebar"
                    },
                )
                .clicked()
                {
                    self.sidebar_collapsed = !self.sidebar_collapsed;
                }
                let room = self.rooms.get(self.current);
                let name = room.map(|r| r.name.clone()).unwrap_or_else(|| "—".into());
                let is_dm = room.is_some_and(|r| r.is_dm);
                let muted = ui.visuals().weak_text_color();
                crate::ui::icon_label(ui, if is_dm { icons::DM } else { icons::HASH }, muted);
                ui.label(egui::RichText::new(name).strong().size(15.0));
                if !self.members.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("{} members", self.members.len()))
                            .small()
                            .weak(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // No fake window buttons; the WM draws the real ones.
                    if crate::ui::icon_toggle(ui, icons::COG, "Settings", self.show_settings)
                        .clicked()
                    {
                        self.show_settings = !self.show_settings;
                        if self.show_settings {
                            // Seed editable fields from current state.
                            let me = self.own_user.clone();
                            self.settings_display_name = self.display_for(&me);
                            self.refresh_devices();
                            self.refresh_ignored();
                        }
                    }
                    if crate::ui::icon_toggle(
                        ui,
                        icons::SHIELD,
                        "Security and device verification",
                        self.show_security,
                    )
                    .clicked()
                    {
                        self.show_security = !self.show_security;
                        if self.show_security {
                            self.refresh_devices();
                        }
                    }
                });
            });
        });

        // Verification panel (Matrix spec SAS).
        if self.show_security {
            egui::Panel::top("security").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.monospace("[security] SAS verify per Matrix spec");
                    ui.add(egui::TextEdit::singleline(&mut self.verify_user_input).hint_text("@user:hs (blank = me)").desired_width(180.0));
                    if ui.small_button("Load devices").clicked() {
                        self.refresh_devices();
                    }
                    if ui.small_button("× [hide]").clicked() {
                        self.show_security = false;
                    }
                });
                ui.monospace("1. [verify] on a device → 2. accept on OTHER device → 3. compare 7 emoji + 3 numbers BOTH sides → [match] / [no match]");
                let mut to_verify: Option<(String, String)> = None;
                for d in self.devices.clone() {
                    ui.horizontal_wrapped(|ui| {
                        let shield = if d.verified { "●" } else { "○" };
                        let own = if d.is_own { " (this account)" } else { "" };
                        ui.monospace(format!("{shield} {}:{}{} {}", d.user_id, d.device_id, own, d.display_name.unwrap_or_default()));
                        if !d.verified && ui.small_button("Verify").clicked() {
                            to_verify = Some((d.user_id.clone(), d.device_id.clone()));
                        }
                    });
                }
                if let Some((u, dev)) = to_verify {
                    self.start_verify_device(&u, &dev);
                }
                if let Some(sas) = &self.sas {
                    ui.separator();
                    ui.monospace("DO THEY MATCH on both devices? If yes [match], else [no match]:");
                    ui.horizontal_wrapped(|ui| {
                        for (sym, desc) in &sas.emojis {
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(sym).size(28.0));
                                ui.monospace(egui::RichText::new(desc).small().weak());
                            });
                        }
                    });
                    ui.monospace(format!("numbers: {} – {} – {}", sas.decimals.0, sas.decimals.1, sas.decimals.2));
                    ui.horizontal(|ui| {
                        if ui.button("Match").clicked() {
                            self.verify_confirm(true);
                        }
                        if ui.button("No match").clicked() {
                            self.verify_confirm(false);
                        }
                    });
                }
            });
        }
        // Settings popup: theme, font size, session.
        if self.show_settings {
            let ctx = ui.ctx().clone();
            let mut close = false;
            let mut logout = false;
            let mut theme_pick: Option<String> = None;
            let mut open = true;
            crate::ui::popup(&ctx, "Settings")
                .open(&mut open)
                .default_size(egui::vec2(580.0, 460.0))
                .show(&ctx, |ui| {
                    ui.horizontal_top(|ui| {
                        // Section list down the left.
                        ui.vertical(|ui| {
                            ui.set_width(150.0);
                            for tab in SettingsTab::ALL {
                                let selected = self.settings_tab == tab;
                                let row = ui.horizontal(|ui| {
                                    crate::ui::icon_label(
                                        ui,
                                        tab.icon(),
                                        if selected {
                                            ui.visuals().selection.stroke.color
                                        } else {
                                            ui.visuals().weak_text_color()
                                        },
                                    );
                                    ui.add(
                                        egui::Label::new(if selected {
                                            egui::RichText::new(tab.label()).strong()
                                        } else {
                                            egui::RichText::new(tab.label())
                                        })
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                    )
                                    .clicked()
                                });
                                // Key by tab: auto ids from layout position can collide.
                                let whole_row = crate::ui::clickable(ui.interact(
                                    row.response.rect,
                                    ui.id().with(("settings-tab", tab.label())),
                                    egui::Sense::click(),
                                ));
                                if row.inner || whole_row.clicked() {
                                    self.settings_tab = tab;
                                }
                            }
                            ui.add_space(8.0);
                            ui.separator();
                            let danger = ui.visuals().error_fg_color;
                            if ui
                                .add(
                                    egui::Label::new(egui::RichText::new("Sign out").color(danger))
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                )
                                .clicked()
                            {
                                logout = true;
                            }
                        });
                        ui.separator();
                        ui.vertical(|ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_min_width(360.0);
                                    match self.settings_tab {
                                        SettingsTab::Account => self.settings_account(ui),
                                        SettingsTab::Appearance => {
                                            theme_pick = self.settings_appearance(ui)
                                        }
                                        SettingsTab::Notifications => {
                                            self.settings_notifications(ui)
                                        }
                                        SettingsTab::Sessions => self.settings_sessions(ui),
                                        SettingsTab::Privacy => self.settings_privacy(ui),
                                        SettingsTab::Emoji => self.settings_emoji(ui),
                                        SettingsTab::Links => self.settings_links(ui),
                                    }
                                });
                        });
                    });
                });
            if !open {
                close = true;
            }
            if let Some(name) = theme_pick {
                self.apply_settings_theme(&name, ui.ctx());
            }
            // Font slider applies live.
            self.apply_settings_font(ui.ctx());
            self.save_preferences();
            if logout {
                self.logout();
                close = true;
            }
            if close {
                self.show_settings = false;
            }
        }
        // Incoming verification: accept compares SAS emoji in security; decline cancels.
        if self.incoming.is_some() {
            let ctx = ui.ctx().clone();
            let mut accept = false;
            let mut decline = false;
            let (user, flow) = self
                .incoming
                .as_ref()
                .map(|r| (r.user_id.clone(), r.flow_id.clone()))
                .unwrap();
            let mut open = true;
            crate::ui::popup(&ctx, "Verify device")
                .open(&mut open)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(egui::RichText::new(format!("{user} wants to verify")).strong());
                    ui.label(
                        egui::RichText::new(
                            "Accept, then compare the seven emoji and three numbers on \
                             both devices.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.label(
                        egui::RichText::new(format!("Session {flow}"))
                            .small()
                            .weak(),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Accept").clicked() {
                            accept = true;
                        }
                        if ui.button("Decline").clicked() {
                            decline = true;
                        }
                    });
                });
            if accept {
                self.accept_incoming();
            } else if decline || !open {
                // Closing the window declines: leaving a verification request
                // half-open on the other device is worse than a clear no.
                self.decline_incoming();
            }
        }

        // Compact room sidebar (rooms + DMs).
        if !self.sidebar_collapsed {
            egui::Panel::left("rooms")
                .default_size(200.0)
                .min_size(140.0)
                .max_size(320.0)
                .show(ui, |ui| {
                    let mut clicked: Option<usize> = None;
                    let mut menu_for: Option<String> = None;
                    let accent = self.theme.gold();
                    let rooms = self.rooms.clone();
                    let cur = self.current;

                    // One closure for both lists, so Rooms and Direct stay in sync.
                    let previews = self.show_previews;
                    // Two lines need more height.
                    let row_h = if previews { 44.0 } else { 32.0 };
                    let mut avatar_jobs: Vec<(egui::Rect, Option<String>, String)> = Vec::new();
                    // Previews paint after the closure; the row closure lacks `&mut self`.
                    let mut previews_to_paint: Vec<(egui::Pos2, String, f32)> = Vec::new();

                    let mut section = |ui: &mut egui::Ui,
                                   title: &str,
                                   dm: bool,
                                   jobs: &mut Vec<(egui::Rect, Option<String>, String)>| {
                    let items: Vec<(usize, &RoomEntry)> = rooms
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.is_dm == dm)
                        .collect();
                    if items.is_empty() {
                        return;
                    }
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(title).small().weak().strong());
                    ui.add_space(2.0);
                    for (i, room) in items {
                        let selected = i == cur;
                        let resp = ui.add(
                            egui::Button::new("")
                                .frame(false)
                                .min_size(egui::vec2(ui.available_width(), row_h)),
                        );
                        let rect = resp.rect;
                        if selected || resp.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                egui::CornerRadius::same(7),
                                if selected {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    ui.visuals().widgets.hovered.bg_fill
                                },
                            );
                        }
                        // Unread/mentioned rows read louder than idle.
                        let fg = if room.mentioned {
                            accent
                        } else if selected || room.unread > 0 {
                            ui.visuals().text_color()
                        } else {
                            ui.visuals().text_color().gamma_multiply(0.82)
                        };

                        // Queue rects; avatars need `&mut self` the closure lacks.
                        let av = 26.0;
                        let av_rect = egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 8.0 + av / 2.0, rect.center().y),
                            egui::vec2(av, av),
                        );
                        jobs.push((av_rect, room.avatar_mxc.clone(), room.name.clone()));

                        let text_x = av_rect.right() + 9.0;
                        let badge_w = if room.unread > 0 { 30.0 } else { 6.0 };
                        let avail = (rect.right() - text_x - badge_w).max(20.0);
                        let name_y = if previews && room.preview.is_some() {
                            rect.center().y - 9.0
                        } else {
                            rect.center().y
                        };
                        ui.painter().text(
                            egui::pos2(text_x, name_y),
                            egui::Align2::LEFT_CENTER,
                            truncate_name(
                                room.name.trim_start_matches('@'),
                                (avail / 7.2).max(4.0) as usize,
                            ),
                            egui::FontId::proportional(13.5),
                            fg,
                        );
                        if previews {
                            if let Some(preview) = room.preview.clone() {
                                previews_to_paint.push((
                                    egui::pos2(text_x, rect.center().y + 10.0),
                                    truncate_name(&preview, (avail / 6.0).max(4.0) as usize),
                                    avail,
                                ));
                            }
                        }
                        if room.unread > 0 {
                            // Capped, so a busy room cannot widen the row.
                            let text = if room.unread > 99 {
                                "99+".to_owned()
                            } else {
                                room.unread.to_string()
                            };
                            let c = egui::pos2(rect.right() - 18.0, rect.center().y);
                            let w = 9.0 + text.len() as f32 * 3.0;
                            ui.painter().rect_filled(
                                egui::Rect::from_center_size(c, egui::vec2(w * 2.0, 17.0)),
                                egui::CornerRadius::same(9),
                                if room.mentioned {
                                    accent
                                } else {
                                    ui.visuals().widgets.hovered.bg_fill
                                },
                            );
                            ui.painter().text(
                                c,
                                egui::Align2::CENTER_CENTER,
                                text,
                                egui::FontId::proportional(10.5),
                                if room.mentioned {
                                    egui::Color32::BLACK
                                } else {
                                    ui.visuals().text_color()
                                },
                            );
                        }
                        // Test the pointer against the rect: no widget that
                        // could swallow the selecting left-click.
                        let secondary = ui.ctx().input(|inp| {
                            inp.pointer.secondary_clicked()
                                && inp
                                    .pointer
                                    .interact_pos()
                                    .is_some_and(|p| rect.contains(p))
                        });
                        if secondary {
                            menu_for = Some(room.room_id.clone());
                        }
                        if resp
                            .on_hover_text(format!("{}\nright-click for options", room.room_id))
                            .clicked()
                        {
                            clicked = Some(i);
                        }
                    }
                };

                    egui::ScrollArea::vertical().show(ui, |ui| {
                        section(ui, "ROOMS", false, &mut avatar_jobs);
                        section(ui, "DIRECT", true, &mut avatar_jobs);
                    });
                    // `self` is free again; paint the queued avatars.
                    for (rect, mxc, name) in avatar_jobs {
                        self.paint_avatar_at(ui, rect, &mxc, &name);
                    }
                    let muted = ui.visuals().weak_text_color();
                    for (pos, text, width) in previews_to_paint {
                        self.paint_text_with_emoji(ui, pos, &text, 11.0, muted, width);
                    }
                    if let Some(i) = clicked {
                        self.switch_room(i);
                    }
                    if let Some(id) = menu_for {
                        self.room_menu = Some((
                            id,
                            ui.ctx().pointer_latest_pos().unwrap_or_default(),
                            ui.ctx().cumulative_pass_nr(),
                        ));
                    }
                });
        }

        // Members (click → DM). Clamped width + truncated names keep the
        // panel from widening every frame on long display names.
        egui::Panel::right("nicks")
            .default_size(170.0)
            .min_size(120.0)
            .max_size(260.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("MEMBERS — {}", self.members.len()))
                        .small()
                        .weak()
                        .strong(),
                )
                .on_hover_text("click a member for their profile");
                ui.add_space(4.0);
                let mut dm_target: Option<(String, String)> = None;
                let mut profile_open: Option<(String, egui::Pos2)> = None;
                // Cloned once; the loop calls `&mut self` avatar methods.
                let members = std::mem::take(&mut self.members);
                for m in &members {
                    // Allocated rect, not a right-aligned button that overlays long names.
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 30.0),
                        egui::Sense::click(),
                    );
                    let hovered = ui.rect_contains_pointer(rect);
                    if hovered {
                        ui.painter().rect_filled(
                            rect,
                            egui::CornerRadius::same(7),
                            ui.visuals().widgets.hovered.bg_fill,
                        );
                    }
                    let av = 22.0;
                    let av_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 6.0 + av / 2.0, rect.center().y),
                        egui::vec2(av, av),
                    );
                    self.paint_avatar_at(ui, av_rect, &m.avatar_mxc, &m.display);
                    // Presence dot against the avatar.
                    ui.painter().circle_filled(
                        egui::pos2(av_rect.right() - 2.0, av_rect.bottom() - 2.0),
                        3.5,
                        if m.online {
                            self.theme.gold()
                        } else {
                            ui.visuals().weak_text_color()
                        },
                    );
                    // Reserve the DM button width only while hovered.
                    let text_x = av_rect.right() + 9.0;
                    let reserve = if hovered { 32.0 } else { 6.0 };
                    let avail = (rect.right() - text_x - reserve).max(20.0);
                    ui.painter().text(
                        egui::pos2(text_x, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        truncate_name(&m.display, (avail / 7.0).max(3.0) as usize),
                        egui::FontId::proportional(13.0),
                        self.nick_color(&m.display),
                    );
                    if hovered {
                        let btn = egui::Rect::from_center_size(
                            egui::pos2(rect.right() - 17.0, rect.center().y),
                            egui::vec2(26.0, 26.0),
                        );
                        if ui
                            .put(
                                btn,
                                egui::Button::new(
                                    egui::RichText::new(crate::ui::icons::CHAT).size(15.0),
                                ),
                            )
                            .on_hover_text(format!("Message {}", m.display))
                            .clicked()
                        {
                            dm_target = Some((m.mxid.clone(), m.display.clone()));
                        }
                    }
                    if resp
                        .on_hover_text(format!("{} ({}) — view profile", m.display, m.mxid))
                        .clicked()
                    {
                        let anchor = ui
                            .ctx()
                            .pointer_latest_pos()
                            .unwrap_or_else(|| rect.right_top());
                        profile_open = Some((m.mxid.clone(), anchor));
                    }
                }
                // Restore before `open_dm` / `profile_of` can see an empty list.
                self.members = members;
                if let Some((mxid, pos)) = profile_open {
                    self.profile_target = Some((mxid, pos, ui.ctx().cumulative_pass_nr()));
                }
                if let Some((mxid, display)) = dm_target {
                    self.open_dm(&mxid, &display);
                }
            });

        // Status bar.
        egui::Panel::bottom("status").show(ui, |ui| {
            use crate::ui::icons;
            ui.horizontal(|ui| {
                // Lead with a state dot.
                let (dot, tint, label) = if self.client.is_none() {
                    (icons::CIRCLE_SM, ui.visuals().weak_text_color(), "offline")
                } else if self.sync_running {
                    (icons::DOT, self.theme.gold(), "connected")
                } else {
                    (icons::LOADING, ui.visuals().weak_text_color(), "connecting")
                };
                crate::ui::icon_label(ui, dot, tint);
                ui.label(egui::RichText::new(&self.status).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Real numbers only; no decoration that measures nothing.
                    ui.label(
                        egui::RichText::new(format!(
                            "{label} · {} cached images",
                            self.media.resident()
                        ))
                        .small()
                        .weak(),
                    );
                });
            });
        });

        // Input with reply bar + slash palette.
        egui::Panel::bottom("input").show(ui, |ui| {
            if let Some(id) = self.replying_to.clone() {
                // Who and what, not the raw event id. Name and quote stay
                // apart so the quote can carry real emoji.
                let (who, quote) = self
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| (r.display_name.clone(), snippet(&r.body, 48)))
                    .unwrap_or_else(|| ("a message".into(), String::new()));
                let accent = self.theme.gold();
                egui::Frame::new()
                    // Same ground as the timeline; not a raised surface.
                    .fill(ui.visuals().panel_fill)
                    .corner_radius(egui::CornerRadius::same(7))
                    .inner_margin(egui::Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // Coloured bar matches the timeline reply line.
                            let (bar, _) =
                                ui.allocate_exact_size(egui::vec2(2.0, 15.0), egui::Sense::hover());
                            ui.painter()
                                .rect_filled(bar, egui::CornerRadius::same(1), accent);
                            ui.label(egui::RichText::new("Replying to").small().weak());
                            ui.label(egui::RichText::new(&who).small().strong());
                            if !quote.trim().is_empty() {
                                self.emoji_text(ui, &quote, 11.0, true);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if crate::ui::icon_button(
                                        ui,
                                        crate::ui::icons::CLOSE,
                                        "cancel reply",
                                    )
                                    .clicked()
                                    {
                                        self.replying_to = None;
                                    }
                                },
                            );
                        });
                    });
            }
            // Icons with tooltips; hints live in the placeholder and status bar.
            // Staged uploads: name + size + remove, sent with the message.
            if !self.pending_uploads.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    let mut remove: Option<usize> = None;
                    let muted = ui.visuals().weak_text_color();
                    // Borrowed: each staged file owns up to 25 MiB, copied every frame if cloned.
                    let staged = std::mem::take(&mut self.pending_uploads);
                    for (i, up) in staged.iter().enumerate() {
                        egui::Frame::new()
                            .fill(ui.visuals().widgets.hovered.bg_fill)
                            .corner_radius(egui::CornerRadius::same(7))
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    match self.upload_thumb(ui.ctx(), up) {
                                        Some(handle) => {
                                            ui.add(
                                                egui::Image::new(&handle)
                                                    .fit_to_exact_size(egui::vec2(26.0, 26.0))
                                                    .corner_radius(egui::CornerRadius::same(4)),
                                            );
                                        }
                                        None => {
                                            let icon = if up.is_image() {
                                                crate::ui::icons::IMAGE
                                            } else {
                                                crate::ui::icons::FILE
                                            };
                                            crate::ui::icon_label(ui, icon, muted);
                                        }
                                    }
                                    ui.label(
                                        egui::RichText::new(truncate_name(&up.name, 22)).small(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} KB",
                                            up.bytes.len() / 1024
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                    if crate::ui::icon_button(
                                        ui,
                                        crate::ui::icons::CLOSE,
                                        &format!("remove {}", up.name),
                                    )
                                    .clicked()
                                    {
                                        remove = Some(i);
                                    }
                                });
                            });
                    }
                    self.pending_uploads = staged;
                    if let Some(i) = remove {
                        // Drop the thumbnail too, or its texture stays resident.
                        let gone = self.pending_uploads.remove(i);
                        self.upload_thumbs.remove(&gone.name);
                    }
                });
            }
            // Drag-and-drop (egui reports drops per-frame).
            let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
            if !dropped.is_empty() {
                self.ingest_dropped(dropped);
            }
            if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
                ui.horizontal(|ui| {
                    crate::ui::icon_label(
                        ui,
                        crate::ui::icons::ATTACH,
                        ui.visuals().selection.stroke.color,
                    );
                    ui.label(egui::RichText::new("Drop to attach").small());
                });
            }
            ui.horizontal(|ui| {
                use crate::ui::icons;
                // Tooltip carries the caveat: Wayland/winit delivers no file drops.
                let attach_tip = format!("Attach a file\n{}", self.drop_hint);
                if crate::ui::icon_button(ui, icons::ATTACH, &attach_tip).clicked() {
                    self.pick_files();
                }
                if crate::ui::icon_button(ui, icons::PASTE, "Paste image from clipboard (Ctrl+V)")
                    .clicked()
                {
                    self.paste_clipboard(true);
                }

                let can_send = !self.input.trim().is_empty() || !self.pending_uploads.is_empty();
                // Reserve room so the field never squeezes off the trailing controls.
                let trailing = crate::ui::ICON_BUTTON * 3.0 + 40.0;
                // Up arrow on an empty composer edits your last message.
                if self.input.is_empty()
                    && self.editing.is_none()
                    && ui.input(|i| i.key_pressed(egui::Key::ArrowUp))
                {
                    self.start_edit_last();
                }
                if self.editing.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.editing = None;
                    self.input.clear();
                }
                // Name the room; the placeholder says where you type, not a manual.
                let hint = if self.editing.is_some() {
                    "Editing — Enter to save, Esc to cancel".to_owned()
                } else {
                    match self.rooms.get(self.current) {
                        Some(room) if room.is_dm => {
                            format!("Message {}", room.name.trim_start_matches('@'))
                        }
                        Some(room) => format!("Message {}", room.name),
                        None => "Message".to_owned(),
                    }
                };
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.input)
                        .hint_text(hint)
                        .desired_width((ui.available_width() - trailing).max(120.0))
                        .margin(egui::Margin::symmetric(10, 7)),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // A partial command completes; an exact verb runs straight away.
                    match self.slash_matches() {
                        Some((verb, matches)) if !matches.iter().any(|(c, _)| c[1..] == verb) => {
                            let idx = self.slash_selected % matches.len();
                            let cmd = matches[idx].0;
                            self.complete_slash(cmd);
                            resp.request_focus();
                        }
                        _ => self.send_current_input(),
                    }
                }

                let picker_open = self.show_picker;
                if crate::ui::icon_toggle(
                    ui,
                    icons::EMOJI,
                    "Emoji, custom emoji and stickers",
                    picker_open,
                )
                .clicked()
                {
                    self.show_picker = !picker_open;
                    self.picker_opened_frame = ui.ctx().cumulative_pass_nr();
                    self.picker_tab = PickerTab::Emoji;
                }
                if crate::ui::icon_button(ui, icons::STICKER, "Stickers").clicked() {
                    self.show_picker = true;
                    self.picker_opened_frame = ui.ctx().cumulative_pass_nr();
                    self.picker_tab = PickerTab::Stickers;
                }
                // Send is the accent action; greys out when there is nothing to send.
                let tint = if can_send {
                    self.theme.gold()
                } else {
                    ui.visuals().weak_text_color()
                };
                if crate::ui::icon_button_tinted(ui, icons::SEND, "Send (Enter)", tint).clicked()
                    && can_send
                {
                    self.send_current_input();
                }
            });
            // @-mention autocomplete: word under the cursor starting with '@'.
            if let Some(prefix) = mention_prefix(&self.input).map(str::to_owned) {
                // Owned: the `self.input` borrow would span the mutable uses below.
                let q = prefix.trim_start_matches('@').to_lowercase();
                let hits: Vec<Member> = self
                    .members
                    .iter()
                    .filter(|m| {
                        q.is_empty()
                            || m.display.to_lowercase().contains(&q)
                            || m.mxid.to_lowercase().contains(&q)
                    })
                    .take(8)
                    .cloned()
                    .collect();
                if !hits.is_empty() {
                    self.mention_selected %= hits.len();
                    let step = ui.input(|i| {
                        i.key_pressed(egui::Key::ArrowDown) as usize
                            + i.key_pressed(egui::Key::Tab) as usize
                    });
                    if step > 0 {
                        self.mention_selected = (self.mention_selected + 1) % hits.len();
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                        self.mention_selected =
                            (self.mention_selected + hits.len() - 1) % hits.len();
                    }
                    let accept = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let mut chosen: Option<Member> = None;
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        for (i, m) in hits.iter().enumerate() {
                            let sel = i == self.mention_selected;
                            let row = ui.horizontal(|ui| {
                                let avatar = m.avatar_mxc.clone();
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(18.0, 18.0),
                                    egui::Sense::hover(),
                                );
                                self.paint_avatar_at(ui, rect, &avatar, &m.display);
                                let _ = ui.selectable_label(
                                    sel,
                                    egui::RichText::new(&m.display)
                                        .color(self.nick_color(&m.display)),
                                );
                                ui.label(egui::RichText::new(&m.mxid).small().weak());
                            });
                            let whole_row = crate::ui::clickable(ui.interact(
                                row.response.rect,
                                ui.id().with(("mention", m.mxid.as_str())),
                                egui::Sense::click(),
                            ));
                            if whole_row.clicked() {
                                chosen = Some(m.clone());
                            }
                            if sel && accept {
                                chosen = Some(m.clone());
                            }
                        }
                    });
                    if let Some(m) = chosen {
                        // Replace the partial @word with the display name.
                        let cut = self.input.len() - prefix.len();
                        self.input.truncate(cut);
                        self.input.push_str(&m.display);
                        self.input.push(' ');
                        self.mention_selected = 0;
                    }
                }
            }
            if let Some((_, matches)) = self.slash_matches() {
                let selected = self.slash_selected % matches.len();
                // Tab completes the highlight; arrows move it.
                if ui.input(|i| i.key_pressed(egui::Key::Tab)) {
                    let cmd = matches[selected].0;
                    self.complete_slash(cmd);
                } else {
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                        self.slash_selected = (selected + 1) % matches.len();
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                        self.slash_selected = (selected + matches.len() - 1) % matches.len();
                    }
                }
                let mut chosen: Option<&str> = None;
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Tab to complete · Enter to run")
                            .small()
                            .weak(),
                    );
                    for (i, (cmd, hint)) in matches.iter().enumerate() {
                        let sel = i == selected;
                        if ui
                            .selectable_label(sel, egui::RichText::new(format!("{cmd}  {hint}")))
                            .clicked()
                        {
                            chosen = Some(cmd);
                        }
                    }
                });
                if let Some(cmd) = chosen {
                    self.complete_slash(cmd);
                }
            }
        });

        // Compact timeline. Logged out / syncing: spinner, not fake demo rooms.
        if self.client.is_none() || (self.rows.is_empty() && self.login.busy) {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.3);
                    let frame = (ui.ctx().cumulative_pass_nr() / 8) % 4;
                    let spinner = ["◐", "◓", "◑", "◒"][frame as usize];
                    ui.monospace(egui::RichText::new(spinner).size(48.0));
                    ui.monospace(if self.login.busy {
                        "syncing rooms…"
                    } else {
                        "log in to load your rooms"
                    });
                    if !self.login.busy {
                        ui.monospace("password or SSO above");
                    }
                });
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                if let Some(room_id) = self.current_room_id() {
                    if self.history_queue.has_failed(&room_id) {
                        ui.horizontal(|ui| {
                            ui.label("Could not load messages.");
                            if ui.button("Retry").clicked() {
                                self.history_queue.select(&room_id);
                                self.pump_history();
                            }
                        });
                    } else if !self.history_queue.is_loaded(&room_id) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Loading messages...");
                        });
                    }
                }
                let scroll = egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    // Fill the panel; a shrunk scroll area clips rows and strands the scrollbar.
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Scrollback indicator at the very top.
                        if self.paginating.is_some() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    egui::RichText::new("Loading older messages…")
                                        .small()
                                        .weak(),
                                );
                            });
                        } else if self.current_room_id().is_some_and(|id| {
                            self.history_queue.is_loaded(&id) && !self.back_tokens.contains_key(&id)
                        }) {
                            ui.label(egui::RichText::new("Beginning of the room").small().weak());
                        }
                        let mut toggle: Option<(usize, String)> = None;
                        let mut reply_to: Option<String> = None;
                        let mut react_open: Option<(String, egui::Pos2)> = None;
                        let mut profile_open: Option<(String, egui::Pos2)> = None;
                        let mut jump_to: Option<String> = None;
                        // Consumed by whichever row matches it this frame.
                        let scroll_target = self.scroll_to_event.take();
                        let now = ui.ctx().input(|i| i.time);
                        let highlight = self
                            .highlight_event
                            .as_ref()
                            .filter(|(_, until)| *until > now)
                            .map(|(id, _)| id.clone());
                        let frame_nr = ui.ctx().cumulative_pass_nr();
                        // Spelled out so a method-resolution change cannot silently deep-copy again.
                        let rows = std::rc::Rc::clone(&self.rows);
                        for (ri, row) in rows.iter().enumerate() {
                            let row_id = row.id.clone();

                            // Live target row first; plain-text quote fallback when not loaded.
                            let reply_view: Option<(String, String, Option<String>)> = row
                                .reply_to_id
                                .as_ref()
                                .and_then(|id| self.rows.iter().find(|r| &r.id == id))
                                .map(|t| {
                                    (
                                        t.display_name.clone(),
                                        snippet(&t.body, 80),
                                        t.avatar_mxc.clone(),
                                    )
                                })
                                .or_else(|| {
                                    row.reply_to.clone().map(|(who, snip)| (who, snip, None))
                                });
                            // Filter our own receipt; it is not news.
                            let me = self.own_user.clone();
                            let seen: Vec<String> =
                                row.seen_by.iter().filter(|u| **u != me).cloned().collect();
                            // Gutter only with receipts, so ordinary messages keep full width.
                            let gutter = if seen.is_empty() { 0.0 } else { 86.0 };
                            let row_resp = ui
                                .horizontal_top(|ui| {
                                    // Avatar: cached mxc texture, else colored initial.
                                    let amxc = row.avatar_mxc.clone();
                                    let dname = row.display_name.clone();
                                    let sender = row.sender.clone();
                                    let avatar_resp = self
                                        .render_avatar(ui, &amxc, &dname)
                                        .on_hover_text(format!("{} — view profile", row.sender));
                                    if avatar_resp.clicked() {
                                        profile_open = Some((
                                            sender.clone(),
                                            ui.ctx()
                                                .pointer_latest_pos()
                                                .unwrap_or_else(|| avatar_resp.rect.right_top()),
                                        ));
                                    }
                                    let content_w = (ui.available_width() - gutter).max(140.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(content_w, 0.0),
                                        egui::Layout::top_down(egui::Align::LEFT),
                                        |ui| {
                                            // Tight gap; the default left messages double-spaced.
                                            ui.spacing_mut().item_spacing.y = 1.0;
                                            // Header: name + time + actions.
                                            ui.horizontal_wrapped(|ui| {
                                                let name_resp = ui
                                                    .add(
                                                        egui::Label::new(
                                                            egui::RichText::new(&row.display_name)
                                                                .strong()
                                                                .color(
                                                                    self.nick_color(
                                                                        &row.display_name,
                                                                    ),
                                                                ),
                                                        )
                                                        .selectable(false)
                                                        .sense(egui::Sense::click()),
                                                    )
                                                    .on_hover_text(format!(
                                                        "{sender} — view profile"
                                                    ));
                                                if name_resp.clicked() {
                                                    profile_open = Some((
                                                        sender.clone(),
                                                        ui.ctx()
                                                            .pointer_latest_pos()
                                                            .unwrap_or_else(|| {
                                                                name_resp.rect.right_top()
                                                            }),
                                                    ));
                                                }
                                                if !row.ts.is_empty() {
                                                    ui.label(
                                                        egui::RichText::new(&row.ts).weak().small(),
                                                    );
                                                }
                                                if row.edited {
                                                    ui.label(
                                                        egui::RichText::new("(edited)")
                                                            .weak()
                                                            .small(),
                                                    )
                                                    .on_hover_text("this message was edited");
                                                }
                                                if row.is_sticker {
                                                    crate::ui::icon_label(
                                                        ui,
                                                        crate::ui::icons::STICKER,
                                                        ui.visuals().weak_text_color(),
                                                    )
                                                    .on_hover_text("sticker");
                                                }
                                                // No header buttons; actions live in right-click, reply on double-click.
                                            });
                                            // Reply chain.
                                            if let Some((who, snip, avatar)) = reply_view.clone() {
                                                // Accent bar + "In reply to" + avatar + name.
                                                let colour = self.nick_color(&who);
                                                let line = ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 5.0;
                                                    let (bar, _) = ui.allocate_exact_size(
                                                        egui::vec2(2.0, 15.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    ui.painter().rect_filled(
                                                        bar,
                                                        egui::CornerRadius::same(1),
                                                        colour,
                                                    );
                                                    ui.label(
                                                        egui::RichText::new("In reply to")
                                                            .small()
                                                            .weak(),
                                                    );
                                                    let (av, _) = ui.allocate_exact_size(
                                                        egui::vec2(14.0, 14.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    let ring = ui.visuals().panel_fill;
                                                    self.paint_avatar_circle(
                                                        ui, av, &avatar, &who, ring,
                                                    );
                                                    ui.label(
                                                        egui::RichText::new(&who)
                                                            .small()
                                                            .color(colour)
                                                            .strong(),
                                                    );
                                                    if !snip.trim().is_empty() {
                                                        let text = snippet(&snip, 70);
                                                        self.emoji_text(ui, &text, 11.0, true);
                                                    }
                                                });
                                                // Hover-only labels inside, so this steals no clicks.
                                                // Explicit per-event id: auto ids from layout position collide.
                                                let hit = crate::ui::clickable(ui.interact(
                                                    line.response.rect,
                                                    ui.id().with(("reply-jump", row_id.as_str())),
                                                    egui::Sense::click(),
                                                ))
                                                .on_hover_text("Go to the original message");
                                                if hit.clicked() {
                                                    jump_to = row.reply_to_id.clone();
                                                }
                                            }
                                            // Body / image (cached textures; rich markdown).
                                            if let Some(img) = row.image.clone() {
                                                self.render_image(ui, &img);
                                                if !row.body.is_empty() {
                                                    let b = row.body.clone();
                                                    let f = row.formatted.clone();
                                                    self.render_body(ui, &b, f.as_deref());
                                                }
                                            } else if row.is_sticker {
                                                match row.image.clone() {
                                                    Some(img) => {
                                                        self.render_image(ui, &img);
                                                        if !row.body.is_empty() {
                                                            let b = row.body.clone();
                                                            let f = row.formatted.clone();
                                                            self.render_body(ui, &b, f.as_deref());
                                                        }
                                                    }
                                                    None => {
                                                        let b = row.body.clone();
                                                        ui.colored_label(
                                                            self.theme.gold(),
                                                            egui::RichText::new(&b),
                                                        );
                                                    }
                                                }
                                            } else {
                                                let b = row.body.clone();
                                                let f = row.formatted.clone();
                                                self.render_body(ui, &b, f.as_deref());
                                                self.render_embeds(ui, &b);
                                            }
                                            // Reactions grouped by key; click toggles ours.
                                            if !row.reactions.is_empty() {
                                                let own_user = self
                                                    .client
                                                    .as_ref()
                                                    .and_then(|c| c.user_id())
                                                    .map(|u| u.to_string())
                                                    .unwrap_or_default();
                                                ui.horizontal_wrapped(|ui| {
                                                    for r in &row.reactions {
                                                        let owned = r.owns(&own_user);
                                                        // Resolved here so the hover closure avoids a second `self` borrow.
                                                        let reactors: Vec<(
                                                            String,
                                                            Option<String>,
                                                        )> = r
                                                            .senders
                                                            .iter()
                                                            .map(|s| {
                                                                let (name, avatar, _) =
                                                                    self.profile_of(&s.user);
                                                                (name, avatar)
                                                            })
                                                            .collect();
                                                        // mxc key: MSC4027 custom emoji as image;
                                                        // `:shortcode:` is the legacy bridge form.
                                                        let custom = if r.key.starts_with("mxc://")
                                                        {
                                                            Some(r.key.clone())
                                                        } else if r.key.starts_with(':') {
                                                            self.packs
                                                                .resolve(&r.key)
                                                                .map(|i| i.mxc_url.clone())
                                                        } else {
                                                            None
                                                        };
                                                        let texture = custom
                                                            .and_then(|mxc| {
                                                                self.media.texture_for(
                                                                    "emoji",
                                                                    &mxc,
                                                                    Some((24, 24)),
                                                                )
                                                            })
                                                            .or_else(|| {
                                                                self.emoji_font.texture_at_size(
                                                                    ui.ctx(),
                                                                    &r.key,
                                                                    18.0,
                                                                )
                                                            });
                                                        // Owned state rides on the chip fill/outline, not extra glyphs.
                                                        let accent = self.theme.gold();
                                                        let (fill, stroke) = if owned {
                                                            (
                                                                accent.gamma_multiply(0.22),
                                                                egui::Stroke::new(1.0, accent),
                                                            )
                                                        } else {
                                                            (
                                                                ui.visuals()
                                                                    .widgets
                                                                    .hovered
                                                                    .bg_fill,
                                                                egui::Stroke::NONE,
                                                            )
                                                        };
                                                        // Bare count; "x5" wastes characters.
                                                        let count = r.count().to_string();
                                                        let button = match texture {
                                                            Some(h) => {
                                                                egui::Button::image_and_text(
                                                                    egui::Image::new(&h)
                                                                        .fit_to_exact_size(
                                                                            egui::vec2(18.0, 18.0),
                                                                        ),
                                                                    count,
                                                                )
                                                            }
                                                            // No bitmap: raw key (unicode or shortcode).
                                                            None => egui::Button::new(format!(
                                                                "{} {count}",
                                                                r.key
                                                            )),
                                                        };
                                                        let label =
                                                            reaction_label(&r.key, &self.packs);
                                                        let key = r.key.clone();
                                                        let resp = ui
                                                            .add(
                                                                button
                                                                    .min_size(egui::vec2(0.0, 26.0))
                                                                    .fill(fill)
                                                                    .stroke(stroke),
                                                            )
                                                            // Rich tooltip: plain text renders emoji via
                                                            // font fallback, mismatching the chip.
                                                            .on_hover_ui(|ui| {
                                                                ui.horizontal(|ui| {
                                                                    self.emoji_widget(
                                                                        ui, &key, 20.0,
                                                                    );
                                                                    ui.label(
                                                                        egui::RichText::new(&label)
                                                                            .strong(),
                                                                    );
                                                                });
                                                                ui.add_space(2.0);
                                                                if reactors.is_empty() {
                                                                    ui.label(
                                                                        egui::RichText::new(
                                                                            "nobody yet",
                                                                        )
                                                                        .small()
                                                                        .weak(),
                                                                    );
                                                                }
                                                                let ring = ui.visuals().panel_fill;
                                                                for (name, avatar) in &reactors {
                                                                    ui.horizontal(|ui| {
                                                                        let (rect, _) = ui
                                                                            .allocate_exact_size(
                                                                                egui::vec2(
                                                                                    16.0, 16.0,
                                                                                ),
                                                                                egui::Sense::hover(
                                                                                ),
                                                                            );
                                                                        self.paint_avatar_circle(
                                                                            ui, rect, avatar, name,
                                                                            ring,
                                                                        );
                                                                        ui.label(
                                                                            egui::RichText::new(
                                                                                name,
                                                                            )
                                                                            .small(),
                                                                        );
                                                                    });
                                                                }
                                                            });
                                                        if resp.clicked() {
                                                            toggle = Some((ri, r.key.clone()));
                                                        }
                                                    }
                                                });
                                            }
                                            // Threads unimplemented (nothing sets `thread_count`); seen-by dots below.
                                        },
                                    );
                                    // Receipts share a right gutter; their own row stretched every message.
                                    if !seen.is_empty() {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::TOP),
                                            |ui| self.render_seen_by(ui, &seen),
                                        );
                                    }
                                })
                                .response;

                            // Right-click should also work in the empty space beside narrow content.
                            // Only the hit-test extends; child widgets keep their left-clicks.
                            let row_hit_rect = egui::Rect::from_min_max(
                                row_resp.rect.min,
                                egui::pos2(ui.max_rect().right(), row_resp.rect.max.y),
                            );

                            // Right-click opens the react picker at the pointer. Never a
                            // click-sensing row rect: last-registered-wins would swallow
                            // child link/reaction clicks. Same for double-click-to-reply.
                            let double = ui.ctx().input(|i| {
                                i.pointer
                                    .button_double_clicked(egui::PointerButton::Primary)
                                    && i.pointer
                                        .interact_pos()
                                        .is_some_and(|p| row_hit_rect.contains(p))
                            });
                            if double {
                                reply_to = Some(row_id.clone());
                            }
                            let hit_row = ui.ctx().input(|i| {
                                i.pointer.secondary_clicked()
                                    && i.pointer
                                        .interact_pos()
                                        .is_some_and(|p| row_hit_rect.contains(p))
                            });
                            if hit_row {
                                react_open = Some((
                                    row_id.clone(),
                                    ui.ctx()
                                        .pointer_latest_pos()
                                        .unwrap_or_else(|| row_resp.rect.left_top()),
                                ));
                            }
                            // Landed from a reply jump: scroll into view and flash.
                            if scroll_target.as_deref() == Some(row_id.as_str()) {
                                row_resp.scroll_to_me(Some(egui::Align::Center));
                            }
                            if highlight.as_deref() == Some(row_id.as_str()) {
                                ui.painter().rect_filled(
                                    row_resp.rect.expand2(egui::vec2(4.0, 2.0)),
                                    egui::CornerRadius::same(6),
                                    self.theme.gold().gamma_multiply(0.16),
                                );
                            }
                            ui.add_space(6.0);
                        }
                        if let Some(target) = jump_to {
                            // Only when the target is loaded.
                            if self.rows.iter().any(|r| r.id == target) {
                                self.highlight_event = Some((target.clone(), now + 1.6));
                                self.scroll_to_event = Some(target);
                            } else if self
                                .current_room_id()
                                .is_some_and(|id| self.back_tokens.contains_key(&id))
                            {
                                // Not loaded yet: fetch the next page and say so.
                                self.paginate_current();
                                self.status = "loading older messages to find it…".into();
                            } else {
                                self.status = "that message is no longer available".into();
                            }
                        }
                        // `scroll_to_me` animates over frames; keep repainting or the scroll stalls mid-way.
                        if self.scroll_to_event.is_some()
                            || self
                                .highlight_event
                                .as_ref()
                                .is_some_and(|(_, until)| *until > now)
                        {
                            ui.ctx().request_repaint();
                        }
                        if let Some((ri, key)) = toggle {
                            self.send_reaction(ri, key);
                        }
                        if let Some(id) = reply_to {
                            self.replying_to = Some(id);
                        }
                        if let Some((event_id, pos)) = react_open {
                            self.react_target = Some((event_id, pos, frame_nr));
                        }
                        if let Some((mxid, pos)) = profile_open {
                            self.profile_target = Some((mxid, pos, frame_nr));
                        }
                        // Scroll to content end; egui animates rather than jumping.
                        if std::mem::take(&mut self.jump_to_latest) {
                            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                        }
                    });

                let content_h = scroll.content_size.y;
                // Prepended pages shift the offset by the added height, or the reader jumps.
                if let Some(before) = self.pending_scroll_fix.take() {
                    let delta = content_h - before;
                    if delta > 0.0 {
                        let mut state = scroll.state;
                        state.offset.y += delta;
                        state.store(ui.ctx(), scroll.id);
                    }
                }
                self.last_content_height = content_h;
                // Near the top with more to fetch: one request via the paginate guard.
                if scroll.state.offset.y < 120.0 && content_h > 0.0 {
                    self.paginate_current();
                }

                // Distance from newest, in rows: images and one-liners differ in pixels.
                let view_h = scroll.inner_rect.height();
                let below = (content_h - scroll.state.offset.y - view_h).max(0.0);
                let row_h = if self.rows.is_empty() {
                    0.0
                } else {
                    content_h / self.rows.len() as f32
                };
                const ROWS_BEFORE_JUMP: f32 = 30.0;
                let hidden_rows = if row_h > 0.0 { below / row_h } else { 0.0 };
                if hidden_rows >= ROWS_BEFORE_JUMP {
                    let area = scroll.inner_rect;
                    let size = 38.0;
                    let btn = egui::Rect::from_min_size(
                        egui::pos2(area.right() - size - 18.0, area.bottom() - size - 14.0),
                        egui::vec2(size, size),
                    );
                    // Round, so it reads as floating action, not timeline.
                    ui.painter().circle_filled(
                        btn.center(),
                        size * 0.5,
                        ui.visuals().widgets.hovered.bg_fill,
                    );
                    ui.painter().circle_stroke(
                        btn.center(),
                        size * 0.5,
                        egui::Stroke::new(1.0, self.theme.gold().gamma_multiply(0.5)),
                    );
                    let resp = ui.put(
                        btn,
                        egui::Button::new(
                            egui::RichText::new(crate::ui::icons::CHEVRON_DOWN)
                                .size(20.0)
                                .color(self.theme.gold()),
                        )
                        .frame(false),
                    );
                    if resp
                        .on_hover_text(format!(
                            "Jump to latest — {} messages below",
                            hidden_rows as usize
                        ))
                        .clicked()
                    {
                        self.jump_to_latest = true;
                        // Repaint only covers the animation, not the whole scrolled-up stay.
                        self.jump_until = ui.ctx().input(|i| i.time) + 0.8;
                    }
                    if ui.ctx().input(|i| i.time) < self.jump_until {
                        ui.ctx().request_repaint();
                    }
                }
            });
        }
        // Full react picker. Anchored by `fixed_pos` near the click, clamped on-screen.
        if let Some((target_id, anchor, opened)) = self.react_target.clone() {
            let ctx = ui.ctx().clone();
            let mut close = false;
            let mut pick: Option<String> = None;
            let mut reply = false;
            let mut delete = false;
            let mut copy = false;
            // Re-resolve every frame; a stored index drifts under sync batches.
            let ri = self.rows.iter().position(|r| r.id == target_id);
            let preview = ri
                .and_then(|i| self.rows.get(i))
                .map(|r| format!("{} — {}", r.display_name, snippet(&r.body, 40)))
                .unwrap_or_default();
            // Escape closes.
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let viewport = ctx.viewport_rect();
            let pos = egui::pos2(
                (anchor.x + 16.0).clamp(
                    viewport.min.x + 8.0,
                    (viewport.max.x - 320.0).max(viewport.min.x + 8.0),
                ),
                (anchor.y - 40.0).clamp(
                    viewport.min.y + 8.0,
                    (viewport.max.y - 300.0).max(viewport.min.y + 8.0),
                ),
            );
            let mut open = true;
            let window = crate::ui::popup(&ctx, "React")
                .open(&mut open)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    if !preview.is_empty() {
                        ui.monospace(format!("reacting to {preview}"));
                    }
                    ui.horizontal(|ui| {
                        use crate::ui::icons;
                        if crate::ui::icon_button(ui, icons::REPLY, "Reply").clicked() {
                            reply = true;
                            close = true;
                        }
                        if crate::ui::icon_button(ui, icons::COPY, "Copy text").clicked() {
                            copy = true;
                            close = true;
                        }
                        if ri.is_some() {
                            // Tinted and apart: destructive.
                            let danger = ui.visuals().error_fg_color;
                            if crate::ui::icon_button_tinted(
                                ui,
                                icons::DELETE,
                                "Delete this message",
                                danger,
                            )
                            .clicked()
                            {
                                delete = true;
                                close = true;
                            }
                        }
                    });
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for e in [
                            "\u{1F44D}",
                            "\u{2764}\u{FE0F}",
                            "\u{1F602}",
                            "\u{1F389}",
                            "\u{1F440}",
                            "\u{1F622}",
                        ] {
                            if self.emoji_widget(ui, e, 22.0).clicked() {
                                self.note_recent_emoji(e);
                                pick = Some(e.into());
                                close = true;
                            }
                        }
                    });
                    ui.separator();
                    // Same order as the composer picker: emoji, custom, stickers.
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        for (tab, name) in [
                            (PickerTab::Emoji, "Emoji"),
                            (PickerTab::Custom, "Custom"),
                            (PickerTab::Stickers, "Stickers"),
                        ] {
                            if ui.selectable_label(self.picker_tab == tab, name).clicked() {
                                self.picker_tab = tab;
                            }
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.picker_query)
                                .hint_text("search")
                                .desired_width(110.0),
                        );
                    });
                    let query = self.picker_query.trim().to_lowercase();
                    match self.picker_tab {
                        // The grid scrolls itself; an outer scroll area would nest two of them.
                        PickerTab::Emoji => {
                            if let Some(e) = self.emoji_grid(ui, 240.0) {
                                pick = Some(e);
                                close = true;
                            }
                        }
                        PickerTab::Stickers => {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    let stickers: Vec<(String, String)> = self
                                        .packs
                                        .stickers()
                                        .map(|(_, i)| (i.shortcode.clone(), i.mxc_url.clone()))
                                        .collect();
                                    ui.horizontal_wrapped(|ui| {
                                        for (sc, mxc) in stickers {
                                            if let Some(h) = self.media.texture_for(
                                                "sticker",
                                                &mxc,
                                                Some((64, 64)),
                                            ) {
                                                if ui
                                                    .add(
                                                        egui::Image::new(&h)
                                                            .max_height(44.0)
                                                            .sense(egui::Sense::click()),
                                                    )
                                                    .on_hover_text(&sc)
                                                    .clicked()
                                                {
                                                    pick = Some(format!(":{sc}:"));
                                                    close = true;
                                                }
                                            }
                                        }
                                    });
                                });
                        }
                        PickerTab::Custom => {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    for pack in self.packs.packs().to_vec() {
                                        let hits: Vec<_> = pack
                                            .images
                                            .iter()
                                            .filter(|i| {
                                                query.is_empty()
                                                    || i.shortcode.to_lowercase().contains(&query)
                                            })
                                            .cloned()
                                            .collect();
                                        if hits.is_empty() {
                                            continue;
                                        }
                                        ui.label(
                                            egui::RichText::new(&pack.display_name)
                                                .small()
                                                .weak()
                                                .strong(),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            for img in hits {
                                                let hit = match self.media.texture_for(
                                                    "emoji",
                                                    &img.mxc_url,
                                                    Some((24, 24)),
                                                ) {
                                                    Some(h) => ui
                                                        .add(
                                                            egui::Image::new(&h)
                                                                .max_height(22.0)
                                                                .sense(egui::Sense::click()),
                                                        )
                                                        .on_hover_text(format!(
                                                            ":{}:",
                                                            img.shortcode
                                                        ))
                                                        .clicked(),
                                                    None => ui
                                                        .button(format!(":{}:", img.shortcode))
                                                        .clicked(),
                                                };
                                                if hit {
                                                    pick = Some(format!(":{}:", img.shortcode));
                                                    close = true;
                                                }
                                            }
                                        });
                                    }
                                });
                        }
                    }
                })
                .map(|w| w.response.rect);
            if !open {
                close = true;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    close = true;
                }
            }
            if let (Some(key), Some(i)) = (pick, ri) {
                self.send_reaction(i, key);
                close = true;
            }
            if reply {
                self.replying_to = Some(target_id.clone());
            }
            if copy {
                let text = ri
                    .and_then(|i| self.rows.get(i))
                    .map(|r| r.body.clone())
                    .unwrap_or_default();
                self.status = match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                    Ok(()) => "copied".into(),
                    Err(e) => format!("copy failed: {e}"),
                };
            }
            if delete {
                self.delete_message(target_id.clone());
            }
            // Target scrolled out: nothing to react to.
            if close || ri.is_none() {
                self.react_target = None;
            }
        }

        // Room menu (sidebar right-click).
        if let Some((room_id, anchor, opened)) = self.room_menu.clone() {
            let ctx = ui.ctx().clone();
            let mut close = false;
            let mut chosen: Option<RoomMenuAction> = None;
            let room = self.rooms.iter().find(|r| r.room_id == room_id).cloned();
            let name = room
                .as_ref()
                .map(|r| r.name.clone())
                .unwrap_or_else(|| short_room(&room_id));
            let unread = room.as_ref().is_some_and(|r| r.unread > 0);
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let viewport = ctx.viewport_rect();
            let pos = egui::pos2(
                (anchor.x + 8.0).clamp(
                    viewport.min.x + 8.0,
                    (viewport.max.x - 220.0).max(viewport.min.x + 8.0),
                ),
                (anchor.y).clamp(
                    viewport.min.y + 8.0,
                    (viewport.max.y - 300.0).max(viewport.min.y + 8.0),
                ),
            );
            let mut open = true;
            let window = crate::ui::popup(&ctx, "Room")
                .open(&mut open)
                .resizable(false)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    use crate::ui::icons;
                    ui.set_min_width(190.0);
                    ui.label(egui::RichText::new(truncate_name(&name, 26)).strong());
                    ui.separator();

                    let item = |ui: &mut egui::Ui, icon: &str, text: &str| {
                        ui.horizontal(|ui| {
                            crate::ui::icon_label(ui, icon, ui.visuals().weak_text_color());
                            ui.add(
                                egui::Label::new(text)
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .clicked()
                        })
                        .inner
                    };

                    if unread {
                        if item(ui, icons::CHECK, "Mark as read") {
                            chosen = Some(RoomMenuAction::MarkRead);
                        }
                    } else if item(ui, icons::DOT, "Mark as unread") {
                        chosen = Some(RoomMenuAction::MarkUnread);
                    }
                    if item(ui, icons::CHECK, "Toggle favourite") {
                        chosen = Some(RoomMenuAction::Favourite(true));
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Notifications").small().weak());
                    for mode in [RoomNotify::All, RoomNotify::MentionsOnly, RoomNotify::Mute] {
                        let icon = match mode {
                            RoomNotify::Mute => icons::CIRCLE_SM,
                            _ => icons::DOT,
                        };
                        if item(ui, icon, mode.label()) {
                            chosen = Some(RoomMenuAction::Notify(mode));
                        }
                    }
                    ui.separator();
                    if item(ui, icons::AT, "Invite someone") {
                        chosen = Some(RoomMenuAction::Invite);
                    }
                    if item(ui, icons::COPY, "Copy room link") {
                        chosen = Some(RoomMenuAction::CopyLink);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        let danger = ui.visuals().error_fg_color;
                        crate::ui::icon_label(ui, icons::LOGOUT, danger);
                        if ui
                            .add(
                                egui::Label::new(egui::RichText::new("Leave room").color(danger))
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .clicked()
                        {
                            chosen = Some(RoomMenuAction::Leave);
                        }
                    });
                })
                .map(|w| w.response.rect);
            if !open {
                close = true;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    close = true;
                }
            }
            if let Some(action) = chosen {
                // Favourite toggles; resolve current state here, not in the menu.
                let action = match action {
                    RoomMenuAction::Favourite(_) => {
                        let on = !self.favourites.contains(&room_id);
                        if on {
                            self.favourites.insert(room_id.clone());
                        } else {
                            self.favourites.remove(&room_id);
                        }
                        RoomMenuAction::Favourite(on)
                    }
                    other => other,
                };
                self.room_menu_action(room_id.clone(), action);
                close = true;
            }
            if close {
                self.room_menu = None;
            }
        }

        // Profile card. Anchored at the click like the picker, clamped on-screen.
        if let Some((mxid, anchor, opened)) = self.profile_target.clone() {
            let ctx = ui.ctx().clone();
            let (display, avatar, online) = self.profile_of(&mxid);
            let shared = self.shared_rooms(&mxid);
            let is_self = mxid == self.own_user;
            let mut close = false;
            let mut action: Option<ProfileAction> = None;
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let viewport = ctx.viewport_rect();
            let pos = egui::pos2(
                (anchor.x + 16.0).clamp(
                    viewport.min.x + 8.0,
                    (viewport.max.x - 300.0).max(viewport.min.x + 8.0),
                ),
                (anchor.y - 20.0).clamp(
                    viewport.min.y + 8.0,
                    (viewport.max.y - 260.0).max(viewport.min.y + 8.0),
                ),
            );
            let mut open = true;
            let window = crate::ui::popup(&ctx, "Profile")
                .open(&mut open)
                .resizable(false)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        self.render_avatar_sized(ui, &avatar, &display, 72.0);
                        ui.vertical(|ui| {
                            ui.colored_label(
                                self.nick_color(&display),
                                egui::RichText::new(&display).heading().strong(),
                            );
                            ui.monospace(egui::RichText::new(&mxid).small().weak());
                            ui.monospace(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    if online { "●" } else { "○" },
                                    if online { "online" } else { "offline" }
                                ))
                                .small()
                                .weak(),
                            );
                            if shared > 0 {
                                ui.monospace(
                                    egui::RichText::new(format!(
                                        "{shared} shared room{}",
                                        if shared == 1 { "" } else { "s" }
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                        });
                    });
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        // No DM/verify actions for yourself.
                        use crate::ui::icons;
                        if !is_self {
                            if crate::ui::icon_button(ui, icons::CHAT, "Message").clicked() {
                                action = Some(ProfileAction::Message);
                            }
                            if crate::ui::icon_button(ui, icons::VERIFIED, "Verify devices")
                                .clicked()
                            {
                                action = Some(ProfileAction::Verify);
                            }
                        }
                        if crate::ui::icon_button(ui, icons::AT, "Mention in the composer")
                            .clicked()
                        {
                            action = Some(ProfileAction::Mention);
                        }
                        if crate::ui::icon_button(ui, icons::COPY, "Copy the full mxid").clicked() {
                            action = Some(ProfileAction::CopyId);
                        }
                    });
                })
                .map(|w| w.response.rect);
            if !open {
                close = true;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    close = true;
                }
            }
            match action {
                Some(ProfileAction::Message) => {
                    self.open_dm(&mxid, &display);
                    close = true;
                }
                Some(ProfileAction::Mention) => {
                    if !self.input.is_empty() && !self.input.ends_with(' ') {
                        self.input.push(' ');
                    }
                    self.input.push_str(&format!("{display}: "));
                    close = true;
                }
                Some(ProfileAction::Verify) => {
                    self.verify_user_input = mxid.clone();
                    self.show_security = true;
                    self.refresh_devices();
                    close = true;
                }
                Some(ProfileAction::CopyId) => {
                    self.status = match arboard::Clipboard::new()
                        .and_then(|mut c| c.set_text(mxid.clone()))
                    {
                        Ok(()) => format!("copied {mxid}"),
                        Err(e) => format!("copy failed: {e}"),
                    };
                    close = true;
                }
                None => {}
            }
            if close {
                self.profile_target = None;
            }
        }

        // Emoji picker.
        if self.show_picker {
            let ctx = ui.ctx().clone();
            let opened = self.picker_opened_frame;
            let mut picker_open = true;
            let window = crate::ui::popup(&ctx, "Emoji")
                .open(&mut picker_open)
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        // Tabs sit apart so the three destinations read as separate targets.
                        ui.spacing_mut().item_spacing.x = 12.0;
                        for (tab, name) in [
                            (PickerTab::Emoji, "Emoji"),
                            (PickerTab::Custom, "Custom"),
                            (PickerTab::Stickers, "Stickers"),
                        ] {
                            if ui.selectable_label(self.picker_tab == tab, name).clicked() {
                                self.picker_tab = tab;
                            }
                        }
                    });
                    // The window owns the only search field; the emoji grid adds none of its own.
                    let hint = match self.picker_tab {
                        PickerTab::Emoji => "Search emoji",
                        PickerTab::Custom => "Search custom emoji",
                        PickerTab::Stickers => "Search stickers",
                    };
                    ui.add(
                        egui::TextEdit::singleline(&mut self.picker_query)
                            .hint_text(hint)
                            .desired_width(f32::INFINITY),
                    );
                    let query = self.picker_query.trim().to_lowercase();
                    match self.picker_tab {
                        // The grid scrolls itself; an outer scroll area would nest two of them.
                        PickerTab::Emoji => {
                            if let Some(e) = self.emoji_grid(ui, 300.0) {
                                self.input.push_str(&e);
                            }
                        }
                        PickerTab::Custom => {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for pack in self.packs.packs() {
                                        let hits: Vec<_> = pack
                                            .images
                                            .iter()
                                            .filter(|i| {
                                                query.is_empty()
                                                    || i.shortcode.to_lowercase().contains(&query)
                                            })
                                            .cloned()
                                            .collect();
                                        // A pack with nothing matching keeps its header out of the way.
                                        if hits.is_empty() {
                                            continue;
                                        }
                                        ui.label(
                                            egui::RichText::new(&pack.display_name)
                                                .small()
                                                .weak()
                                                .strong(),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            for img in hits {
                                                let mxc = img.mxc_url.clone();
                                                let sc = img.shortcode.clone();
                                                // Ready / pending / failed red shortcode; click retries.
                                                let key = format!("emoji:{mxc}");
                                                let hit = match (
                                                    self.media.texture_for(
                                                        "emoji",
                                                        &mxc,
                                                        Some((24, 24)),
                                                    ),
                                                    self.media.status(&key),
                                                ) {
                                                    (Some(h), _) => ui
                                                        .add(
                                                            egui::Image::new(&h)
                                                                .max_height(20.0)
                                                                .sense(egui::Sense::click()),
                                                        )
                                                        .on_hover_text(format!(":{sc}:"))
                                                        .clicked(),
                                                    (None, Some(entry))
                                                        if entry.error().is_some() =>
                                                    {
                                                        // Click retries; never auto-requeue per frame.
                                                        if ui
                                                            .button(format!(":{sc}:"))
                                                            .on_hover_text(
                                                                "failed — click to retry",
                                                            )
                                                            .clicked()
                                                        {
                                                            self.media.retry(&key);
                                                        }
                                                        false
                                                    }
                                                    (None, _) => ui
                                                        .button(crate::ui::icons::IMAGE)
                                                        .on_hover_text(format!("loading :{sc}:…"))
                                                        .clicked(),
                                                };
                                                if hit {
                                                    self.input.push_str(&format!(":{sc}: "));
                                                }
                                            }
                                        });
                                    }
                                });
                        }
                        PickerTab::Stickers => {
                            let stickers: Vec<(String, String)> = self
                                .packs
                                .stickers()
                                .filter(|(_, i)| {
                                    query.is_empty() || i.shortcode.to_lowercase().contains(&query)
                                })
                                .map(|(_, i)| (i.shortcode.clone(), i.mxc_url.clone()))
                                .collect();
                            if stickers.is_empty() {
                                ui.monospace(if query.is_empty() {
                                    "no sticker packs — stickers live in packs with sticker usage"
                                } else {
                                    "no stickers match"
                                });
                            }
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        for (sc, mxc) in stickers {
                                            let hit = if let Some(h) = self.media.texture_for(
                                                "sticker",
                                                &mxc,
                                                Some((64, 64)),
                                            ) {
                                                ui.add(
                                                    egui::Image::new(&h)
                                                        .max_height(48.0)
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(format!(":{sc}:"))
                                                .clicked()
                                            } else {
                                                ui.button(format!("[{sc}]")).clicked()
                                            };
                                            if hit {
                                                self.input = format!("/sticker {sc}");
                                                self.send_current_input();
                                                self.show_picker = false;
                                            }
                                        }
                                    });
                                });
                        }
                    }
                })
                .map(|w| w.response.rect);
            if !picker_open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.show_picker = false;
            }
            if let Some(rect) = window {
                if crate::ui::clicked_outside(&ctx, rect, opened) {
                    self.show_picker = false;
                }
            }
        }
        self.render_image_preview(ui.ctx());
    }
}

/// Blocking password login on a worker thread; saves the session for cached restore.
/// Fresh logins mint a new device id, so the sqlite crypto store must be fresh too.
fn login_blocking(
    homeserver: String,
    username: String,
    password: String,
) -> Result<LoggedIn, String> {
    use matrix_sdk::Client;
    // Fresh login: wipe any prior-device store so ids never collide.
    let store_dir = password_store_dir(&username);
    let _ = std::fs::remove_file(&store_dir);
    let _ = std::fs::remove_dir_all(&store_dir);
    std::fs::create_dir_all(store_dir.parent().unwrap()).map_err(|e| format!("db dir: {e}"))?;
    let store_dir_s = store_dir.to_string_lossy().into_owned();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async move {
        let client = Client::builder()
            .homeserver_url(&homeserver)
            .sqlite_store(&store_dir, None)
            .build()
            .await
            .map_err(|e| format!("connect {homeserver}: {e}"))?;
        client
            .matrix_auth()
            .login_username(&username, &password)
            .send()
            .await
            .map_err(|e| format!("login as {username}: {e}"))?;
        finish_login(client, &homeserver, &store_dir_s).await
    })
}
/// Collect the room list from synced state. History loads after the UI is ready.
async fn collect_logged_in(
    client: matrix_sdk::Client,
    _homeserver: &str,
) -> Result<LoggedIn, String> {
    // Filter the initial sync too, or it downloads full member state first.
    client
        .sync_once(matrix_sdk::config::SyncSettings::default().filter(sync_filter()))
        .await
        .map_err(|e| format!("sync: {e}"))?;
    let user_id = client
        .user_id()
        .map(|u| u.to_string())
        .unwrap_or("me".into());
    let emoji_usage = crate::recent_emoji::RecentEmoji::load(&client).await?;
    let packs = load_all_packs(&client).await;
    let mut rooms_info = Vec::new();
    // Read room metadata concurrently from the local SDK store.
    const ROOM_CONCURRENCY: usize = 8;
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(ROOM_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for room in client.joined_rooms() {
        let permits = permits.clone();
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await.ok()?;
            let room_id = room.room_id().to_string();
            // Real name, not room id; falls back to DM peer.
            let name = match room.display_name().await {
                Ok(d) => d.to_string(),
                Err(_) => room.name().unwrap_or_else(|| short_room(&room_id)),
            };
            let name = if name.trim().is_empty() || name == "Empty room" {
                short_room(&room_id)
            } else {
                name
            };
            let is_dm = room.is_direct().await.unwrap_or_else(|_| room.is_dm());
            let avatar_mxc = room.avatar_url().map(|u| u.to_string());
            Some(RoomInfo {
                room_id,
                name,
                is_dm,
                avatar_mxc,
            })
        });
    }
    while let Some(done) = tasks.join_next().await {
        if let Ok(Some(info)) = done {
            rooms_info.push(info);
        }
    }

    Ok(LoggedIn {
        client,
        user_id,
        rooms: rooms_info,
        packs,
        emoji_usage,
        session_metadata: None,
        persistence_warning: None,
    })
}

/// Load all image packs: per-room `im.ponies.room_emotes` state + MSC2545 user
/// packs from account data, raw-JSON decoded (no `unstable-msc2545` needed).
async fn load_all_packs(client: &matrix_sdk::Client) -> crate::matrix::PackStore {
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
async fn load_initial_history(
    client: &matrix_sdk::Client,
    room_id: &str,
) -> Result<(Vec<TimelineRow>, Option<String>, Vec<Member>), String> {
    let id = matrix_sdk::ruma::OwnedRoomId::try_from(room_id).map_err(|e| e.to_string())?;
    let room = client.get_room(&id).ok_or("room not found")?;
    let members: Vec<Member> = room
        .members_no_sync(matrix_sdk::RoomMemberships::ACTIVE)
        .await
        .map_err(|e| format!("members: {e}"))?
        .into_iter()
        .take(50)
        .map(|m| Member {
            display: m.display_name().unwrap_or_else(|| m.name()).to_owned(),
            mxid: m.user_id().to_string(),
            online: true,
            avatar_mxc: m.avatar_url().map(|u| u.to_string()),
        })
        .collect();
    let avatars = members
        .iter()
        .map(|m| (m.mxid.clone(), m.avatar_mxc.clone()))
        .collect();
    let (rows, token) = load_room_history(&room, &avatars, None, INITIAL_HISTORY).await?;
    Ok((rows, token, members))
}

/// One screenful per history request. Older messages load on scroll.
const INITIAL_HISTORY: u32 = 50;
/// Guard rail: each page is its own round trip.
const _: () = assert!(INITIAL_HISTORY <= 100);

/// Fetch one page of history, newest last. Returns rows plus the token for
/// the page before them (`None` = start of room).
async fn load_room_history(
    room: &matrix_sdk::Room,
    avatars: &std::collections::HashMap<String, Option<String>>,
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
            let display = sender
                .trim_start_matches('@')
                .split(':')
                .next()
                .unwrap_or(&sender)
                .to_owned();
            let content = json.get("content").cloned().unwrap_or_default();
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

/// Shared decoder for history + live sync, so arrivals render like backfill.
#[allow(clippy::too_many_arguments)] // flat decode signature, not a struct
async fn decode_timeline_json(
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
        sender: sender.to_owned(),
        display_name: display.to_owned(),
        body: body.to_owned(),
        formatted,
        avatar_mxc,
        reply_to,
        reply_to_id,
        thread_count: 0,
        image: image.or(sticker_image),
        reactions: vec![],
        edited: false,
        seen_by: Vec::new(),
        is_sticker: event_type == "m.sticker",
        txn_id: None,
    }
}

fn plain_media_source(mxc: &str) -> matrix_sdk::ruma::events::room::MediaSource {
    let uri: &matrix_sdk::ruma::MxcUri = mxc.into();
    matrix_sdk::ruma::events::room::MediaSource::Plain(uri.to_owned())
}

fn media_source_mxc(source: &matrix_sdk::ruma::events::room::MediaSource) -> String {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => uri.to_string(),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(file) => file.url.to_string(),
    }
}

fn thumbnail_media_source(
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
async fn decode_sync_batch(
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
    for (room_id, update) in &resp.rooms.joined {
        let room_id_s = room_id.to_string();
        let Some(room) = client.get_room(room_id) else {
            continue;
        };
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
            let display = sender
                .trim_start_matches('@')
                .split(':')
                .next()
                .unwrap_or(&sender)
                .to_owned();
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
            // An `m.replace` edit rewrites its target, never appends a new row.
            let relates = content.get("m.relates_to").cloned().unwrap_or_default();
            if relates.get("rel_type").and_then(|v| v.as_str()) == Some("m.replace") {
                let target = relates
                    .get("event_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                // `m.new_content` is the replacement; top-level body is the legacy fallback.
                let new = content.get("m.new_content").cloned().unwrap_or_default();
                let new_body = new
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
                let new_fmt = new
                    .get("formatted_body")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
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
                    for (user_id, _) in users {
                        batch
                            .receipts
                            .push((room_id_s.clone(), event_id.clone(), user_id.clone()));
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
    batch
}

/// Sender avatar mxc from the member state event (cheap, cached by sync).
async fn avatar_for_sender(room: &matrix_sdk::Room, sender: &str) -> Option<String> {
    use matrix_sdk::ruma::OwnedUserId;
    let Ok(uid) = OwnedUserId::try_from(sender) else {
        return None;
    };
    let member = room.get_member_no_sync(&uid).await.ok()??;
    member.avatar_url().map(|u| u.to_string())
}

fn empty_row(body: &str) -> TimelineRow {
    TimelineRow {
        id: "empty".into(),
        ts: format_ts(now_millis()),
        sender: "system".into(),
        display_name: "system".into(),
        body: body.into(),
        formatted: None,
        avatar_mxc: None,
        reply_to: None,
        reply_to_id: None,
        thread_count: 0,
        image: None,
        reactions: vec![],
        is_sticker: false,
        txn_id: None,
        edited: false,
        seen_by: Vec::new(),
    }
}

/// Non-secret login metadata. Tokens live in Secret Service; the store directory
/// must stay paired with the same Matrix device when restoring a session.
fn session_path() -> std::path::PathBuf {
    dirs_data_dir().join("thrace").join("session.json")
}

/// A missing wallet permits an in-memory login, with a visible persistence warning.
async fn finish_login(
    client: matrix_sdk::Client,
    homeserver: &str,
    store_dir: &str,
) -> Result<LoggedIn, String> {
    let session = client.matrix_auth().session().ok_or("No login session")?;
    let metadata = crate::session_store::SessionMetadata::new(homeserver, store_dir, &session);
    let saved = crate::session_store::save(&session_path(), &metadata, &session).await;
    let mut logged = collect_logged_in(client, homeserver).await?;
    logged.session_metadata = Some(metadata);
    logged.persistence_warning = saved
        .err()
        .map(|error| format!("Login is not saved: {error:#}"));
    Ok(logged)
}

/// Per-user password store dir (fresh device per login; wiped above).
fn password_store_dir(username: &str) -> std::path::PathBuf {
    let safe_user: String = username
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    dirs_data_dir()
        .join("thrace")
        .join(format!("{safe_user}.sqlite"))
}

/// Restore the same device without deleting its saved session on transient errors.
fn restore_blocking() -> Result<LoggedIn, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async move {
        use matrix_sdk::{AuthSession, Client};
        let (metadata, session) = crate::session_store::load(&session_path())
            .await
            .map_err(|e| format!("restore: {e:#}"))?
            .ok_or("No saved session")?;
        let client = Client::builder()
            .homeserver_url(&metadata.homeserver)
            .sqlite_store(&metadata.store_dir, None)
            .build()
            .await
            .map_err(|e| format!("connect: {e}"))?;
        client
            .restore_session(AuthSession::Matrix(session))
            .await
            .map_err(|e| format!("restore: {e}"))?;
        let mut logged = collect_logged_in(client, &metadata.homeserver).await?;
        logged.session_metadata = Some(metadata);
        Ok(logged)
    })
}

/// One-click SSO/PocketID flow on a worker thread: bind 127.0.0.1:8008, open
/// the URL in the browser, wait for the loopback token, finish login + sync.
fn sso_auto_flow(homeserver: String, tx: std::sync::mpsc::Sender<LoginMsg>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:8008").and_then(|l| {
        l.set_nonblocking(false)?;
        Ok(l)
    });
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(LoginMsg::SsoDone(Err(format!(
                "loopback :8008 busy: {e} — use token paste"
            ))));
            return;
        }
    };
    if let Err(e) = sso_build_and_wait(homeserver, listener, tx.clone()) {
        let _ = tx.send(LoginMsg::SsoDone(Err(e)));
    }
}

/// Build the SSO client + URL. Each attempt gets a unique store dir: SSO mints
/// a new device id, and a reused store fails the account-match check.
fn sso_build_only(homeserver: &str) -> Result<(matrix_sdk::Client, String, String), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async {
        use matrix_sdk::Client;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir_name = format!("sso-{}-{}.sqlite", millis, std::process::id());
        let db_dir = dirs_data_dir().join("thrace").join(dir_name);
        // Drop the legacy shared store so it can never collide again.
        let _ = std::fs::remove_file(dirs_data_dir().join("thrace").join("sso.sqlite"));
        let _ = std::fs::remove_dir_all(dirs_data_dir().join("thrace").join("sso.sqlite"));
        let store_dir_s = db_dir.to_string_lossy().into_owned();
        let client = Client::builder()
            .homeserver_url(homeserver)
            .sqlite_store(&db_dir, None)
            .build()
            .await
            .map_err(|e| format!("connect {homeserver}: {e}"))?;
        let url = client
            .matrix_auth()
            .get_sso_login_url("http://localhost:8008/callback", None)
            .await
            .map_err(|e| format!("sso url: {e}"))?;
        Ok((client, url.to_string(), store_dir_s))
    })
}

/// Build client, send URL, wait for the loopback token, finish. `Err` only
/// when Done already went out via tx.
fn sso_build_and_wait(
    homeserver: String,
    listener: std::net::TcpListener,
    tx: std::sync::mpsc::Sender<LoginMsg>,
) -> Result<(), String> {
    let (client, url, store_dir) = sso_build_only(&homeserver)?;
    let _ = tx.send(LoginMsg::SsoUrlReady(Ok((
        client.clone(),
        url,
        store_dir.clone(),
    ))));
    // Block for the PocketID → Synapse → loopback redirect (worker thread only).
    let token = wait_for_loopback_token(listener)?;
    let res = sso_finish_blocking(client, store_dir, token);
    let _ = tx.send(LoginMsg::SsoDone(res));
    Ok(())
}

/// Minimal loopback HTTP: one GET, parse loginToken, reply 200, return it.
fn wait_for_loopback_token(listener: std::net::TcpListener) -> Result<String, String> {
    use std::io::{Read, Write};
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| format!("loopback accept: {e}"))?;
        let mut buf = [0u8; 8192];
        let n = stream
            .read(&mut buf)
            .map_err(|e| format!("loopback read: {e}"))?;
        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = "<html><body><h2>Logged in — you can close this tab</h2></body></html>";
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = stream.write_all(resp.as_bytes());
        let line = req.lines().next().unwrap_or("");
        if let Some(token) = extract_token_from_get(line) {
            if !token.is_empty() {
                return Ok(token);
            }
        }
        // Ignore favicon etc; keep waiting.
    }
    Err("SSO timed out waiting on localhost:8008".into())
}

fn extract_token_from_get(request_line: &str) -> Option<String> {
    // "GET /callback?loginToken=ABC&x=1 HTTP/1.1"; reuse the query parser.
    let path = request_line.split_whitespace().nth(1)?;
    let fake = format!("http://localhost{path}");
    let parsed = url::Url::parse(&fake).ok()?;
    for (k, v) in parsed.query_pairs() {
        if k == "loginToken" {
            return Some(v.into_owned());
        }
    }
    None
}

/// Finish SSO with a loginToken (worker thread). `store_dir` stays the unique
/// dir from the pre-login client.
fn sso_finish_blocking(
    client: matrix_sdk::Client,
    store_dir: String,
    token_or_url: String,
) -> Result<LoggedIn, String> {
    // Homeserver comes from the client, for session persist.
    let hs = client.homeserver().to_string();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("rt: {e}"))?;
    rt.block_on(async move {
        let token = extract_login_token(&token_or_url);
        client
            .matrix_auth()
            .login_token(&token)
            .send()
            .await
            .map_err(|e| format!("sso login: {e}"))?;
        finish_login(client, &hs, &store_dir).await
    })
}

/// Accept a raw loginToken or a full `...?loginToken=XYZ` callback URL.
fn extract_login_token(s: &str) -> String {
    let t = s.trim();
    if let Some(i) = t.find("loginToken=") {
        let rest = &t[i + "loginToken=".len()..];
        let end = rest.find('&').unwrap_or(rest.len());
        return rest[..end].to_owned();
    }
    t.to_owned()
}

fn dirs_data_dir() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home).join(".local/share")
    } else {
        std::env::temp_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, sender: &str, body: &str, txn: Option<&str>) -> TimelineRow {
        TimelineRow {
            id: id.into(),
            ts: format_ts(now_millis()),
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
            reactions: vec![],
            is_sticker: false,
            txn_id: txn.map(str::to_owned),
        }
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
            online: false,
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
    fn mention_prefix_only_completes_the_word_being_typed() {
        assert_eq!(mention_prefix("hey @al"), Some("@al"));
        assert_eq!(mention_prefix("@al"), Some("@al"));
        // Not a mention at all.
        assert_eq!(mention_prefix("hello there"), None);
        // Only the final word is a candidate; completing must not move the cursor.
        assert_eq!(mention_prefix("@bob said hi"), None);
        // Full mxid: nothing left to complete.
        assert_eq!(mention_prefix("@bob:matrix.org"), None);
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
        let known: std::collections::HashSet<&str> =
            existing.iter().map(|r| r.id.as_str()).collect();
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

        assert!(!seen_in(&mut rows, "$not-loaded", "@reader:hs"));
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
        assert!(seen_in(&mut rows, "$a", "@reader:hs"));
        assert_eq!(rows[0].seen_by.len(), 1);

        assert!(seen_in(&mut rows, "$b", "@reader:hs"));
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
            .push(("!r:hs".into(), "$e".into(), "@u:hs".into()));
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
        let bin =
            PendingUpload::from_bytes("archive.zzz-unknown".to_owned(), vec![0u8; 16]).unwrap();
        assert!(!bin.is_image());
        assert_eq!(bin.mime, "application/octet-stream");
        assert!(PendingUpload::from_bytes(
            "big.bin".to_owned(),
            vec![0u8; PendingUpload::MAX_BYTES + 1]
        )
        .is_err());
    }
}
