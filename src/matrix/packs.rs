/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Cinny-compatible custom emoji / sticker pack store.
//!
//! Aggregates usable packs = current-room packs + global user packs + every
//! joined room (so `:shortcode:` renders in any room). Backed by raw
//! `im.ponies.room_emotes` state content so we stay protocol-compliant.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct PackImage {
    pub shortcode: String,
    pub mxc_url: String,
    pub is_sticker: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackAddress {
    pub room_id: String,
    pub state_key: String,
}

#[derive(Debug, Clone, Default)]
pub struct ImagePack {
    pub address: Option<PackAddress>,
    pub display_name: String,
    pub images: Vec<PackImage>,
}

/// In-memory pack store.
#[derive(Debug, Default)]
pub struct PackStore {
    packs: Vec<ImagePack>,
    /// shortcode -> (pack_idx, image_idx) for O(1) `:foo:` lookup.
    index: HashMap<String, (usize, usize)>,
}

impl PackStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_pack(&mut self, pack: ImagePack) {
        if let Some(addr) = &pack.address {
            self.packs.retain(|p| p.address.as_ref() != Some(addr));
        }
        self.packs.push(pack);
        self.reindex();
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (pi, pack) in self.packs.iter().enumerate() {
            for (ii, img) in pack.images.iter().enumerate() {
                // First pack wins — same as Cinny.
                self.index.entry(img.shortcode.clone()).or_insert((pi, ii));
            }
        }
    }

    /// Resolve with or without surrounding colons.
    pub fn resolve(&self, shortcode: &str) -> Option<&PackImage> {
        let key = shortcode.trim_matches(':');
        let (pi, ii) = self.index.get(key)?;
        Some(&self.packs[*pi].images[*ii])
    }

    pub fn packs(&self) -> &[ImagePack] {
        &self.packs
    }

    /// Sticker images for the sticker tab.
    pub fn stickers(&self) -> impl Iterator<Item = (&ImagePack, &PackImage)> {
        self.packs
            .iter()
            .flat_map(|p| p.images.iter().map(move |i| (p, i)))
            .filter(|(_, i)| i.is_sticker)
    }

    /// Decode the wire shape of `im.ponies.room_emotes` content.
    /// `content`: `{ images: { sc: { url } }, pack: { display_name, usage } }`
    pub fn decode_state_content(
        room_id: &str,
        state_key: &str,
        content: &serde_json::Value,
    ) -> ImagePack {
        let display_name = content
            .pointer("/pack/display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Room emotes")
            .to_owned();
        let usage: Vec<String> = content
            .pointer("/pack/usage")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let pack_is_sticker = usage.iter().any(|u| u == "sticker");
        let mut images = Vec::new();
        if let Some(map) = content.get("images").and_then(|v| v.as_object()) {
            for (shortcode, v) in map {
                if let Some(url) = v.get("url").and_then(|u| u.as_str()) {
                    let img_usage = v
                        .get("usage")
                        .and_then(|u| u.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_owned))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let is_sticker = if img_usage.is_empty() {
                        pack_is_sticker
                    } else {
                        img_usage.iter().any(|u| u == "sticker")
                    };
                    images.push(PackImage {
                        shortcode: shortcode.clone(),
                        mxc_url: url.to_owned(),
                        is_sticker,
                    });
                }
            }
        }
        images.sort_by(|a, b| a.shortcode.cmp(&b.shortcode));
        ImagePack {
            address: Some(PackAddress {
                room_id: room_id.to_owned(),
                state_key: state_key.to_owned(),
            }),
            display_name,
            images,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_room_emotes_state() {
        let content = serde_json::json!({
            "images": {
                "dance": { "url": "mxc://hs/aaa" },
                "party": { "url": "mxc://hs/bbb", "usage": ["sticker"] }
            },
            "pack": { "display_name": "Cool", "usage": ["emoticon"] }
        });
        let pack = PackStore::decode_state_content("!r:hs", "", &content);
        assert_eq!(pack.images.len(), 2);
        assert!(pack
            .images
            .iter()
            .any(|i| i.shortcode == "dance" && !i.is_sticker));
        assert!(pack
            .images
            .iter()
            .any(|i| i.shortcode == "party" && i.is_sticker));
    }

    #[test]
    fn resolves_across_packs_first_wins() {
        let mut store = PackStore::new();
        store.upsert_pack(ImagePack {
            address: None,
            display_name: "a".into(),
            images: vec![PackImage {
                shortcode: "x".into(),
                mxc_url: "mxc://a".into(),
                is_sticker: false,
            }],
        });
        store.upsert_pack(ImagePack {
            address: None,
            display_name: "b".into(),
            images: vec![PackImage {
                shortcode: "x".into(),
                mxc_url: "mxc://b".into(),
                is_sticker: false,
            }],
        });
        assert_eq!(store.resolve(":x:").unwrap().mxc_url, "mxc://a");
    }
}
