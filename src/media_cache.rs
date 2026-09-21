/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! mxc → egui texture cache.
//!
//! Downloads via `Client::media().get_media_content`, decodes with `image`
//! (downscaling anything over the pixel budget rather than rejecting it), and
//! serves `TextureHandle`s keyed by a caller-supplied cache key.
//!
//! Threading: the cache lives on the UI thread (textures are `!Send`). Worker
//! tasks on the shared runtime download *and decode* — `MediaBytes` carries a
//! finished `ColorImage`, so the UI thread only uploads it. Decoding a 4 MP
//! JPEG costs ~100 ms; doing that inline would hitch every frame an image
//! lands on.
//!
//! Memory: ready textures live in an LRU capped at [`TEXTURE_CACHE_CAP`]
//! entries. Eviction drops the `TextureHandle`, which frees the GPU
//! allocation; if the image scrolls back into view it is simply re-fetched.
//! Without the cap, every avatar and image ever scrolled past stayed resident
//! for the life of the process.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::time::Duration;

/// Ready textures kept resident. ~256 covers several screens of avatars and
/// images; past that the oldest are dropped and re-fetched on demand.
pub const TEXTURE_CACHE_CAP: usize = 256;

/// Pixel budget for thumbnails (avatars, timeline images) and for originals
/// opened in the preview window. Over budget images are *downscaled*, not
/// dropped — homeservers can't thumbnail encrypted media, so the original is
/// all an E2EE room ever gets.
pub const THUMB_MAX_PIXELS: u64 = 4_000_000;
pub const ORIGINAL_MAX_PIXELS: u64 = 40_000_000;

/// Hard decode bounds, applied before any pixels are allocated. Guards against
/// a malicious or corrupt header claiming 60000×60000.
const MAX_DECODE_DIM: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// One media request gets this long before it is failed. Without it, a single
/// stalled connection used to wedge the whole download queue.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// In-flight download request (UI → worker).
#[derive(Debug, Clone)]
pub struct MediaFetch {
    /// Cache key. Namespaced by caller (`avatar:`, `thumbnail:`, `emoji:`,
    /// `original:`) so the same mxc requested at different sizes does not
    /// collide — an avatar wanted at 32px and a custom emoji at 24px are
    /// different entries.
    pub mxc: String,
    pub source: matrix_sdk::ruma::events::room::MediaSource,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub thumbnail: bool,
}

/// One decoded still, or an animation's frames with their delays.
pub enum Decoded {
    Still(egui::ColorImage),
    /// `(frame, delay in ms)`, in order.
    Frames(Vec<(egui::ColorImage, u32)>),
}

/// A decoded image on its way back to the UI thread (worker → UI).
pub struct MediaBytes {
    pub mxc: String,
    pub image: Result<Decoded, String>,
    /// The request that produced this, kept so a failure can be retried
    /// without the UI having to remember what it asked for.
    pub fetch: Option<MediaFetch>,
}

/// Cache entry for a key that has no texture yet.
pub enum MediaEntry {
    /// In flight. `attempts` carries across retries — it lives on `Pending`
    /// too, because `retry_due` moves an entry back here, and counting only
    /// on `Failed` reset the tally on every retry and looped forever.
    Pending { fetch: MediaFetch, attempts: u8 },
    /// A failed download with what retry needs. Failures used to stick all
    /// session: one login-time timeout left that image broken until clicked.
    Failed {
        fetch: MediaFetch,
        error: String,
        attempts: u8,
        retry_at: std::time::Instant,
    },
}

impl MediaEntry {
    pub fn error(&self) -> Option<&str> {
        match self {
            MediaEntry::Failed { error, .. } => Some(error),
            MediaEntry::Pending { .. } => None,
        }
    }
}

/// How many times a download is retried before the failure sticks.
const MAX_ATTEMPTS: u8 = 3;
/// Backoff before the first retry; doubles each attempt.
const RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// An uploaded animation: one texture per frame, plus when each is shown.
pub struct Animation {
    frames: Vec<egui::TextureHandle>,
    /// Cumulative end time of each frame, so picking one is a binary search.
    ends_ms: Vec<u32>,
    total_ms: u32,
}

