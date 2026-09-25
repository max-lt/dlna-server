//! Audio the TV cannot decode, converted ahead of time: the video is copied,
//! the offending track becomes AC-3, and the result is a complete Matroska
//! file in the cache, served like any other. A stream made on the fly was
//! tried first: a TV probes a file by byte offsets before it plays, and only
//! a real file answers those.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::cache_key;
use crate::library::{Item, Kind, Library};

/// Codecs a run of the mill TV has no decoder for: DTS in every flavour,
/// Dolby TrueHD, and the two the web brought into MKV.
const UNSUPPORTED: &[&str] = &["dts", "truehd", "mlp", "opus", "vorbis"];
/// AC-3 tops out at 5.1 channels and 640 kb/s.
const MAX_CHANNELS: u32 = 6;
/// Words in a track title that name the codec it had before, lowercase.
const OLD_CODEC_WORDS: &[&str] = &["dts", "truehd", "atmos", "master audio", "opus", "vorbis"];
/// Statistics mkvmerge writes per track; they describe the old codec's
/// stream, bitrate and size included, and go stale once it is re-encoded.
const STALE_TAGS: &[&str] = &[
    "BPS",
    "DURATION",
    "NUMBER_OF_FRAMES",
    "NUMBER_OF_BYTES",
    "_STATISTICS_WRITING_APP",
    "_STATISTICS_WRITING_DATE_UTC",
    "_STATISTICS_TAGS",
];
pub const MIME: &str = "video/x-matroska";
pub const EXTENSION: &str = "mkv";

pub struct Transcoder {
    cache: PathBuf,
    probed: Mutex<HashMap<PathBuf, Probed>>,
    /// The conversion under way, so a shutdown can kill it.
    running: Mutex<Option<Child>>,
}

struct Probed {
    size: u64,
    convert: Vec<Track>,
    ready: Option<PathBuf>,
    /// A conversion that failed is not retried every cycle.
    given_up: bool,
}

/// One audio track to convert: its index among the audio streams, how many
/// channels it has, and its title.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Track {
    index: usize,
    channels: u32,
    title: Option<String>,
}

/// A probed audio stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Audio {
    codec: String,
    channels: u32,
    title: Option<String>,
}

