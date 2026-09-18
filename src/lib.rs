//! A DLNA (UPnP AV) media server: point it at a folder, browse it from the TV.
//!
//! This crate is the server; `main.rs` is one front end for it, a command
//! line. A GUI builds a [`Config`], calls [`Server::start`] and keeps the
//! handle for [`Server::shutdown`].

pub mod library;
pub mod net;
pub mod thumbs;

mod http;
mod ssdp;
mod upnp;

use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use library::Library;
use thumbs::Thumbnails;

pub const DEFAULT_PORT: u16 = 8200;
/// How long a blocked worker takes to notice a shutdown.
const SHUTDOWN_POLL: Duration = Duration::from_secs(1);

pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Called after every rescan that changed the library.
pub type OnChange = Box<dyn Fn(&Library) + Send + Sync>;

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
    pub on_change: Option<OnChange>,
}

impl Config {
    /// The default port, no timer, no thumbnails, an autodetected address.
    pub fn new(name: impl Into<String>, root: PathBuf) -> Config {
        Config {
            name: name.into(),
            root,
            ip: None,
            port: DEFAULT_PORT,
            rescan_every: None,
            thumbnails: None,
            on_change: None,
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
    thumbs: Option<Thumbnails>,
    on_change: Option<OnChange>,
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
            thumbs: config.thumbnails.map(Thumbnails::new),
            on_change: config.on_change,
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
        self.update_id.fetch_add(1, Ordering::Relaxed);
        if let Some(on_change) = &self.on_change {
            on_change(&fresh);
        }
        (fresh, true)
    }

    /// `SystemUpdateID`: a control point re-browses when this changes.
    pub fn update_id(&self) -> u32 {
        self.update_id.load(Ordering::Relaxed)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }

    /// `None` when thumbnails are off.
    pub fn thumbnails(&self) -> Option<&Thumbnails> {
        self.thumbs.as_ref()
    }

    /// Withdraws the announcement and stops every worker. The handle is inert
    /// afterwards; start a new one to serve again.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        // The accept loop only looks at the flag once a connection arrives.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers {
            let _ = worker.join();
        }
        ssdp::byebye(self);
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

/// FNV-1a, for the device UUID, the library fingerprint and the thumbnail cache.
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
    use std::net::{Ipv4Addr, TcpStream};

    use super::{Config, Server};
    use crate::test_support::TempDir;

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

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
