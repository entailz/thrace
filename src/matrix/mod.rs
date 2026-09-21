/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Matrix glue: client handle + image-pack store.
//!
//! - custom emotes: `im.ponies.room_emotes` + MSC2545 `im.ponies.user_emotes`
//!   `{ images: { shortcode: { url } }, pack: { display_name, usage } }`
//! - stickers: `m.sticker` `{ body, url, info }`
//! - reactions: `m.reaction`, `m.relates_to = { rel_type: "m.annotation", event_id, key }`,
//!   key is unicode or `:shortcode:`.

pub mod packs;

pub use packs::{ImagePack, PackImage, PackStore};
