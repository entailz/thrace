/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! In-app video playback.
//!
//! egui has no video support, and there is no pure-Rust decoder that handles
//! what people actually post (H.264/AAC in MP4). So we drive `ffmpeg`, which
//! is already on every desktop that plays video: one process decodes frames
//! to raw RGBA on stdout, which a reader thread turns into `ColorImage`s, and
//! a second process plays the audio.
//!
//! Splitting audio and video across two processes avoids writing an A/V sync
//! loop. They are started together and both run off the same file, so a chat
//! clip stays in step; it is not a general-purpose player and does not try to
//! be. If `ffmpeg` is missing, [`VideoPlayer::start`] fails and the caller
//! offers the file to the system player instead.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Longest edge of the decoded frames. Chat video is watched in a window, and
/// decoding 4K frames to throw most of the pixels away is wasted work.
const MAX_EDGE: u32 = 720;

/// Frames buffered ahead of the UI. Enough to absorb a slow frame without
/// letting the decoder run away with memory.
const BUFFER_FRAMES: usize = 8;

pub struct VideoPlayer {
    frames: std::sync::mpsc::Receiver<(egui::ColorImage, f64)>,
    /// Currently displayed frame and its presentation time.
    current: Option<(egui::TextureHandle, f64)>,
    /// Next frame, held until its presentation time arrives.
    pending: Option<(egui::ColorImage, f64)>,
    started: f64,
    video: Child,
    audio: Option<Child>,
    stop: Arc<AtomicBool>,
    pub finished: bool,
    /// Paused playback holds its position here; `started` is rebased on
    /// resume so the clip carries on rather than jumping.
    paused_at: Option<f64>,
    /// Kept so audio can be restarted at an offset when resuming.
    path: std::path::PathBuf,
}

impl VideoPlayer {
    /// Start decoding `path`. Errors when ffmpeg is unavailable or refuses
    /// the file.
    ///
    /// `display_size` is the sender's own width and height from the Matrix
    /// event. Prefer it: phone video is usually stored landscape with a 90°
    /// rotation flag, so `ffprobe`'s stream dimensions are the *stored* ones
    /// while ffmpeg auto-rotates on decode. Scaling to the stored size
    /// squashes a portrait clip into a landscape frame.
    pub fn start(
        path: &std::path::Path,
        now: f64,
        display_size: Option<(u32, u32)>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (width, height) = display_size
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or_else(|| probe_size(path))
            .unwrap_or((640, 360));
        let (w, h) = fit(width, height);

        let mut video = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-i"])
            .arg(path)
            .args([
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-vf",
                &format!("scale={w}:{h}"),
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;

        let mut stdout = video.stdout.take().ok_or("ffmpeg gave no output")?;
        let (tx, frames) = std::sync::mpsc::sync_channel(BUFFER_FRAMES);
        let fps = probe_fps(path).unwrap_or(30.0).clamp(1.0, 120.0);
        let reader_stop = stop.clone();
        std::thread::spawn(move || {
            let frame_bytes = (w as usize) * (h as usize) * 4;
            let mut buf = vec![0u8; frame_bytes];
            let mut index = 0u64;
            while !reader_stop.load(Ordering::Relaxed) {
                if stdout.read_exact(&mut buf).is_err() {
                    break; // clip ended, or ffmpeg died
                }
                let image =
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &buf);
                // `send` blocks once the buffer is full, which is what keeps
                // ffmpeg from decoding the whole file into memory.
                if tx.send((image, index as f64 / fps)).is_err() {
                    break;
                }
                index += 1;
            }
        });

        // Audio as its own process: no sync loop to write, and it exits by
        // itself at the end of the clip.
        let audio = spawn_audio(path, 0.0);

        Ok(Self {
            frames,
            current: None,
            pending: None,
            started: now,
            video,
            audio,
            stop,
            finished: false,
            paused_at: None,
            path: path.to_path_buf(),
        })
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// Pause or resume.
    ///
    /// Video pauses by simply not consuming frames: the reader writes into a
    /// bounded channel, so ffmpeg blocks on its own once it fills. Audio is a
    /// separate process with no pause channel, so it is killed and restarted
    /// at the offset — which also keeps it in step after a pause.
    pub fn toggle_pause(&mut self, now: f64) {
        match self.paused_at {
            Some(position) => {
                self.started = now - position;
                self.paused_at = None;
                self.audio = spawn_audio(&self.path, position);
            }
            None => {
                self.paused_at = Some(now - self.started);
                if let Some(audio) = &mut self.audio {
                    let _ = audio.kill();
                }
                self.audio = None;
            }
        }
    }

    /// Advance to the frame due at `now`, returning what to draw.
    pub fn frame(&mut self, ctx: &egui::Context, now: f64) -> Option<egui::TextureHandle> {
        // Paused: hold the frame. Not draining the channel is what makes
        // ffmpeg stop too, once its buffer fills.
        if let Some(_position) = self.paused_at {
            return self.current.as_ref().map(|(h, _)| h.clone());
        }
        let elapsed = now - self.started;
        loop {
            // Hold the next frame until its time comes, so playback runs at
            // the clip's rate rather than as fast as it decodes.
            if self.pending.is_none() {
                match self.frames.try_recv() {
                    Ok(next) => self.pending = Some(next),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.finished = true;
                        break;
                    }
                }
            }
            let Some((_, pts)) = &self.pending else { break };
            if *pts > elapsed {
                break;
            }
            let (image, pts) = self.pending.take().expect("checked above");
            let handle = ctx.load_texture("video-frame", image, egui::TextureOptions::LINEAR);
            self.current = Some((handle, pts));
        }
        self.current.as_ref().map(|(h, _)| h.clone())
    }

    /// Seconds played so far.
    pub fn position(&self, now: f64) -> f64 {
        self.paused_at.unwrap_or(now - self.started).max(0.0)
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Both children outlive the struct unless killed, and ffplay would
        // keep playing audio over a closed window.
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.video.kill();
        if let Some(audio) = &mut self.audio {
            let _ = audio.kill();
        }
    }
}

fn spawn_audio(path: &std::path::Path, offset: f64) -> Option<Child> {
    let mut cmd = Command::new("ffplay");
    cmd.args(["-loglevel", "error", "-nodisp", "-autoexit", "-vn"]);
    if offset > 0.05 {
        cmd.args(["-ss", &format!("{offset:.3}")]);
    }
    cmd.arg("-i")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()
}

/// Scale `w`x`h` down to [`MAX_EDGE`], keeping the aspect and even numbers
/// (many encoders reject odd dimensions).
fn fit(w: u32, h: u32) -> (u32, u32) {
    let longest = w.max(h);
    if longest <= MAX_EDGE || longest == 0 {
        return (w.max(2) & !1, h.max(2) & !1);
    }
    let scale = MAX_EDGE as f32 / longest as f32;
    let nw = ((w as f32 * scale) as u32).max(2) & !1;
    let nh = ((h as f32 * scale) as u32).max(2) & !1;
    (nw, nh)
}

/// Ask ffprobe for the video's *display* dimensions.
///
/// Swaps them when the stream carries a quarter-turn rotation, since ffmpeg
/// auto-rotates on decode and the stored size is then the wrong way round.
fn probe_size(path: &std::path::Path) -> Option<(u32, u32)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().split(',');
    let w: u32 = parts.next()?.trim().parse().ok()?;
    let h: u32 = parts.next()?.trim().parse().ok()?;
    Some(if quarter_turned(path) { (h, w) } else { (w, h) })
}

