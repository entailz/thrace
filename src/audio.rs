/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Audio playback + waveform data for voice notes and music.
//!
//! Playback follows the video strategy: `ffplay` owns decoding, so
//! ogg/opus/mp3/wav/m4a/flac all play with no new dependencies. Pause and
//! seek kill the process and restart it at an offset; position is tracked by
//! the UI clock. Waveforms come from the event (MSC1767) when the sender
//! included one, otherwise `ffmpeg` decodes to mono PCM here and the peaks
//! are downsampled to bar heights.

use std::process::{Child, Command, Stdio};

/// Waveform bars per message, matching Element's density.
pub const WAVE_BARS: usize = 64;

/// Sample rate for waveform decoding; voice needs no more.
const WAVE_RATE: u32 = 8000;

pub struct AudioPlayer {
    child: Option<Child>,
    started: f64,
    paused_at: Option<f64>,
    path: std::path::PathBuf,
    pub duration: f64,
    pub finished: bool,
}

impl AudioPlayer {
    pub fn start(
        path: &std::path::Path,
        now: f64,
        position: f64,
        duration: f64,
    ) -> Result<Self, String> {
        let position = position.clamp(0.0, duration.max(0.0));
        let child = spawn_audio(path, position);
        if child.is_none() && ffplay_missing() {
            return Err("ffplay not found; install ffmpeg for in-app playback".into());
        }
        Ok(Self {
            child,
            started: now - position,
            paused_at: None,
            path: path.to_path_buf(),
            duration,
            finished: false,
        })
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn toggle_pause(&mut self, now: f64) {
        match self.paused_at {
            Some(position) => {
                self.started = now - position;
                self.paused_at = None;
                self.finished = false;
                self.child = spawn_audio(&self.path, position);
            }
            None => {
                self.paused_at = Some(self.position(now));
                self.kill_child();
            }
        }
    }

    pub fn seek(&mut self, now: f64, position: f64) {
        let position = position.clamp(0.0, self.duration.max(0.0));
        self.finished = false;
        match self.paused_at {
            Some(_) => self.paused_at = Some(position),
            None => {
                self.started = now - position;
                self.kill_child();
                self.child = spawn_audio(&self.path, position);
            }
        }
    }

    pub fn position(&mut self, now: f64) -> f64 {
        if let Some(position) = self.paused_at {
            return position;
        }
        let elapsed = (now - self.started).max(0.0);
        if self.duration > 0.0 && elapsed >= self.duration {
            return self.duration;
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.finished = true;
                    self.child = None;
                    return self.duration.max(elapsed);
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        elapsed
    }

    fn kill_child(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
        self.child = None;
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.kill_child();
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

fn ffplay_missing() -> bool {
    Command::new("ffplay")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
}

/// Track length in seconds via ffprobe, for events that report none.
pub fn probe_duration(path: &std::path::Path) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let seconds: f64 = text.trim().parse().ok()?;
    (seconds.is_finite() && seconds > 0.0).then_some(seconds)
}

/// Bar heights 0..=1 decoded from the file; call off the UI thread.
pub fn compute_waveform(path: &std::path::Path, bars: usize) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-ac",
            "1",
            "-ar",
            &WAVE_RATE.to_string(),
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "pipe:1",
        ])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let samples: Vec<i16> = out
        .stdout
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    (!samples.is_empty()).then(|| peaks_to_bars(&samples, bars))
}

fn peaks_to_bars(samples: &[i16], bars: usize) -> Vec<f32> {
    if bars == 0 {
        return Vec::new();
    }
    if samples.is_empty() {
        return vec![0.0; bars];
    }
    let window = (samples.len() + bars - 1) / bars;
    (0..bars)
        .map(|i| {
            let start = i * window;
            if start >= samples.len() {
                return 0.0;
            }
            let end = (start + window).min(samples.len());
            let peak = samples[start..end]
                .iter()
                .map(|s| s.unsigned_abs() as f32 / 32768.0)
                .fold(0.0, f32::max);
            peak.clamp(0.0, 1.0)
        })
        .collect()
}

pub fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_spread_evenly_across_bars() {
        let mut samples = vec![0i16; 100];
        samples[10] = 16384;
        samples[80] = -32768;
        let bars = peaks_to_bars(&samples, 10);
        assert_eq!(bars.len(), 10);
        assert!((bars[1] - 0.5).abs() < 0.01, "got {:?}", bars);
        assert_eq!(bars[8], 1.0);
        assert_eq!(bars[0], 0.0);
    }

    #[test]
    fn peaks_degrade_to_flat_for_empty_input() {
        assert_eq!(peaks_to_bars(&[], 4), vec![0.0; 4]);
        assert!(peaks_to_bars(&[1, 2, 3], 0).is_empty());
        for bar in peaks_to_bars(&[i16::MAX, i16::MIN], 4) {
            assert!((0.0..=1.0).contains(&bar), "bar out of range: {bar}");
        }
    }

    #[test]
    fn times_format_as_minutes_and_seconds() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(65.0), "1:05");
        assert_eq!(format_time(-3.0), "0:00");
    }
}