impl Animation {
    /// The frame showing at `now_ms` into the loop.
    fn frame_at(&self, now_ms: u32) -> &egui::TextureHandle {
        if self.total_ms == 0 {
            return &self.frames[0];
        }
        let t = now_ms % self.total_ms;
        let idx = self.ends_ms.partition_point(|end| *end <= t);
        &self.frames[idx.min(self.frames.len() - 1)]
    }
}

enum Media {
    Still(egui::TextureHandle),
    Animated(Animation),
}

pub struct MediaCache {
    /// Decoded, uploaded textures — LRU so memory stays bounded.
    ready: lru::LruCache<String, Media>,
    /// Keys that are downloading or have failed. Small and short-lived.
    states: HashMap<String, MediaEntry>,
    /// Keys queued for download (drained by `take_queued`).
    queue: VecDeque<MediaFetch>,
    /// Wall-clock milliseconds, refreshed once a frame by [`Self::tick`].
    /// Held here so every `texture_for` call picks the same animation frame
    /// without each call site having to thread a clock through.
    now_ms: u32,
    /// Whether any animation was handed out since the last tick, so the UI
    /// knows to keep painting.
    animating: bool,
}

impl MediaCache {
    pub fn new() -> Self {
        Self {
            ready: lru::LruCache::new(
                NonZeroUsize::new(TEXTURE_CACHE_CAP).expect("cap is non-zero"),
            ),
            states: HashMap::new(),
            queue: VecDeque::new(),
            now_ms: 0,
            animating: false,
        }
    }

    /// Texture for a plain mxc URI under a namespaced `kind` (`avatar`,
    /// `emoji`, …), queuing a download if unseen.
    pub fn texture_for(
        &mut self,
        kind: &str,
        mxc: &str,
        thumb: Option<(u32, u32)>,
    ) -> Option<egui::TextureHandle> {
        let uri: &matrix_sdk::ruma::MxcUri = mxc.into();
        self.texture_for_source(
            &format!("{kind}:{mxc}"),
            matrix_sdk::ruma::events::room::MediaSource::Plain(uri.to_owned()),
            thumb,
        )
    }

    /// Texture for a Matrix media source, preserving encryption metadata.
    pub fn texture_for_source(
        &mut self,
        cache_key: &str,
        source: matrix_sdk::ruma::events::room::MediaSource,
        thumb: Option<(u32, u32)>,
    ) -> Option<egui::TextureHandle> {
        // `get` bumps recency: whatever is on screen stays resident.
        let now = self.now_ms;
        let mut animating = false;
        let hit = self.ready.get(cache_key).map(|m| match m {
            Media::Still(h) => h.clone(),
            Media::Animated(a) => {
                animating = true;
                a.frame_at(now).clone()
            }
        });
        self.animating |= animating;
        if let Some(handle) = hit {
            return Some(handle);
        }
        if self.states.contains_key(cache_key) {
            return None;
        }
        let (w, h) = thumb.unwrap_or((320, 320));
        self.enqueue(MediaFetch {
            mxc: cache_key.to_owned(),
            source,
            thumb_width: w,
            thumb_height: h,
            thumbnail: true,
        });
        None
    }

    /// Original-resolution texture, queued separately from its thumbnail.
    pub fn original_for_source(
        &mut self,
        cache_key: &str,
        source: matrix_sdk::ruma::events::room::MediaSource,
    ) -> Option<egui::TextureHandle> {
        let now = self.now_ms;
        let mut animating = false;
        let hit = self.ready.get(cache_key).map(|m| match m {
            Media::Still(h) => h.clone(),
            Media::Animated(a) => {
                animating = true;
                a.frame_at(now).clone()
            }
        });
        self.animating |= animating;
        if let Some(handle) = hit {
            return Some(handle);
        }
        if self.states.contains_key(cache_key) {
            return None;
        }
        self.enqueue(MediaFetch {
            mxc: cache_key.to_owned(),
            source,
            thumb_width: 0,
            thumb_height: 0,
            thumbnail: false,
        });
        None
    }

    fn enqueue(&mut self, fetch: MediaFetch) {
        self.enqueue_with(fetch, 0);
    }

    fn enqueue_with(&mut self, fetch: MediaFetch, attempts: u8) {
        self.states.insert(
            fetch.mxc.clone(),
            MediaEntry::Pending {
                fetch: fetch.clone(),
                attempts,
            },
        );
        self.queue.push_back(fetch);
    }

