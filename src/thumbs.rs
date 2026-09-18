//! Thumbnails for the TV's grid: a JPEG next to the video, or one frame pulled
//! out by ffmpeg when it is installed. Everything about them lives here; the
//! rest of the server only asks whether an item has one and where it is.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::library::{Item, Kind};
use crate::{FNV_OFFSET, fnv1a};

/// 1024x768 at most: sharp on a 4K set's grid, and a few tens of KB on a LAN.
/// `JPEG_TN`, the 160x160 profile, is the fallback should a renderer refuse it.
pub const PROFILE: &str = "JPEG_MED";
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;
/// Past the studio logos of a film. A clip shorter than this gets its first frame.
const SEEK_SECONDS: &str = "120";
/// A corrupt file can hang ffmpeg, and the TV has given up long before this.
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Thumbnails {
    cache: PathBuf,
    ffmpeg: bool,
    /// One ffmpeg at a time: a TV asks for a whole grid at once, and ten 4K
    /// decodes in parallel finish later than ten in a row.
    generating: Mutex<()>,
}

impl Thumbnails {
    /// Looks ffmpeg up in `PATH` once; the cache directory is created on first use.
    pub fn new(cache: PathBuf) -> Thumbnails {
        let ffmpeg = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        Thumbnails {
            cache,
            ffmpeg,
            generating: Mutex::new(()),
        }
    }

    pub fn ffmpeg_found(&self) -> bool {
        self.ffmpeg
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Whether Browse should advertise a thumbnail for `item`.
    pub fn offered(&self, item: &Item) -> bool {
        item.thumbnail.is_some() || (self.ffmpeg && item.kind == Kind::Video)
    }

    /// The JPEG for `item`, generated and cached on first request.
    pub fn fetch(&self, item: &Item) -> Option<PathBuf> {
        if let Some(sidecar) = &item.thumbnail {
            return Some(sidecar.clone());
        }
        if !self.ffmpeg || item.kind != Kind::Video {
            return None;
        }
        // The profile is in the name, so a change of size does not serve stale files.
        let target = self
            .cache
            .join(format!("{:016x}-{PROFILE}.jpg", cache_key(item)));
        let _one_at_a_time = self.generating.lock().unwrap();
        if target.is_file() {
            return Some(target);
        }
        fs::create_dir_all(&self.cache).ok()?;
        extract_frame(&item.path, &target).then_some(target)
    }
}

/// A JPEG sharing the video's stem: the poster convention Kodi, Plex and
/// Jellyfin all follow.
pub fn sidecar(video: &Path) -> Option<PathBuf> {
    for extension in ["jpg", "jpeg", "JPG", "JPEG"] {
        let candidate = video.with_extension(extension);
        if candidate != video && candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Path, size and mtime: a replaced file gets a fresh thumbnail.
fn cache_key(item: &Item) -> u64 {
    let modified = fs::metadata(&item.path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let key = fnv1a(FNV_OFFSET, item.path.to_string_lossy().as_bytes());
    let key = fnv1a(key, &item.size.to_le_bytes());
    fnv1a(key, &modified.to_le_bytes())
}

fn extract_frame(video: &Path, target: &Path) -> bool {
    for seek in [SEEK_SECONDS, "0"] {
        // Seeking past the end exits cleanly with nothing written.
        if run_ffmpeg(video, target, seek) && fs::metadata(target).is_ok_and(|m| m.len() > 0) {
            return true;
        }
        let _ = fs::remove_file(target);
    }
    false
}

fn run_ffmpeg(video: &Path, target: &Path, seek: &str) -> bool {
    let scale = format!("scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease");
    let spawned = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-ss", seek, "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-vf", &scale, "-q:v", "4"])
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            eprintln!("thumbs: ffmpeg: {err}");
            return false;
        }
    };

    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("thumbs: ffmpeg timed out on {}", video.display());
                return false;
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{Thumbnails, sidecar};
    use crate::library::{Item, Kind};
    use crate::test_support::TempDir;

    fn video(path: std::path::PathBuf) -> Item {
        Item {
            id: 1,
            parent: 0,
            title: "clip".into(),
            size: std::fs::metadata(&path).map_or(0, |m| m.len()),
            path,
            kind: Kind::Video,
            mime: "video/mp4",
            subtitle: None,
            thumbnail: None,
            children: Vec::new(),
        }
    }

    #[test]
    fn a_sidecar_shares_the_stem() {
        let tmp = TempDir::new();
        let film = tmp.file("film.mkv", 1);
        assert_eq!(sidecar(&film), None);
        let poster = tmp.file("film.jpg", 1);
        assert_eq!(sidecar(&film), Some(poster));
    }

    #[test]
    fn a_sidecar_wins_over_generation() {
        let tmp = TempDir::new();
        let thumbs = Thumbnails::new(tmp.path().join("cache"));
        let mut item = video(tmp.file("film.mkv", 1));
        item.thumbnail = Some(tmp.file("film.jpg", 1));
        assert_eq!(thumbs.fetch(&item), item.thumbnail);
    }

    /// Runs only where ffmpeg is installed: the clip is three seconds long, so
    /// this also covers the fall-back from the two-minute mark to the start.
    #[test]
    fn a_frame_is_extracted_once_and_cached() {
        let tmp = TempDir::new();
        let thumbs = Thumbnails::new(tmp.path().join("cache"));
        if !thumbs.ffmpeg_found() {
            eprintln!("no ffmpeg in PATH, skipping");
            return;
        }
        let clip = tmp.path().join("clip.mp4");
        let made = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=3:size=64x64:rate=10")
            .arg(&clip)
            .status()
            .expect("run ffmpeg");
        assert!(made.success());

        let item = video(clip);
        let first = thumbs.fetch(&item).expect("a frame");
        assert!(std::fs::metadata(&first).unwrap().len() > 0);
        assert!(first.starts_with(thumbs.cache_dir()));
        let written = std::fs::metadata(&first).unwrap().modified().unwrap();
        assert_eq!(thumbs.fetch(&item), Some(first.clone()));
        assert_eq!(
            std::fs::metadata(&first).unwrap().modified().unwrap(),
            written,
            "the second fetch should come from the cache"
        );
    }
}