impl Transcoder {
    /// `None` without both ffmpeg and ffprobe in `PATH`. The cache directory
    /// is created on first use.
    pub fn new(cache: PathBuf) -> Option<Transcoder> {
        let found = |tool: &str| {
            Command::new(tool)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        (found("ffmpeg") && found("ffprobe")).then(|| Transcoder {
            cache,
            probed: Mutex::new(HashMap::new()),
            running: Mutex::new(None),
        })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Probes every video not yet known, converts the ones that need it, one
    /// at a time, and reports each as it becomes ready. Runs on its own
    /// thread: a conversion copies the whole file.
    pub fn work(&self, lib: &Library, stop: &dyn Fn() -> bool, ready: &dyn Fn(&Item)) {
        for item in lib.items.values().filter(|i| i.kind == Kind::Video) {
            if stop() {
                return;
            }
            let Some(tracks) = self.pending(item) else {
                continue;
            };
            let target = self
                .cache
                .join(format!("{:016x}-ac3.{EXTENSION}", cache_key(item)));
            let done = target.is_file() || self.convert(item, &tracks, &target);
            let mut probed = self.probed.lock().unwrap();
            let Some(entry) = probed.get_mut(&item.path) else {
                continue;
            };
            if done {
                entry.ready = Some(target);
            } else {
                entry.given_up = true;
            }
            drop(probed);
            if done {
                ready(item);
            }
        }
    }

    /// The tracks still to convert for `item`, probing it first if need be.
    fn pending(&self, item: &Item) -> Option<Vec<Track>> {
        let mut probed = self.probed.lock().unwrap();
        if let Some(known) = probed.get(&item.path).filter(|p| p.size == item.size) {
            let pending = known.ready.is_none() && !known.given_up && !known.convert.is_empty();
            return pending.then(|| known.convert.clone());
        }
        let convert = decide(&parse_probe(&probe(&item.path)?));
        probed.insert(
            item.path.clone(),
            Probed {
                size: item.size,
                convert: convert.clone(),
                ready: None,
                given_up: false,
            },
        );
        (!convert.is_empty()).then_some(convert)
    }

    /// The converted file for `item`, once there is one.
    pub fn ready(&self, item: &Item) -> Option<PathBuf> {
        let probed = self.probed.lock().unwrap();
        let known = probed.get(&item.path).filter(|p| p.size == item.size)?;
        known.ready.clone()
    }

    /// Kills the conversion under way; `work` then reports nothing for it.
    pub fn abort(&self) {
        if let Some(mut child) = self.running.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Writes `target` next to a `.part` first, so a file is either whole or
    /// absent. False when ffmpeg failed or was aborted.
    fn convert(&self, item: &Item, tracks: &[Track], target: &Path) -> bool {
        if fs::create_dir_all(&self.cache).is_err() {
            return false;
        }
        let part = target.with_extension("part");
        let mut command = Command::new("ffmpeg");
        command
            .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
            .arg(&item.path)
            .args(["-map", "0:v", "-map", "0:a", "-c:v", "copy", "-c:a", "copy"]);
        // Subtitles ride along from Matroska; MP4's mov_text cannot be copied
        // into it and would sink the whole run.
        if is_matroska(&item.path) {
            command.args(["-map", "0:s?", "-c:s", "copy"]);
        }
        for track in tracks {
            let bitrate = if track.channels <= 2 { "256k" } else { "640k" };
            command
                .arg(format!("-c:a:{}", track.index))
                .arg("ac3")
                .arg(format!("-b:a:{}", track.index))
                .arg(bitrate);
            if track.channels > MAX_CHANNELS {
                command
                    .arg(format!("-ac:a:{}", track.index))
                    .arg(MAX_CHANNELS.to_string());
            }
            // An empty value removes the tag.
            for tag in STALE_TAGS {
                for key in [tag.to_string(), format!("{tag}-eng")] {
                    command
                        .arg(format!("-metadata:s:a:{}", track.index))
                        .arg(format!("{key}="));
                }
            }
            if let Some(title) = new_title(track) {
                command
                    .arg(format!("-metadata:s:a:{}", track.index))
                    .arg(format!("title={title}"));
            }
        }
        command
            .args(["-f", "matroska"])
            .arg(&part)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                eprintln!("transcode: ffmpeg: {err}");
                return false;
            }
        };
        *self.running.lock().unwrap() = Some(child);

        // Polled rather than waited on, so `abort` can get at the child.
        let succeeded = loop {
            let mut running = self.running.lock().unwrap();
            let Some(child) = running.as_mut() else {
                break false;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    running.take();
                    break status.success();
                }
                Ok(None) => {}
                Err(_) => {
                    running.take();
                    break false;
                }
            }
            drop(running);
            thread::sleep(Duration::from_millis(200));
        };
        if succeeded && fs::rename(&part, target).is_ok() {
            return true;
        }
        let _ = fs::remove_file(&part);
        if succeeded {
            eprintln!(
                "transcode: could not move the result into place for {}",
                item.path.display()
            );
        } else {
            eprintln!("transcode: ffmpeg failed on {}", item.path.display());
        }
        false
    }
}

fn is_matroska(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mkv") || ext.eq_ignore_ascii_case("webm"))
}

fn probe(path: &Path) -> Option<String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name,channels:stream_tags=title",
            "-of",
            "flat",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// ffprobe's flat output, `streams.stream.N.codec_name="dts"` lines, into the
/// audio tracks in order.
fn parse_probe(flat: &str) -> Vec<Audio> {
    let mut streams: HashMap<usize, (String, Audio)> = HashMap::new();
    for line in flat.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        let Some(rest) = key.strip_prefix("streams.stream.") else {
            continue;
        };
        let Some((index, field)) = rest.split_once('.') else {
            continue;
        };
        let Ok(index) = index.parse::<usize>() else {
            continue;
        };
        let stream = streams.entry(index).or_default();
        match field {
            "codec_type" => stream.0 = value.to_string(),
            "codec_name" => stream.1.codec = value.to_string(),
            "channels" => stream.1.channels = value.parse().unwrap_or(0),
            "tags.title" => stream.1.title = Some(value.to_string()),
            _ => {}
        }
    }
    let mut ordered: Vec<_> = streams.into_iter().collect();
    ordered.sort_by_key(|(index, _)| *index);
    ordered
        .into_iter()
        .filter(|(_, (kind, _))| kind == "audio")
        .map(|(_, (_, audio))| audio)
        .collect()
}

fn decide(audio: &[Audio]) -> Vec<Track> {
    audio
        .iter()
        .enumerate()
        .filter(|(_, audio)| UNSUPPORTED.contains(&audio.codec.as_str()))
        .map(|(index, audio)| Track {
            index,
            channels: audio.channels,
            title: audio.title.clone(),
        })
        .collect()
}