    /// Retry a failed entry now, at the user's request.
    pub fn retry(&mut self, key: &str) {
        if let Some(MediaEntry::Failed { fetch, .. }) = self.states.get(key) {
            // A deliberate click resets the tally: the user may know
            // something changed (they reconnected, the server came back).
            let fetch = fetch.clone();
            self.enqueue_with(fetch, 0);
        }
    }

    /// Re-queue failures whose backoff has elapsed. Called every frame.
    ///
    /// Bounded by `MAX_ATTEMPTS` so a genuinely broken image (a deleted mxc,
    /// an unsupported format) stops costing requests instead of retrying for
    /// the life of the process.
    pub fn retry_due(&mut self) {
        let now = std::time::Instant::now();
        let due: Vec<String> = self
            .states
            .iter()
            .filter_map(|(k, e)| match e {
                MediaEntry::Failed {
                    attempts, retry_at, ..
                } if *attempts < MAX_ATTEMPTS && *retry_at <= now => Some(k.clone()),
                _ => None,
            })
            .collect();
        for key in due {
            let Some(MediaEntry::Failed {
                fetch, attempts, ..
            }) = self.states.get(&key)
            else {
                continue;
            };
            let (fetch, attempts) = (fetch.clone(), *attempts);
            self.enqueue_with(fetch, attempts);
        }
    }

    /// Re-queue everything still `Pending` — used when the download worker is
    /// (re)started, since a dead worker's requests are gone but their entries
    /// remain, and `texture_for` would never re-queue them.
    pub fn requeue_pending(&mut self) {
        let pending: Vec<MediaFetch> = self
            .states
            .values()
            .filter_map(|e| match e {
                MediaEntry::Pending { fetch, .. } => Some(fetch.clone()),
                _ => None,
            })
            .collect();
        for fetch in pending {
            self.queue.push_back(fetch);
        }
    }

    pub fn take_queued(&mut self) -> Vec<MediaFetch> {
        self.queue.drain(..).collect()
    }

    /// Put a queued fetch back at the front — used when there is no worker to
    /// hand it to yet, so nothing is silently dropped.
    pub fn unqueue(&mut self, fetches: Vec<MediaFetch>) {
        for fetch in fetches.into_iter().rev() {
            self.queue.push_front(fetch);
        }
    }

    /// Upload a decoded image. Called each frame from `poll_media`.
    pub fn ingest(&mut self, ctx: &egui::Context, msg: MediaBytes) {
        // Read the tally *before* dropping the entry: removing first meant
        // the error branch below always saw zero, so attempts never grew and
        // a broken image retried forever.
        let previous = match self.states.get(&msg.mxc) {
            Some(MediaEntry::Failed { attempts, .. })
            | Some(MediaEntry::Pending { attempts, .. }) => *attempts,
            None => 0,
        };
        self.states.remove(&msg.mxc);
        match msg.image {
            Ok(Decoded::Still(img)) => {
                let handle = ctx.load_texture(&msg.mxc, img, egui::TextureOptions::LINEAR);
                self.ready.put(msg.mxc, Media::Still(handle));
            }
            Ok(Decoded::Frames(frames)) => {
                let mut handles = Vec::with_capacity(frames.len());
                let mut ends = Vec::with_capacity(frames.len());
                let mut total = 0u32;
                for (i, (img, delay)) in frames.into_iter().enumerate() {
                    handles.push(ctx.load_texture(
                        format!("{}#{i}", msg.mxc),
                        img,
                        egui::TextureOptions::LINEAR,
                    ));
                    // A zero delay means "as fast as possible"; browsers clamp
                    // these to 100ms and so do we, or the GIF is a strobe.
                    total += delay.clamp(20, 10_000);
                    ends.push(total);
                }
                if handles.is_empty() {
                    return;
                }
                self.ready.put(
                    msg.mxc,
                    Media::Animated(Animation {
                        frames: handles,
                        ends_ms: ends,
                        total_ms: total,
                    }),
                );
            }
            Err(error) => {
                // Keep the attempt count across retries so backoff grows and
                // a hopeless image eventually stops being retried.
                let attempts = previous.saturating_add(1);
                let Some(fetch) = msg.fetch else {
                    return;
                };
                self.states.insert(
                    msg.mxc,
                    MediaEntry::Failed {
                        fetch,
                        error,
                        attempts,
                        retry_at: std::time::Instant::now()
                            + RETRY_BACKOFF * 2u32.pow(attempts.min(4) as u32 - 1),
                    },
                );
            }
        }
    }

