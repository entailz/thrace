/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Sidebar | timeline | members. Chrome is ascii/unicode only; emoji live in picker + content.

use crate::app::text::localpart;
use crate::matrix::PackStore;
use crate::theme::{self, ThemeFile};
use std::future::Future;
use std::sync::Arc;

mod decode;
mod dm;
mod media;
mod pins;
mod prefs;
mod rows;
mod send;
mod session;
mod sync;
mod text;
mod view;

#[cfg(test)]
mod tests;

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

/// Audio attachment (`m.audio` via mxc): voice notes and music.
#[derive(Debug, Clone)]
pub struct AudioAttachment {
    pub mxc: String,
    pub source: matrix_sdk::ruma::events::room::MediaSource,
    pub name: String,
    pub mime: String,
    /// Track length in ms, from `info` or MSC1767.
    pub duration_ms: Option<u64>,
    /// Bar heights 0..=1 from MSC1767, empty when the sender sent none.
    pub waveform: Vec<f32>,
    /// MSC3245 voice note: compact bubble rather than file row.
    pub is_voice: bool,
}

/// Timeline row with reply/thread support.
#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub id: String,
    pub ts: String,
    pub origin_server_ts: u64,
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
    pub audio: Option<AudioAttachment>,
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

#[derive(Debug, Clone)]
pub struct PinnedMessage {
    pub id: String,
    pub sender: String,
    pub display_name: String,
    pub avatar_mxc: Option<String>,
    pub body: String,
    pub ts: String,
    pub origin_server_ts: u64,
    pub timeline_row: Option<TimelineRow>,
}

