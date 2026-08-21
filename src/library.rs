//! Filesystem scan: the media tree flattened into stable, numeric UPnP object IDs.

use std::fs;
use std::path::{Path, PathBuf};

/// Depth limit, so a pathological tree cannot run the scan away.
const MAX_DEPTH: u32 = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Folder,
    Video,
    Audio,
    Image,
}

impl Kind {
    pub fn upnp_class(self) -> &'static str {
        match self {
            Kind::Folder => "object.container.storageFolder",
            Kind::Video => "object.item.videoItem",
            Kind::Audio => "object.item.audioItem.musicTrack",
            Kind::Image => "object.item.imageItem.photo",
        }
    }
}

pub struct Item {
    pub id: u32,
    pub parent: u32,
    pub title: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub mime: &'static str,
    /// Sidecar `.srt` sitting next to the video.
    pub subtitle: Option<PathBuf>,
    pub children: Vec<u32>,
}

pub struct Library {
    pub items: Vec<Item>,
    pub folders: usize,
    pub files: usize,
}

impl Library {
    /// Walks `root`, keeping only playable files and the folders leading to them.
    pub fn scan(root: &Path, title: &str) -> Library {
        let mut lib = Library {
            items: vec![Item {
                id: 0,
                parent: 0,
                title: title.to_string(),
                path: root.to_path_buf(),
                kind: Kind::Folder,
                size: 0,
                mime: "",
                subtitle: None,
                children: Vec::new(),
            }],
            folders: 0,
            files: 0,
        };
        let children = lib.walk(root, 0, 0);
        lib.items[0].children = children;
        lib.folders = lib.items.iter().filter(|i| i.kind == Kind::Folder).count() - 1;
        lib.files = lib.items.len() - 1 - lib.folders;
        lib
    }

    pub fn get(&self, id: u32) -> Option<&Item> {
        self.items.get(id as usize)
    }

    /// Adds every playable child of `dir` and returns their ids, in display order.
    fn walk(&mut self, dir: &Path, parent: u32, depth: u32) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };

        // Directories first, then files, each alphabetically and case-insensitively.
        let mut sorted: Vec<(bool, String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            // `file_type` does not follow symlinks, so link cycles cannot trap us.
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() && !ft.is_file() {
                continue;
            }
            sorted.push((ft.is_dir(), name.to_lowercase(), entry.path()));
        }
        sorted.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let mut children = Vec::new();
        for (is_dir, _, path) in sorted {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_dir {
                if depth >= MAX_DEPTH {
                    continue;
                }
                if let Some(id) = self.add_folder(&path, parent, name, depth) {
                    children.push(id);
                }
            } else if let Some((kind, mime)) = classify(&path) {
                children.push(self.add_file(&path, parent, name, kind, mime));
            }
        }
        children
    }

    /// Returns `None` for a folder with no playable content anywhere below it.
    fn add_folder(&mut self, path: &Path, parent: u32, title: String, depth: u32) -> Option<u32> {
        let id = self.items.len() as u32;
        self.items.push(Item {
            id,
            parent,
            title,
            path: path.to_path_buf(),
            kind: Kind::Folder,
            size: 0,
            mime: "",
            subtitle: None,
            children: Vec::new(),
        });

        let children = self.walk(path, id, depth + 1);
        if children.is_empty() {
            // Nothing was kept below, so nothing above `id` was pushed either.
            self.items.truncate(id as usize);
            return None;
        }
        self.items[id as usize].children = children;
        Some(id)
    }

    fn add_file(
        &mut self,
        path: &Path,
        parent: u32,
        name: String,
        kind: Kind,
        mime: &'static str,
    ) -> u32 {
        let id = self.items.len() as u32;
        let title = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(stem, _)| stem);
        self.items.push(Item {
            id,
            parent,
            title: title.to_string(),
            path: path.to_path_buf(),
            kind,
            size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            mime,
            subtitle: if kind == Kind::Video {
                find_subtitle(path)
            } else {
                None
            },
            children: Vec::new(),
        });
        id
    }
}

/// A `.srt` sharing the video's stem, which LG and Samsung sets pick up over HTTP.
fn find_subtitle(video: &Path) -> Option<PathBuf> {
    for ext in ["srt", "SRT"] {
        let candidate = video.with_extension(ext);
        if candidate != video && candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn classify(path: &Path) -> Option<(Kind, &'static str)> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    let entry = match ext.as_str() {
        "mkv" => (Kind::Video, "video/x-matroska"),
        "mp4" | "m4v" => (Kind::Video, "video/mp4"),
        "avi" | "divx" => (Kind::Video, "video/x-msvideo"),
        "mov" => (Kind::Video, "video/quicktime"),
        "wmv" => (Kind::Video, "video/x-ms-wmv"),
        "webm" => (Kind::Video, "video/webm"),
        "flv" => (Kind::Video, "video/x-flv"),
        "ts" | "m2ts" | "mts" => (Kind::Video, "video/mp2t"),
        "mpg" | "mpeg" | "vob" => (Kind::Video, "video/mpeg"),
        "mp3" => (Kind::Audio, "audio/mpeg"),
        "flac" => (Kind::Audio, "audio/flac"),
        "m4a" => (Kind::Audio, "audio/mp4"),
        "aac" => (Kind::Audio, "audio/aac"),
        "ogg" | "oga" => (Kind::Audio, "audio/ogg"),
        "opus" => (Kind::Audio, "audio/opus"),
        "wav" => (Kind::Audio, "audio/wav"),
        "wma" => (Kind::Audio, "audio/x-ms-wma"),
        "jpg" | "jpeg" => (Kind::Image, "image/jpeg"),
        "png" => (Kind::Image, "image/png"),
        "gif" => (Kind::Image, "image/gif"),
        "bmp" => (Kind::Image, "image/bmp"),
        _ => return None,
    };
    Some(entry)
}