    pub fn status(&self, key: &str) -> Option<&MediaEntry> {
        self.states.get(key)
    }

    /// Resident texture count (for the status bar / tests).
    pub fn resident(&self) -> usize {
        self.ready.len()
    }

    /// Advance the animation clock. Returns whether anything on screen is
    /// animating, so the caller can keep requesting frames.
    pub fn tick(&mut self, now_ms: u32) -> bool {
        self.now_ms = now_ms;
        std::mem::take(&mut self.animating)
    }
}

impl Default for MediaCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode bytes to an egui image, downscaling to fit `max_pixels`.
///
/// Returning `None` here means "genuinely not an image" (an HTML error page,
/// a truncated file). Size is never a reason to fail: an over-budget image is
/// scaled down, because rejecting it showed the user "undecodable image" for
/// every photo in an encrypted room.
/// Frames beyond which a GIF is treated as a still.
///
/// Every frame is its own GPU texture, so a 500-frame GIF would evict most
/// of the cache on its own. Long ones show their first frame instead.
const MAX_GIF_FRAMES: usize = 120;

/// Decode bytes to a still or, for an animated GIF, to its frames.
pub fn decode_media(bytes: &[u8], max_pixels: u64) -> Option<Decoded> {
    // GIF magic. Only GIFs animate here: the `image` crate does not decode
    // animated WebP, and APNG needs a separate decoder.
    if bytes.starts_with(b"GIF8") {
        if let Some(frames) = decode_gif(bytes, max_pixels) {
            return Some(frames);
        }
    }
    decode_image(bytes, max_pixels).map(Decoded::Still)
}

/// Decode an animated GIF into frames and their delays.
fn decode_gif(bytes: &[u8], max_pixels: u64) -> Option<Decoded> {
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let mut out = Vec::new();
    for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
        let Ok(frame) = frame else { break };
        let (num, den) = frame.delay().numer_denom_ms();
        let delay = if den == 0 { 100 } else { num / den.max(1) };
        let buffer = frame.into_buffer();
        let (w, h) = (buffer.width(), buffer.height());
        if w == 0 || h == 0 {
            continue;
        }
        // Frames share the canvas size, so one budget check covers them all.
        if u64::from(w) * u64::from(h) > max_pixels {
            return None;
        }
        out.push((
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], buffer.as_raw()),
            delay,
        ));
    }
    match out.len() {
        0 => None,
        // A one-frame GIF is just an image; skip the animation machinery.
        1 => Some(Decoded::Still(out.pop()?.0)),
        _ => Some(Decoded::Frames(out)),
    }
}

pub fn decode_image(bytes: &[u8], max_pixels: u64) -> Option<egui::ColorImage> {
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let img = reader.decode().ok()?;

    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let pixels = u64::from(w) * u64::from(h);
    let img = if pixels > max_pixels && max_pixels > 0 {
        // Preserve aspect ratio: scale both axes by sqrt(budget / pixels).
        let scale = (max_pixels as f64 / pixels as f64).sqrt();
        let nw = ((w as f64 * scale).floor() as u32).max(1);
        let nh = ((h as f64 * scale).floor() as u32).max(1);
        img.thumbnail(nw, nh)
    } else if max_pixels == 0 {
        // Zero budget means "no image may be decoded" — only used by tests.
        return None;
    } else {
        img
    };

    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        rgba.as_raw(),
    ))
}

pub async fn fetch_mxc(
    client: &matrix_sdk::Client,
    source: matrix_sdk::ruma::events::room::MediaSource,
) -> Result<Vec<u8>, String> {
    let req = matrix_sdk::media::MediaRequestParameters {
        source,
        format: matrix_sdk::media::MediaFormat::File,
    };
    client
        .media()
        .get_media_content(&req, true)
        .await
        .map_err(|e| format!("media: {e}"))
}

