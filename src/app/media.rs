/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Fetching and caching images, video, audio and link previews.

use crate::app::text::sanitise;
use crate::app::{AudioAttachment, AudioReady, ImageAttachment, SendResult, ThraceApp};
use std::sync::Arc;

/// Parallel media downloads; fills a screen of avatars without per-image connections.
pub(in crate::app) const MEDIA_CONCURRENCY: usize = 6;

/// Texture uploads per frame; caps bursts that would stutter.
pub(in crate::app) const MEDIA_UPLOADS_PER_FRAME: usize = 8;

/// Does the clipboard hold a bitmap? Distinguishes the text half of an image
/// copy from a real text paste.
pub(in crate::app) fn clipboard_has_image() -> bool {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_image())
        .is_ok()
}

/// Encode a clipboard bitmap (RGBA8) as PNG for upload.
pub(in crate::app) fn encode_clipboard_png(
    img: &arboard::ImageData<'_>,
) -> Result<(String, Vec<u8>), String> {
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

impl ThraceApp {
    /// Download a video to a temp file so ffmpeg can read it.
    pub(in crate::app) fn fetch_video(&mut self, image: &ImageAttachment) {
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

    /// Download audio to a temp file so ffplay can read it, then measure what
    /// the event did not report (duration, waveform).
    pub(in crate::app) fn fetch_audio(&mut self, event_id: &str, audio: &AudioAttachment) {
        if self.audio_paths.contains_key(&audio.mxc)
            || !self.audio_pending.insert(event_id.to_owned())
        {
            return;
        }
        let Some(client) = self.client.clone() else {
            self.audio_pending.remove(event_id);
            return;
        };
        if self.audio_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.audio_tx = Some(tx);
            self.audio_rx = Some(rx);
        }
        let tx = self.audio_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        let event_id = event_id.to_owned();
        let source = audio.source.clone();
        let mxc = audio.mxc.clone();
        let name = audio.name.clone();
        let duration_ms = audio.duration_ms;
        let need_wave = audio.waveform.is_empty();
        self.rt.spawn(async move {
            let result = match crate::media_cache::fetch_mxc(&client, source).await {
                Ok(bytes) => {
                    let mut path = std::env::temp_dir();
                    path.push(format!("thrace-audio-{}", sanitise(&name)));
                    match std::fs::write(&path, &bytes) {
                        Ok(()) => {
                            let duration_secs = duration_ms
                                .map(|ms| ms as f64 / 1000.0)
                                .or_else(|| crate::audio::probe_duration(&path))
                                .unwrap_or(0.0);
                            let waveform = need_wave
                                .then(|| {
                                    crate::audio::compute_waveform(&path, crate::audio::WAVE_BARS)
                                })
                                .flatten();
                            Ok(AudioReady {
                                mxc,
                                path,
                                duration_secs,
                                waveform,
                            })
                        }
                        Err(e) => Err(format!("write: {e}")),
                    }
                }
                Err(e) => Err(e),
            };
            let _ = tx.send((event_id, result));
            ctx.request_repaint();
        });
    }

    pub(in crate::app) fn download_image(&mut self, image: &ImageAttachment) {
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

    /// Drop media worker channels; `ensure_media_worker` rebuilds it.
    pub(in crate::app) fn stop_media_worker(&mut self) {
        self.media_tx = None;
        self.media_rx = None;
        self.media_req_tx = None;
    }

    /// Start fetching a card, once per URL.
    pub(in crate::app) fn fetch_embed(&mut self, url: String) {
        if !self.embeds.claim(&url) {
            return;
        }
        let github = crate::embed::github_endpoint(&url).is_some();
        let rule = crate::embed::card_rule(&self.embed_rules, &url).cloned();
        if !github && rule.is_none() {
            return;
        }
        if self.embed_tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.embed_tx = Some(tx);
            self.embed_rx = Some(rx);
        }
        let tx = self.embed_tx.clone().unwrap();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let result = if github {
                crate::embed::fetch_github(&url).await.ok()
            } else {
                crate::embed::fetch(&rule.unwrap(), &url).await.ok()
            };
            let _ = tx.send((url, result));
            ctx.request_repaint();
        });
    }

    /// Thumbnail for a card. These are https URLs, not mxc, so they bypass
    /// the Matrix media cache entirely.
    pub(in crate::app) fn embed_image(
        &mut self,
        ctx: &egui::Context,
        url: &str,
    ) -> Option<egui::TextureHandle> {
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

    /// Lazily create the media channel + spawn the download worker.
    pub(in crate::app) fn ensure_media_worker(&mut self) {
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
    pub(in crate::app) fn poll_media(&mut self, ctx: &egui::Context) {
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
}
