/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Material icons from Symbols Nerd Font (outline glyphs; egui can't rasterise colour bitmaps).
//!
//! Codepoints verified against the installed font with fontTools — missing glyphs render blank with
//! no compile-time check. Installed as fallback in `main.rs::install_fonts`; if missing, icons go
//! blank but tooltips still identify buttons.

/// Startup probe confirming the icon font loaded.
pub const PROBE: &str = SEND;

// composer
pub const SEND: &str = "\u{F048A}"; // md-send
pub const ATTACH: &str = "\u{F03E2}"; // md-paperclip
pub const EMOJI: &str = "\u{F01F2}"; // md-emoticon_outline
pub const STICKER: &str = "\u{F0785}"; // md-sticker_emoji
pub const PASTE: &str = "\u{F0192}"; // md-content_paste

// title bar
pub const COG: &str = "\u{F08BB}"; // md-cog_outline
pub const SHIELD: &str = "\u{F0CCC}"; // md-shield_lock_outline
pub const LOGOUT: &str = "\u{F0343}"; // md-logout
pub const PALETTE: &str = "\u{F0E0C}"; // md-palette_outline
pub const SEARCH: &str = "\u{F0349}"; // md-magnify

// message actions
pub const REPLY: &str = "\u{F0F20}"; // md-reply_outline
pub const ADD_REACTION: &str = "\u{F0C68}"; // md-emoticon
pub const THREAD: &str = "\u{F0181}"; // md-comment_multiple_outline
pub const MORE: &str = "\u{F01D8}"; // md-dots_horizontal
pub const DOWNLOAD: &str = "\u{F0120}"; // md-tray_arrow_down

// profile / people
pub const ACCOUNT: &str = "\u{F0B55}"; // md-account_circle_outline
pub const DM: &str = "\u{F000F}"; // md-account_multiple_outline
pub const AT: &str = "\u{F0065}"; // md-at
pub const COPY: &str = "\u{F018F}"; // md-content_copy
pub const VERIFIED: &str = "\u{F1740}"; // md-check_decagram_outline

// rooms / sidebar
pub const CHAT: &str = "\u{F0365}"; // md-message_outline
pub const LINK: &str = "\u{F03CC}"; // md-open_in_new
pub const HASH: &str = "\u{F0423}"; // md-pound
pub const LOCK: &str = "\u{F0341}"; // md-lock_outline
pub const CHEVRON_LEFT: &str = "\u{F0141}"; // md-chevron_left
pub const CHEVRON_DOWN: &str = "\u{F0140}"; // md-chevron_down
pub const CHEVRON_RIGHT: &str = "\u{F0142}"; // md-chevron_right

// state
pub const DOT: &str = "\u{F09DE}"; // md-circle_medium — online / unread
pub const CIRCLE_SM: &str = "\u{F09DF}"; // md-circle_small — offline
pub const CHECK: &str = "\u{F012C}"; // md-check
pub const ALERT: &str = "\u{F05D6}"; // md-alert_circle_outline
pub const CLOSE: &str = "\u{F0156}"; // md-close
pub const REFRESH: &str = "\u{F0450}"; // md-refresh
pub const EYE: &str = "\u{F06D0}"; // md-eye_outline — read receipts

// media
pub const IMAGE: &str = "\u{F0976}"; // md-image_outline
pub const BROKEN: &str = "\u{F02EE}"; // md-image_broken_variant
pub const FILE: &str = "\u{F0224}"; // md-file_outline
pub const DELETE: &str = "\u{F09E7}"; // md-delete_outline
pub const LOADING: &str = "\u{F0996}"; // md-progress_clock
