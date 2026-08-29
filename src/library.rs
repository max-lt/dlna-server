//! Filesystem scan: the media tree flattened into numeric UPnP object IDs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::{FNV_OFFSET, fnv1a};

/// Depth limit, so a pathological tree cannot run the scan away.
const MAX_DEPTH: u32 = 16;
const ROOT: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub items: HashMap<u32, Item>,
    /// Ids already handed out, keyed by path. A rescan reuses them, or a TV
    /// holding an id from before would resolve it to a different file.
    assigned: HashMap<PathBuf, u32>,
    next_id: u32,
    pub folders: usize,
    pub files: usize,
    /// Differs whenever a file is added, removed, replaced or resized.
    pub fingerprint: u64,
}

impl Library {
    /// Walks `root`, keeping only playable files and the folders leading to
    /// them. Object ids carry over from `previous`.
    pub fn scan(root: &Path, title: &str, previous: Option<&Library>) -> Library {
        let mut lib = Library {
            items: HashMap::new(),
            assigned: previous.map(|p| p.assigned.clone()).unwrap_or_default(),
            next_id: previous.map_or(ROOT + 1, |p| p.next_id),
            folders: 0,
            files: 0,
            fingerprint: FNV_OFFSET,
        };
        lib.assigned.insert(root.to_path_buf(), ROOT);

        let children = lib.walk(root, ROOT, 0);
        lib.items.insert(
            ROOT,
            Item {
                id: ROOT,
                parent: ROOT,
                title: title.to_string(),
                path: root.to_path_buf(),
                kind: Kind::Folder,
                size: 0,
                mime: "",
                subtitle: None,
                children,
            },
        );
        lib.folders = lib
            .items
            .values()
            .filter(|i| i.kind == Kind::Folder)
            .count()
            - 1;
        lib.files = lib.items.len() - 1 - lib.folders;
        lib
    }

    pub fn get(&self, id: u32) -> Option<&Item> {
        self.items.get(&id)
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
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() && !file_type.is_file() {
                continue;
            }
            sorted.push((file_type.is_dir(), name.to_lowercase(), entry.path()));
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
        // Claimed before the walk because the children need it as their parent,
        // and kept on the pruned path so the folder keeps its id if it later
        // gains content.
        let id = self.claim_id(path);
        let children = self.walk(path, id, depth + 1);
        if children.is_empty() {
            return None;
        }

        self.stamp(path, 0, 0);
        self.items.insert(
            id,
            Item {
                id,
                parent,
                title,
                path: path.to_path_buf(),
                kind: Kind::Folder,
                size: 0,
                mime: "",
                subtitle: None,
                children,
            },
        );
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
        let id = self.claim_id(path);
        let title = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(stem, _)| stem);
        let metadata = fs::metadata(path);
        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = metadata
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());

        self.stamp(path, size, modified);
        self.items.insert(
            id,
            Item {
                id,
                parent,
                title: title.to_string(),
                path: path.to_path_buf(),
                kind,
                size,
                mime,
                subtitle: if kind == Kind::Video {
                    find_subtitle(path)
                } else {
                    None
                },
                children: Vec::new(),
            },
        );
        id
    }

    fn claim_id(&mut self, path: &Path) -> u32 {
        if let Some(&id) = self.assigned.get(path) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.assigned.insert(path.to_path_buf(), id);
        id
    }

    /// Folds one entry into the fingerprint. The walk is sorted, so the same
    /// tree always folds in the same order.
    fn stamp(&mut self, path: &Path, size: u64, modified: u64) {
        self.fingerprint = fnv1a(self.fingerprint, path.to_string_lossy().as_bytes());
        self.fingerprint = fnv1a(self.fingerprint, &size.to_le_bytes());
        self.fingerprint = fnv1a(self.fingerprint, &modified.to_le_bytes());
    }
}