fn quarter_turned(path: &std::path::Path) -> bool {
    let Ok(out) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream_side_data=rotation",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(',')
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .any(|deg| {
            let turn = deg.rem_euclid(180.0);
            (turn - 90.0).abs() < 1.0
        })
}

/// Ask ffprobe for the frame rate, which arrives as a rational like "30000/1001".
fn probe_fps(path: &std::path::Path) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=avg_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (num, den) = text.trim().split_once('/')?;
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    (den > 0.0 && num > 0.0).then(|| num / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_even_dimensions() {
        assert_eq!(fit(640, 360), (640, 360));
        let (w, h) = fit(1920, 1080);
        assert_eq!(w, MAX_EDGE);
        assert!((h as f32 - MAX_EDGE as f32 * 1080.0 / 1920.0).abs() < 2.0);
        let (w, h) = fit(1921, 1081);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        let (w, h) = fit(1080, 1920);
        assert_eq!(h, MAX_EDGE);
        assert!(w < h);
    }

    /// Build a tiny clip for the probe tests, or skip when ffmpeg is absent.
    fn sample_clip(name: &str, size: &str) -> Option<std::path::PathBuf> {
        let path = std::env::temp_dir().join(name);
        let ok = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size={size}:duration=1:rate=10"),
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&path)
            .status()
            .ok()?
            .success();
        ok.then_some(path)
    }

    #[test]
    fn probe_reports_portrait_as_portrait() {
        let Some(clip) = sample_clip("thrace-probe-portrait.mp4", "240x320") else {
            return; // no ffmpeg here
        };
        assert_eq!(
            probe_size(&clip),
            Some((240, 320)),
            "a portrait clip must not come back transposed"
        );
        let (w, h) = fit(240, 320);
        assert!(h > w, "portrait must stay portrait, got {w}x{h}");
        let _ = std::fs::remove_file(clip);
    }

    #[test]
    fn scaling_preserves_aspect_for_tall_video() {
        // The reported case: 574x1280 was being forced into a landscape
        // buffer, which stretched the picture sideways.
        let (w, h) = fit(574, 1280);
        assert!(h > w, "expected portrait, got {w}x{h}");
        let source = 574.0 / 1280.0;
        let scaled = w as f32 / h as f32;
        assert!(
            (source - scaled).abs() < 0.02,
            "aspect drifted: {source} vs {scaled}"
        );
    }

    #[test]
    fn degenerate_sizes_do_not_produce_zero() {
        for (w, h) in [(0, 0), (1, 1), (3, 1)] {
            let (fw, fh) = fit(w, h);
            assert!(fw >= 2 && fh >= 2, "{w}x{h} produced {fw}x{fh}");
        }
    }
}