/// `AC-3 5.1` in place of a title that names the old codec, so the TV's track
/// menu does not show DTS for an AC-3 track. `None` keeps the title as it is.
fn new_title(track: &Track) -> Option<String> {
    let title = track.title.as_deref()?.to_lowercase();
    if !OLD_CODEC_WORDS.iter().any(|word| title.contains(word)) {
        return None;
    }
    let layout = match track.channels.min(MAX_CHANNELS) {
        1 => "1.0".to_string(),
        2 => "2.0".to_string(),
        6 => "5.1".to_string(),
        n => format!("{n}ch"),
    };
    Some(format!("AC-3 {layout}"))
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::Mutex;

    use super::{Audio, Track, Transcoder, decide, new_title, parse_probe};
    use crate::library::Library;
    use crate::test_support::{TempDir, dts_clip};

    const WAKE_IN_FRIGHT: &str = r#"streams.stream.0.codec_name="h264"
streams.stream.0.codec_type="video"
streams.stream.1.codec_name="dts"
streams.stream.1.codec_type="audio"
streams.stream.1.channels=2
streams.stream.1.tags.title="DTS 2.0"
streams.stream.2.codec_name="hdmv_pgs_subtitle"
streams.stream.2.codec_type="subtitle"
format.duration="6530.483000"
"#;

    #[test]
    fn a_probe_yields_the_audio_tracks_in_order() {
        assert_eq!(
            parse_probe(WAKE_IN_FRIGHT),
            vec![Audio {
                codec: "dts".to_string(),
                channels: 2,
                title: Some("DTS 2.0".to_string()),
            }]
        );
    }

    #[test]
    fn only_the_tracks_the_tv_cannot_play_are_converted() {
        let tracks = |list: &[(&str, u32)]| -> Vec<Audio> {
            list.iter()
                .map(|(c, n)| Audio {
                    codec: c.to_string(),
                    channels: *n,
                    title: None,
                })
                .collect()
        };
        assert_eq!(decide(&tracks(&[("eac3", 6), ("aac", 2)])), vec![]);
        assert_eq!(
            decide(&tracks(&[("aac", 2), ("dts", 8), ("truehd", 8)])),
            vec![
                Track {
                    index: 1,
                    channels: 8,
                    title: None,
                },
                Track {
                    index: 2,
                    channels: 8,
                    title: None,
                }
            ]
        );
    }

    #[test]
    fn a_title_that_names_the_old_codec_is_rewritten() {
        let track = |title: &str, channels| Track {
            index: 0,
            channels,
            title: Some(title.to_string()),
        };
        assert_eq!(
            new_title(&track("JA DTS-HDMA 5.1", 6)).as_deref(),
            Some("AC-3 5.1")
        );
        assert_eq!(
            new_title(&track("TrueHD Atmos 7.1", 8)).as_deref(),
            Some("AC-3 5.1")
        );
        assert_eq!(new_title(&track("Commentary", 2)), None);
        assert_eq!(
            new_title(&Track {
                index: 0,
                channels: 2,
                title: None
            }),
            None
        );
    }

    /// Runs only where ffmpeg is installed: the clip comes out of the cache
    /// as a whole file with AC-3 in place of DTS, reported once.
    #[test]
    fn a_dts_clip_is_converted_into_the_cache_once() {
        let tmp = TempDir::new();
        let Some(clip) = dts_clip(tmp.path()) else {
            eprintln!("no ffmpeg in PATH, skipping");
            return;
        };
        let transcoder = Transcoder::new(tmp.path().join("cache")).expect("tools");
        let lib = Library::scan(tmp.path(), "test", None);
        let item = lib.items.values().find(|i| i.path == clip).unwrap();
        assert!(
            transcoder.ready(item).is_none(),
            "nothing is ready before work"
        );

        let reported = Mutex::new(Vec::new());
        transcoder.work(&lib, &|| false, &|i| reported.lock().unwrap().push(i.id));
        assert_eq!(*reported.lock().unwrap(), vec![item.id]);
        let converted = transcoder.ready(item).expect("a converted file");
        assert!(converted.starts_with(transcoder.cache_dir()));
        assert!(!converted.to_string_lossy().ends_with(".part"));

        let codecs = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name:stream_tags=title",
                "-of",
                "csv=p=0",
            ])
            .arg(&converted)
            .output()
            .expect("run ffprobe");
        let codecs = String::from_utf8_lossy(&codecs.stdout);
        assert!(
            codecs.contains("ac3") && !codecs.to_lowercase().contains("dts"),
            "{codecs}"
        );
        assert!(codecs.contains("AC-3 1.0"), "the title follows: {codecs}");

        transcoder.work(&lib, &|| false, &|i| reported.lock().unwrap().push(i.id));
        assert_eq!(
            reported.lock().unwrap().len(),
            1,
            "a second pass converts nothing"
        );
    }
}