/// Thumbnail variant (avatars, timeline images).
///
/// Request a bounded thumbnail first and fall back to the original file.
/// Encrypted media uses the original because homeservers cannot thumbnail it.
pub async fn fetch_mxc_thumb(
    client: &matrix_sdk::Client,
    source: matrix_sdk::ruma::events::room::MediaSource,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    match source.clone() {
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(_) => {
            fetch_mxc(client, source).await
        }
        matrix_sdk::ruma::events::room::MediaSource::Plain(_) => {
            use matrix_sdk::ruma::UInt;
            let req = matrix_sdk::media::MediaRequestParameters {
                source: source.clone(),
                format: matrix_sdk::media::MediaFormat::Thumbnail(
                    matrix_sdk::media::MediaThumbnailSettings {
                        method:
                            matrix_sdk::ruma::api::client::media::get_content_thumbnail::v3::Method::Scale,
                        width: UInt::new(width.into()).unwrap_or(UInt::MAX),
                        height: UInt::new(height.into()).unwrap_or(UInt::MAX),
                        animated: false,
                    },
                ),
            };
            match client.media().get_media_content(&req, true).await {
                Ok(bytes) if !bytes.is_empty() => Ok(bytes),
                _ => fetch_mxc(client, source).await,
            }
        }
    }
}

