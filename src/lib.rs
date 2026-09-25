//! A DLNA (UPnP AV) media server: point it at a folder, browse it from the TV.
//!
//! This crate is the server; `main.rs` is one front end for it, a command
//! line. A GUI builds a [`Config`], calls [`Server::start`] and keeps the
//! handle for [`Server::shutdown`].

pub mod library;
pub mod net;
pub mod thumbs;
pub mod transcode;

mod gena;
mod http;
mod ssdp;
mod upnp;

use std::io;
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gena::{Service, Subscriptions};
use library::{Item, Library};
use thumbs::Thumbnails;
use transcode::Transcoder;

pub const DEFAULT_PORT: u16 = 8200;
/// How long a blocked worker takes to notice a shutdown.
const SHUTDOWN_POLL: Duration = Duration::from_secs(1);
/// How long a new video waits before its audio is probed.
const PROBE_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// What the server reports as it runs. Errors go to stderr; this is the rest.
pub enum Event<'a> {
    /// A rescan swapped the library in.
    LibraryChanged(&'a Library),
    /// A file with audio the TV cannot decode now has a converted twin.
    Converted(&'a Item),
    /// Every HTTP request, as it comes in. Chatty; for watching what a TV does.
    Request {
        peer: IpAddr,
        method: &'a str,
        path: &'a str,
        /// The `Range` header, the one that tells what a renderer is up to.
        range: Option<&'a str>,
    },
    /// A renderer started reading a video or a track, `from` the position it
    /// asked for; `converted` when the audio is re-encoded on the way.
    Streaming {
        peer: IpAddr,
        item: &'a Item,
        converted: bool,
        from: Duration,
    },
}

pub type OnEvent = Box<dyn Fn(Event) + Send + Sync>;

/// What to serve and how to reach it.
pub struct Config {
    /// Name shown on the TV.
    pub name: String,
    /// Folder to share. Canonical, so the scan records real paths.
    pub root: PathBuf,
    /// LAN address to advertise; `None` picks one, see [`net::pick_ip`].
    pub ip: Option<Ipv4Addr>,
    /// HTTP port; 0 takes any free one.
    pub port: u16,
    /// Rescan on a timer; `None` leaves rescans to [`Server::rescan`].
    pub rescan_every: Option<Duration>,
    /// Where generated thumbnails are cached; `None` turns thumbnails off.
    pub thumbnails: Option<PathBuf>,
    /// Where converted files go; `None` leaves audio the TV cannot decode
    /// as it is.
    pub transcode: Option<PathBuf>,
    pub on_event: Option<OnEvent>,
}

impl Config {
    /// The default port, an autodetected address, no timer, no thumbnails,
    /// no transcoding.
    pub fn new(name: impl Into<String>, root: PathBuf) -> Config {
        Config {
            name: name.into(),
            root,
            ip: None,
            port: DEFAULT_PORT,
            rescan_every: None,
            thumbnails: None,
            transcode: None,
            on_event: None,
        }
    }
}

pub struct Server {
    pub name: String,
    pub uuid: String,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub root: PathBuf,
    lib: RwLock<Arc<Library>>,
    update_id: AtomicU32,
    events: Subscriptions,
    thumbs: Option<Thumbnails>,
    transcoder: Option<Transcoder>,
    on_event: Option<OnEvent>,
    stopping: AtomicBool,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Server {
    /// Binds the HTTP port, scans the folder and starts answering discovery.
    pub fn start(config: Config) -> io::Result<Arc<Server>> {
        let ip = match config.ip {
            Some(ip) => ip,
            None => net::pick_ip(&net::interfaces()?)?,
        };
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.port))?;
        let port = listener.local_addr()?.port();
        let rescan_every = config.rescan_every;

        let srv = Arc::new(Server::build(config, ip, port));
        let mut workers = vec![http::serve(listener, Arc::clone(&srv))];
        workers.extend(ssdp::start(Arc::clone(&srv))?);
        if let Some(interval) = rescan_every {
            let watcher = Arc::clone(&srv);
            workers.push(thread::spawn(move || {
                while watcher.sleep(interval) {
                    watcher.rescan();
                }
            }));
        }
        if srv.transcoder.is_some() {
            let worker = Arc::clone(&srv);
            workers.push(thread::spawn(move || {
                loop {
                    let transcoder = worker.transcoder.as_ref().expect("checked above");
                    transcoder.work(&worker.library(), &|| worker.stopping(), &|item| {
                        worker.changed();
                        worker.notify(Event::Converted(item));
                    });
                    if !worker.sleep(PROBE_INTERVAL) {
                        break;
                    }
                }
            }));
        }
        *srv.workers.lock().unwrap() = workers;
        Ok(srv)
    }

    /// The scan and the state, with nothing bound or spawned yet.
    pub(crate) fn build(config: Config, ip: Ipv4Addr, port: u16) -> Server {
        let library = Library::scan(&config.root, &config.name, None);
        Server {
            uuid: upnp::stable_uuid(&format!("{}|{}", config.name, config.root.display())),
            name: config.name,
            ip,
            port,
            root: config.root,
            lib: RwLock::new(Arc::new(library)),
            update_id: AtomicU32::new(1),
            events: Subscriptions::new(),
            thumbs: config.thumbnails.map(Thumbnails::new),
            transcoder: config.transcode.and_then(Transcoder::new),
            on_event: config.on_event,
            stopping: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
        }
    }

    pub fn library(&self) -> Arc<Library> {
        Arc::clone(&self.lib.read().unwrap())
    }

    /// Rescans, swapping the library in only if the tree actually changed.
    pub fn rescan(&self) -> (Arc<Library>, bool) {
        let current = self.library();
        let fresh = Library::scan(&self.root, &self.name, Some(&current));
        if fresh.fingerprint == current.fingerprint {
            return (current, false);
        }
        let fresh = Arc::new(fresh);
        *self.lib.write().unwrap() = Arc::clone(&fresh);
        self.changed();
        self.notify(Event::LibraryChanged(&fresh));
        (fresh, true)
    }

    /// `SystemUpdateID`: a control point re-browses when this changes.
    pub fn update_id(&self) -> u32 {
        self.update_id.load(Ordering::Relaxed)
    }

    /// Moves SystemUpdateID and posts it to the ContentDirectory subscribers:
    /// a TV browses again only once told.
    pub(crate) fn changed(&self) {
        let id = self.update_id.fetch_add(1, Ordering::Relaxed) + 1;
        let state = gena::propertyset(&[("SystemUpdateID", id.to_string())]);
        self.events.publish(Service::ContentDirectory, &state);
    }

    pub(crate) fn events(&self) -> &Subscriptions {
        &self.events
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }

    /// `None` when thumbnails are off.
    pub fn thumbnails(&self) -> Option<&Thumbnails> {
        self.thumbs.as_ref()
    }

    /// `None` when transcoding is off, or ffmpeg is not installed.
    pub fn transcoder(&self) -> Option<&Transcoder> {
        self.transcoder.as_ref()
    }

    /// Withdraws the announcement and stops every worker. The handle is inert
    /// afterwards; start a new one to serve again.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(transcoder) = &self.transcoder {
            transcoder.abort();
        }
        // The accept loop only looks at the flag once a connection arrives.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers {
            let _ = worker.join();
        }
        ssdp::byebye(self);
    }

    pub(crate) fn notify(&self, event: Event) {
        if let Some(on_event) = &self.on_event {
            on_event(event);
        }
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Sleeps `dur` in slices, so a shutdown is not kept waiting. False once
    /// one is under way.
    pub(crate) fn sleep(&self, dur: Duration) -> bool {
        let deadline = Instant::now() + dur;
        while !self.stopping() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return true;
            };
            thread::sleep(left.min(SHUTDOWN_POLL));
        }
        false
    }
}