/// A `.srt` sharing the video's stem, which LG and Samsung sets pick up over HTTP.
fn find_subtitle(video: &Path) -> Option<PathBuf> {
    for extension in ["srt", "SRT"] {
        let candidate = video.with_extension(extension);
        if candidate != video && candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn classify(path: &Path) -> Option<(Kind, &'static str)> {
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    let entry = match extension.as_str() {
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

#[cfg(test)]
mod tests {
    use super::{Kind, Library};
    use crate::test_support::TempDir;

    fn id_of(lib: &Library, title: &str) -> Option<u32> {
        lib.items.values().find(|i| i.title == title).map(|i| i.id)
    }

    #[test]
    fn a_scan_keeps_only_playable_files() {
        let tmp = TempDir::new();
        tmp.file("holiday.mp4", 1);
        tmp.file("notes.txt", 1);
        let lib = Library::scan(tmp.path(), "test", None);

        assert_eq!(lib.files, 1);
        assert_eq!(lib.folders, 0);
        assert!(id_of(&lib, "holiday").is_some());
        assert_eq!(id_of(&lib, "notes"), None);
    }

    #[test]
    fn ids_carry_over_a_rescan() {
        let tmp = TempDir::new();
        tmp.file("a.mkv", 1);
        tmp.file("b.mkv", 1);
        let first = Library::scan(tmp.path(), "test", None);
        let (a, b) = (id_of(&first, "a").unwrap(), id_of(&first, "b").unwrap());

        tmp.file("c.mkv", 1);
        let second = Library::scan(tmp.path(), "test", Some(&first));

        assert_eq!(id_of(&second, "a"), Some(a));
        assert_eq!(id_of(&second, "b"), Some(b));
        let c = id_of(&second, "c").unwrap();
        assert!(c != a && c != b, "a new file took an id already handed out");
    }

    /// The invariant a TV depends on: an id it cached before the rescan either
    /// still resolves to the file it asked for, or to nothing at all.
    #[test]
    fn a_removed_file_never_passes_its_id_on() {
        let tmp = TempDir::new();
        tmp.file("gone.mkv", 1);
        let first = Library::scan(tmp.path(), "test", None);
        let gone = id_of(&first, "gone").unwrap();

        tmp.remove("gone.mkv");
        tmp.file("fresh.mkv", 1);
        let second = Library::scan(tmp.path(), "test", Some(&first));

        assert_eq!(second.get(gone).map(|i| i.id), None);
        assert_ne!(id_of(&second, "fresh"), Some(gone));
    }

    #[test]
    fn folders_without_playable_content_are_pruned() {
        let tmp = TempDir::new();
        tmp.file("papers/thesis.txt", 1);
        tmp.file("clips/holiday.mp4", 1);
        let lib = Library::scan(tmp.path(), "test", None);

        assert_eq!(id_of(&lib, "papers"), None);
        assert!(id_of(&lib, "clips").is_some());
        assert_eq!(lib.folders, 1);
        assert_eq!(lib.files, 1);
    }

    /// `add_folder` claims an id before it walks, so a folder that was empty at
    /// the first scan keeps that id once it earns a place in the tree.
    #[test]
    fn a_pruned_folder_keeps_its_id_when_it_gains_content() {
        let tmp = TempDir::new();
        tmp.file("later/thesis.txt", 1);
        let first = Library::scan(tmp.path(), "test", None);
        assert_eq!(id_of(&first, "later"), None);
        let claimed = first.assigned[&tmp.path().join("later")];

        tmp.file("later/holiday.mp4", 1);
        let second = Library::scan(tmp.path(), "test", Some(&first));
        assert_eq!(id_of(&second, "later"), Some(claimed));
    }

    #[test]
    fn an_unchanged_tree_keeps_its_fingerprint() {
        let tmp = TempDir::new();
        tmp.file("a.mkv", 10);
        let first = Library::scan(tmp.path(), "test", None);
        let again = Library::scan(tmp.path(), "test", Some(&first));
        assert_eq!(first.fingerprint, again.fingerprint);

        tmp.file("a.mkv", 20);
        let resized = Library::scan(tmp.path(), "test", Some(&first));
        assert_ne!(resized.fingerprint, first.fingerprint);

        tmp.file("b.mkv", 1);
        let grown = Library::scan(tmp.path(), "test", Some(&resized));
        assert_ne!(grown.fingerprint, resized.fingerprint);
    }

    #[test]
    fn a_sidecar_srt_rides_along_without_becoming_an_item() {
        let tmp = TempDir::new();
        tmp.file("film.mkv", 1);
        tmp.file("film.srt", 1);
        tmp.file("silent.mkv", 1);
        let lib = Library::scan(tmp.path(), "test", None);

        let film = lib.get(id_of(&lib, "film").unwrap()).unwrap();
        assert_eq!(film.kind, Kind::Video);
        assert!(film.subtitle.is_some());
        assert!(lib.get(id_of(&lib, "silent").unwrap()).unwrap().subtitle.is_none());
        assert_eq!(lib.files, 2, "the .srt should not be browsable itself");
    }

    #[test]
    fn dotfiles_are_skipped() {
        let tmp = TempDir::new();
        tmp.file(".hidden.mp4", 1);
        tmp.file("shown.mp4", 1);
        let lib = Library::scan(tmp.path(), "test", None);
        assert_eq!(lib.files, 1);
        assert!(id_of(&lib, "shown").is_some());
    }
}