/// One reaction group: emoji key + sender mxids.
#[derive(Debug, Clone)]
pub struct Reaction {
    pub key: String,
    pub senders: Vec<Reactor>,
    /// Server-bundled total per key; covers reactions from before this session.
    pub bundled: usize,
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
        self.senders.len().max(self.bundled)
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
pub(in crate::app) enum SendResult {
    Done(String),
    Failed(String),
}

/// DM creation result (worker → UI): swap the local stub for the real room.
#[derive(Debug)]
pub(in crate::app) enum DmResult {
    Created { stub_id: String, room_id: String },
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
    /// Rooms with a scrollback page in flight; one request per room, so a slow
    /// page in one room cannot block scrolling back in another.
    paginating: std::collections::HashSet<String>,
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
    /// Running audio playback: (event id, player). One track at a time.
    audio: Option<(String, crate::audio::AudioPlayer)>,
    /// Downloaded audio temp files by mxc; replay needs no second download.
    audio_paths: std::collections::HashMap<String, std::path::PathBuf>,
    /// Computed waveforms by event id, when the event sent none.
    audio_waves: std::collections::HashMap<String, Vec<f32>>,
    /// Resolved durations in seconds by event id, when the event reported none.
    audio_lengths: std::collections::HashMap<String, f64>,
    /// Per-event audio errors (missing ffplay, undecodable file).
    audio_errors: std::collections::HashMap<String, String>,
    /// Event ids with an audio download in flight.
    audio_pending: std::collections::HashSet<String>,
    /// Ready audio files arriving from the worker.
    audio_rx: Option<std::sync::mpsc::Receiver<(String, Result<AudioReady, String>)>>,
    audio_tx: Option<std::sync::mpsc::Sender<(String, Result<AudioReady, String>)>>,
    /// Messages whose reaction senders were backfilled from relations.
    relations_fetched: std::collections::HashSet<String>,
    /// A relation batch is in flight; keeps scrolling from spawning one per frame.
    relations_busy: bool,
    /// Backfilled reaction senders arriving from the worker: (room, per-target results).
    react_rx: Option<std::sync::mpsc::Receiver<(String, RelationResults)>>,
    react_tx: Option<std::sync::mpsc::Sender<(String, RelationResults)>>,
    /// Sidebar room menu: (room id, anchor, frame it opened on).
    room_menu: Option<(String, egui::Pos2, u64)>,
    /// Open pinned-message list and its room-scoped fetch result.
    pinned_room: Option<String>,
    pinned_messages: Vec<PinnedMessage>,
    pinned_loading: bool,
    pinned_error: Option<String>,
    pinned_rx: Option<std::sync::mpsc::Receiver<(String, Result<Vec<PinnedMessage>, String>)>>,
    /// Rooms we have tagged as favourites, for the menu's toggle state.
    favourites: std::collections::HashSet<String>,
    /// Profile card: (mxid, anchor). Opened by clicking an avatar or name in
    /// the timeline or the member list.
    profile_target: Option<(String, egui::Pos2, u64)>,
    /// Pending outgoing sends (echoed locally, confirmed on Sent).
    send_rx: Option<std::sync::mpsc::Receiver<SendResult>>,
    send_tx: Option<std::sync::mpsc::Sender<SendResult>>,
    /// DM room creation (worker → UI); drained every frame.
    dm_rx: Option<std::sync::mpsc::Receiver<DmResult>>,
    dm_tx: Option<std::sync::mpsc::Sender<DmResult>>,
    /// Live sync: fresh timeline events (worker → UI), drained every frame.
    sync_rx: Option<std::sync::mpsc::Receiver<SyncBatch>>,
    /// Live sync runs once per login; re-login spawns a fresh loop.
    sync_running: bool,
    /// Live-sync task; aborted on logout/re-login so loops never overlap.
    sync_task: Option<tokio::task::JoinHandle<()>>,
    /// Newest event each room's read receipt was sent for; stops re-sending it
    /// every frame. Per room, or returning to a room would re-send it.
    last_receipt: std::collections::HashMap<String, String>,
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
pub(in crate::app) struct SyncBatch {
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
}

impl SyncBatch {
    /// True when nothing here changes the UI.
    pub(in crate::app) fn is_empty(&self) -> bool {
        self.emoji_usage.is_none()
            && self.rows.is_empty()
            && self.reactions.is_empty()
            && self.verify_flows.is_empty()
            && self.receipts.is_empty()
            && self.redactions.is_empty()
            && self.edits.is_empty()
    }
}

/// Downloaded + measured audio file arriving from the worker.
pub(in crate::app) struct AudioReady {
    mxc: String,
    path: std::path::PathBuf,
    duration_secs: f64,
    waveform: Option<Vec<f32>>,
}

/// One `m.reaction`: target + key + sender.
#[derive(Debug, Clone)]
pub(in crate::app) struct SyncReaction {
    /// The message being reacted to.
    target: String,
    key: String,
    sender: String,
    /// Own event id; named by a later redaction.
    event_id: String,
}

/// Backfilled relations per message: target event id plus senders, or `None`
/// when that message's request failed.
pub(in crate::app) type RelationResults = Vec<(String, Option<Vec<SyncReaction>>)>;

/// Skin-tone modifiers, applied on insert.
pub(in crate::app) const SKIN_TONES: &[(&str, &str)] = &[
    ("", "none"),
    ("🏻", "light"),
    ("🏼", "medium-light"),
    ("🏽", "medium"),
    ("🏾", "medium-dark"),
    ("🏿", "dark"),
];

/// Slash commands; every entry is dispatched in `send_current_input`.
pub(in crate::app) const SLASH_COMMANDS: &[(&str, &str)] = &[
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
pub(in crate::app) struct RoomEntry {
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
pub(in crate::app) struct RoomInfo {
    room_id: String,
    name: String,
    is_dm: bool,
    avatar_mxc: Option<String>,
}

#[derive(Debug, Clone)]
pub(in crate::app) struct Member {
    display: String,
    mxid: String,
    online: bool,
    avatar_mxc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum PickerTab {
    Emoji,
    Custom,
    Stickers,
}

/// Staged upload: bytes + name + mime. 25 MiB cap.
#[derive(Debug, Clone)]
pub(in crate::app) struct PendingUpload {
    name: String,
    mime: String,
    bytes: Vec<u8>,
}

impl PendingUpload {
    pub(in crate::app) const MAX_BYTES: usize = 25 * 1024 * 1024;

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
pub(in crate::app) struct LoginForm {
    homeserver: String,
    username: String,
    password: String,
    sso_token_input: String,
    sso_url: String,
    error: String,
    busy: bool,
    show_sso_token: bool,
}

pub(in crate::app) struct LoggedIn {
    client: matrix_sdk::Client,
    user_id: String,
    rooms: Vec<RoomInfo>,
    packs: crate::matrix::PackStore,
    emoji_usage: crate::recent_emoji::RecentEmoji,
    session_metadata: Option<crate::session_store::SessionMetadata>,
    persistence_warning: Option<String>,
}

/// Initial history and cached members, delivered independently for each room.
pub(in crate::app) struct InitialHistory {
    room_id: String,
    result: Result<(Vec<TimelineRow>, Option<String>, Vec<Member>), String>,
}

pub(in crate::app) enum LoginMsg {
    PasswordDone(Result<LoggedIn, String>),
    SsoUrlReady(Result<(matrix_sdk::Client, String, String), String>),
    SsoDone(Result<LoggedIn, String>),
}

/// One scrollback page: room, older rows, token for the page before (`None` at start).
pub(in crate::app) type HistoryPage = (String, Result<(Vec<TimelineRow>, Option<String>), String>);

/// Settings sections.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum SettingsTab {
    Account,
    Appearance,
    Notifications,
    Sessions,
    Privacy,
    Emoji,
    Links,
}

impl SettingsTab {
    pub(in crate::app) const ALL: [SettingsTab; 7] = [
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
pub(in crate::app) enum RoomMenuAction {
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
pub(in crate::app) enum RoomNotify {
    All,
    MentionsOnly,
    Mute,
}

impl RoomNotify {
    pub(in crate::app) fn label(self) -> &'static str {
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
pub(in crate::app) enum ProfileAction {
    Message,
    Mention,
    Verify,
    CopyId,
}

/// Moderation verbs that take `@user:hs [extra]`.
#[derive(Clone, Copy)]
pub(in crate::app) enum Moderation {
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
pub(in crate::app) enum RoomAction {
    Join,
    Invite,
    Topic,
    Leave,
}

/// Count rooms a user shares with us. `members_by_room` excludes the room on
/// screen, whose list lives in `members`.
pub(in crate::app) fn count_shared_rooms(
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
            audio: None,
            audio_paths: Default::default(),
            audio_waves: Default::default(),
            audio_lengths: Default::default(),
            audio_errors: Default::default(),
            audio_pending: Default::default(),
            audio_rx: None,
            audio_tx: None,
            relations_fetched: Default::default(),
            relations_busy: false,
            react_rx: None,
            react_tx: None,
            profile_target: None,
            room_menu: None,
            pinned_room: None,
            pinned_messages: Vec::new(),
            pinned_loading: false,
            pinned_error: None,
            pinned_rx: None,
            favourites: std::collections::HashSet::new(),
            send_rx: None,
            send_tx: None,
            dm_rx: None,
            dm_tx: None,
            sync_rx: None,
            sync_running: false,
            sync_task: None,
            last_receipt: Default::default(),
            own_user: String::new(),
            timelines: std::collections::HashMap::new(),
            history_queue: Default::default(),
            history_tasks: Default::default(),
            history_rx: None,
            history_tx: None,
            back_tokens: std::collections::HashMap::new(),
            paginating: Default::default(),
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

    /// Display name for an mxid.
    pub(in crate::app) fn display_for(&self, mxid: &str) -> String {
        self.members
            .iter()
            .find(|m| m.mxid == mxid)
            .map(|m| m.display.clone())
            .unwrap_or_else(|| localpart(mxid))
    }

    /// Known user info without a round trip.
    pub(in crate::app) fn profile_of(&self, mxid: &str) -> (String, Option<String>, bool) {
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
    pub(in crate::app) fn shared_rooms(&self, mxid: &str) -> usize {
        count_shared_rooms(&self.members, &self.members_by_room, mxid)
    }

    /// Current room id (None when logged out / no rooms yet).
    pub(in crate::app) fn current_room_id(&self) -> Option<String> {
        let id = self.rooms.get(self.current)?.room_id.clone();
        if id.starts_with("dm:") && self.client.is_none() {
            return None;
        }
        Some(id)
    }

    /// Lazily create the send channel.
    pub(in crate::app) fn send_channel(&mut self) -> std::sync::mpsc::Sender<SendResult> {
        if self.send_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.send_tx = Some(tx.clone());
            self.send_rx = Some(rx);
        }
        self.send_tx.clone().unwrap()
    }

    /// Queue a send worker; failures surface in status (no unsend).
    pub(in crate::app) fn spawn_send<F>(&mut self, fut: F)
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
    pub(in crate::app) fn spawn_task<F>(&self, fut: F)
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
    pub(in crate::app) fn spawn_blocking_task<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            f();
            ctx.request_repaint();
        });
    }

    pub(in crate::app) fn poll_send(&mut self) {
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

    /// Nanos-based client txn id (unique per send in this process).
    pub(in crate::app) fn uuid_txn() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{n:x}")
    }
}