/// Path, size and mtime of an item's file: a replaced file gets a fresh
/// thumbnail and a fresh conversion.
pub(crate) fn cache_key(item: &Item) -> u64 {
    let modified = std::fs::metadata(&item.path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let key = fnv1a(FNV_OFFSET, item.path.to_string_lossy().as_bytes());
    let key = fnv1a(key, &item.size.to_le_bytes());
    fnv1a(key, &modified.to_le_bytes())
}

/// FNV-1a, for the device UUID, the library fingerprint and the caches.
pub(crate) fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    let mut hash = seed;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    use super::{Config, Server};
    use crate::test_support::{TempDir, receive_notify};

    /// A TV subscribed to ContentDirectory hears the new SystemUpdateID when
    /// the library changes, and browses again.
    #[test]
    fn a_subscriber_is_told_when_the_library_changes() {
        let tmp = TempDir::new();
        tmp.file("one.mp4", 1);
        let mut config = Config::new("Test", tmp.path().to_path_buf());
        config.ip = Some(Ipv4Addr::LOCALHOST);
        config.port = 0;
        let srv = Server::start(config).expect("start");

        let tv = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, srv.port)).unwrap();
        write!(
            stream,
            "SUBSCRIBE /evt/cd HTTP/1.1\r\nHOST: 127.0.0.1\r\n\
             CALLBACK: <http://{}/evt>\r\nNT: upnp:event\r\nTIMEOUT: Second-300\r\n\r\n",
            tv.local_addr().unwrap()
        )
        .unwrap();
        let answer: Vec<String> = BufReader::new(&stream)
            .lines()
            .map_while(Result::ok)
            .take_while(|line| !line.is_empty())
            .collect();
        assert!(answer[0].contains(" 200 "), "{answer:?}");
        assert!(
            answer.iter().any(|h| h.starts_with("SID: uuid:")),
            "{answer:?}"
        );

        let (seq, body) = receive_notify(&tv);
        assert_eq!(seq, "0");
        assert!(
            body.contains("<SystemUpdateID>1</SystemUpdateID>"),
            "{body}"
        );

        tmp.file("two.mp4", 1);
        assert!(srv.rescan().1, "the library changed");
        let (seq, body) = receive_notify(&tv);
        assert_eq!(seq, "1");
        assert!(
            body.contains("<SystemUpdateID>2</SystemUpdateID>"),
            "{body}"
        );
        srv.shutdown();
    }

    #[test]
    fn shutdown_frees_the_port_for_the_next_start() {
        let tmp = TempDir::new();
        tmp.file("clip.mp4", 1);
        let config = || {
            let mut config = Config::new("Test", tmp.path().to_path_buf());
            config.ip = Some(Ipv4Addr::LOCALHOST);
            config.port = 0;
            config
        };

        let first = Server::start(config()).expect("start");
        let port = first.port;
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
        first.shutdown();
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
            "the listener should be gone"
        );

        let mut again = config();
        again.port = port;
        let second = Server::start(again).expect("start again on the same port");
        assert_eq!(second.port, port);
        second.shutdown();
    }
}