/// Download + decode one request, wholly off the UI thread.
///
/// The decode runs on `spawn_blocking` so it never stalls the runtime's async
/// workers, and the whole fetch is bounded by [`FETCH_TIMEOUT`] so one dead
/// connection cannot hold a concurrency slot forever.
pub async fn fetch_and_decode(client: &matrix_sdk::Client, fetch: MediaFetch) -> MediaBytes {
    let max_pixels = if fetch.thumbnail {
        THUMB_MAX_PIXELS
    } else {
        ORIGINAL_MAX_PIXELS
    };
    let key = fetch.mxc.clone();
    let retry = fetch.clone();
    let fetched = tokio::time::timeout(FETCH_TIMEOUT, async {
        if fetch.thumbnail {
            fetch_mxc_thumb(client, fetch.source, fetch.thumb_width, fetch.thumb_height).await
        } else {
            fetch_mxc(client, fetch.source).await
        }
    })
    .await;

    let bytes = match fetched {
        Err(_) => {
            return MediaBytes {
                mxc: key,
                image: Err(format!("timed out after {}s", FETCH_TIMEOUT.as_secs())),
                fetch: Some(retry),
            }
        }
        Ok(Err(e)) => {
            return MediaBytes {
                mxc: key,
                image: Err(e),
                fetch: Some(retry),
            }
        }
        Ok(Ok(b)) => b,
    };

    let len = bytes.len();
    let decoded = tokio::task::spawn_blocking(move || decode_media(&bytes, max_pixels))
        .await
        .ok()
        .flatten();
    MediaBytes {
        mxc: key,
        fetch: Some(retry),
        image: decoded.ok_or_else(|| {
            // Include the byte count: distinguishes "server sent 200 bytes of
            // JSON error" from "real image the decoder can't read".
            format!("undecodable image ({len} bytes, unsupported format)")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_1x1() -> Vec<u8> {
        vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, b'I', b'D', b'A', b'T', 0x08,
            0xD7, 0x63, 0xF8, 0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xDC, 0xCC, 0x59,
            0xE7, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    fn fail(cache: &mut MediaCache, key: &str) {
        let ctx = egui::Context::default();
        let fetch = match cache.status(key) {
            Some(MediaEntry::Pending { fetch, .. }) => fetch.clone(),
            _ => panic!("expected {key} to be pending"),
        };
        cache.ingest(
            &ctx,
            MediaBytes {
                mxc: key.to_owned(),
                image: Err("timed out".into()),
                fetch: Some(fetch),
            },
        );
    }

    #[test]
    fn a_failed_download_retries_itself() {
        // The bug: one timeout in the burst of avatar fetches at login left
        // that image broken for the whole session, so images could be
        // missing on one launch and fine on the next.
        let mut cache = MediaCache::new();
        cache.texture_for("avatar", "mxc://hs/a", None);
        let key = "avatar:mxc://hs/a";
        cache.take_queued();
        fail(&mut cache, key);
        assert!(cache.status(key).unwrap().error().is_some());

        cache.retry_due();
        assert!(cache.take_queued().is_empty(), "must wait for the backoff");

        if let Some(MediaEntry::Failed { retry_at, .. }) = cache.states.get_mut(key) {
            *retry_at = std::time::Instant::now();
        }
        cache.retry_due();
        assert_eq!(cache.take_queued().len(), 1, "should retry on its own");
    }

    #[test]
    fn retries_give_up_eventually() {
        let mut cache = MediaCache::new();
        cache.texture_for("avatar", "mxc://hs/gone", None);
        let key = "avatar:mxc://hs/gone";
        assert_eq!(cache.take_queued().len(), 1, "first attempt");

        let mut retries = 0;
        for _ in 0..10 {
            fail(&mut cache, key);
            // Pretend the backoff elapsed.
            if let Some(MediaEntry::Failed { retry_at, .. }) = cache.states.get_mut(key) {
                *retry_at = std::time::Instant::now();
            }
            cache.retry_due();
            let queued = cache.take_queued().len();
            if queued == 0 {
                break;
            }
            retries += queued;
        }
        assert_eq!(
            retries,
            (MAX_ATTEMPTS - 1) as usize,
            "expected {} retries after the first attempt",
            MAX_ATTEMPTS - 1
        );
        assert!(
            cache.status(key).unwrap().error().is_some(),
            "entry should be left failed, not pending"
        );
    }

    #[test]
    fn queues_once_then_pending() {
        let mut c = MediaCache::new();
        assert!(c.texture_for("avatar", "mxc://hs/a", None).is_none());
        assert!(c.texture_for("avatar", "mxc://hs/a", None).is_none());
        assert_eq!(c.take_queued().len(), 1);
    }

    #[test]
    fn kind_namespaces_the_cache_key() {
        let mut c = MediaCache::new();
        c.texture_for("avatar", "mxc://hs/a", Some((32, 32)));
        c.texture_for("emoji", "mxc://hs/a", Some((24, 24)));
        let queued = c.take_queued();
        assert_eq!(queued.len(), 2, "same mxc at two sizes must not collide");
        assert_eq!(queued[0].mxc, "avatar:mxc://hs/a");
        assert_eq!(queued[1].mxc, "emoji:mxc://hs/a");
    }

    #[test]
    fn oversize_is_downscaled_not_rejected() {
        use image::ImageEncoder;
        let raw = vec![255u8; 64 * 64 * 4];
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&raw, 64, 64, image::ExtendedColorType::Rgba8)
            .unwrap();

        let decoded = decode_image(&png, 256).expect("over-budget image must still decode");
        let [w, h] = decoded.size;
        assert!(
            (w * h) as u64 <= 256,
            "expected downscale to fit budget, got {w}x{h}"
        );
        assert!(w > 0 && h > 0);
    }

    #[test]
    fn rejects_non_images() {
        assert!(decode_image(b"{\"errcode\":\"M_NOT_FOUND\"}", THUMB_MAX_PIXELS).is_none());
    }

    #[test]
    fn decodes_webp() {
        use image::ImageEncoder;

        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();

        let decoded = decode_image(&webp, THUMB_MAX_PIXELS).expect("WebP should decode");
        assert_eq!(decoded.size, [1, 1]);
    }

    #[test]
    fn decodes_png_within_budget() {
        let decoded = decode_image(&png_1x1(), THUMB_MAX_PIXELS).expect("PNG should decode");
        assert_eq!(decoded.size, [1, 1]);
    }

    #[test]
    fn original_is_queued_separately_from_thumbnail() {
        let mut cache = MediaCache::new();
        let source = matrix_sdk::ruma::events::room::MediaSource::Plain("mxc://hs/image".into());
        cache.texture_for_source("thumbnail:mxc://hs/image", source.clone(), Some((320, 320)));
        cache.original_for_source("original:mxc://hs/image", source);

        let queued = cache.take_queued();
        assert_eq!(queued.len(), 2);
        assert!(queued[0].thumbnail);
        assert!(!queued[1].thumbnail);
    }

    #[test]
    fn unqueue_restores_dropped_fetches() {
        let mut cache = MediaCache::new();
        cache.texture_for("avatar", "mxc://hs/a", None);
        let taken = cache.take_queued();
        assert_eq!(taken.len(), 1);
        cache.unqueue(taken);
        assert_eq!(cache.take_queued().len(), 1, "fetch must not be lost");
    }
}