/// A scratch directory that clears itself up, so the scan can be tested against
/// real trees without a crate to build them.
#[cfg(test)]
pub mod test_support {
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub struct TempDir(PathBuf);

    #[allow(clippy::new_without_default)]
    impl TempDir {
        pub fn new() -> TempDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "dlna-server-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create scratch directory");
            // The scan records paths verbatim, and `/var` is a symlink on macOS.
            TempDir(path.canonicalize().expect("canonicalise scratch directory"))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn file(&self, relative: &str, size: usize) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent folder");
            }
            fs::write(&path, vec![0u8; size]).expect("write file");
            path
        }

        pub fn remove(&self, relative: &str) {
            fs::remove_file(self.0.join(relative)).expect("remove file");
        }
    }

    /// A two-second clip with a mono DTS track titled `DTS`, where ffmpeg is
    /// installed.
    pub fn dts_clip(dir: &Path) -> Option<PathBuf> {
        let clip = dir.join("clip.mkv");
        // The DTS encoder is marked experimental, hence -strict -2.
        let made = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=2:size=64x64:rate=10")
            .args(["-f", "lavfi", "-i", "sine=duration=2"])
            .args(["-c:v", "mpeg4", "-c:a", "dca", "-strict", "-2"])
            .args(["-metadata:s:a:0", "title=DTS"])
            .arg(&clip)
            .status();
        made.is_ok_and(|s| s.success()).then_some(clip)
    }

    /// Reads one NOTIFY off `listener`, answers 200, and returns its SEQ
    /// header and body.
    pub fn receive_notify(listener: &TcpListener) -> (String, String) {
        let (stream, _) = listener.accept().expect("a NOTIFY");
        let mut reader = BufReader::new(&stream);
        let mut seq = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("SEQ: ") {
                seq = value.to_string();
            }
            if let Some(value) = line.strip_prefix("CONTENT-LENGTH: ") {
                length = value.parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (&stream).write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
        (seq, String::from_utf8(body).unwrap())
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
