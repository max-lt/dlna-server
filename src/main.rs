//! A DLNA (UPnP AV) media server: point it at a folder, browse it from the TV.

mod http;
mod library;
mod net;
mod ssdp;
mod upnp;

use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use library::Library;

const DEFAULT_PORT: u16 = 8200;
/// Long enough that the stat sweep stays invisible, short enough that a file
/// dropped in shows up before you reach for the remote.
const DEFAULT_WATCH: Duration = Duration::from_secs(30);

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub struct Server {
    pub name: String,
    pub uuid: String,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub root: PathBuf,
    lib: RwLock<Arc<Library>>,
    update_id: AtomicU32,
}

impl Server {
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
        (fresh, true)
    }

    /// `SystemUpdateID`: a control point re-browses when this changes.
    pub fn update_id(&self) -> u32 {
        self.update_id.load(Ordering::Relaxed)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }
}

/// FNV-1a, for the device UUID and the library fingerprint.
pub fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    let mut hash = seed;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn main() -> ExitCode {
    let options = match Options::parse() {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("try `dlna-server --help`");
            return ExitCode::from(2);
        }
    };

    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: Options) -> std::io::Result<()> {
    let ip = match options.ip {
        Some(ip) => ip,
        None => net::local_ipv4()?,
    };

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, options.port))?;
    let port = listener.local_addr()?.port();

    let library = Library::scan(&options.dir, &options.name, None);
    println!(
        "{}: {} folders, {} files under {}",
        options.name,
        library.folders,
        library.files,
        options.dir.display()
    );
    if library.files == 0 {
        eprintln!("warning: no playable media found, the server will look empty on the TV");
    }

    let srv = Arc::new(Server {
        uuid: upnp::stable_uuid(&format!("{}|{}", options.name, options.dir.display())),
        name: options.name,
        ip,
        port,
        root: options.dir,
        lib: RwLock::new(Arc::new(library)),
        update_id: AtomicU32::new(1),
    });

    http::serve(listener, Arc::clone(&srv));
    ssdp::start(Arc::clone(&srv))?;
    if let Some(interval) = options.watch {
        watch_library(Arc::clone(&srv), interval);
    }

    println!("listening on {}  (uuid:{})", srv.base_url(), srv.uuid);
    match options.watch {
        Some(interval) => println!("rescanning every {}s", interval.as_secs()),
        None => println!("rescan on demand at {}/rescan", srv.base_url()),
    }
    println!("announcing over SSDP; pick it up from the TV's media player");

    net::on_terminate(on_signal);
    while !SHUTDOWN.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }

    println!("\nshutting down, withdrawing announcements");
    ssdp::byebye(&srv);
    Ok(())
}

/// Rescans on a timer, so files added or removed reach the TV without a restart.
fn watch_library(srv: Arc<Server>, interval: Duration) {
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            let (lib, changed) = srv.rescan();
            if changed {
                println!(
                    "library changed: {} folders, {} files",
                    lib.folders, lib.files
                );
            }
        }
    });
}

extern "C" fn on_signal(_signal: i32) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

struct Options {
    dir: PathBuf,
    name: String,
    port: u16,
    ip: Option<Ipv4Addr>,
    watch: Option<Duration>,
}

impl Options {
    /// `Ok(None)` means `--help` was handled and there is nothing left to do.
    fn parse() -> Result<Option<Options>, String> {
        let mut dir = None;
        let mut name = None;
        let mut port = DEFAULT_PORT;
        let mut ip = None;
        let mut watch = Some(DEFAULT_WATCH);

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
            match arg.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    return Ok(None);
                }
                "-p" | "--port" => {
                    port = value("--port")?
                        .parse()
                        .map_err(|_| "--port must be a number".to_string())?;
                }
                "-n" | "--name" => name = Some(value("--name")?),
                "-w" | "--watch" => {
                    let seconds: u64 = value("--watch")?
                        .parse()
                        .map_err(|_| "--watch must be a number of seconds".to_string())?;
                    watch = (seconds > 0).then(|| Duration::from_secs(seconds));
                }
                "--ip" => {
                    ip = Some(
                        value("--ip")?
                            .parse()
                            .map_err(|_| "--ip must be an IPv4 address".to_string())?,
                    );
                }
                other if other.starts_with('-') => return Err(format!("unknown option {other}")),
                other if dir.is_none() => dir = Some(PathBuf::from(other)),
                other => return Err(format!("unexpected argument {other}")),
            }
        }

        let dir = dir
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Movies")))
            .ok_or_else(|| "no directory given and $HOME is unset".to_string())?;
        let dir = dir
            .canonicalize()
            .map_err(|err| format!("{}: {err}", dir.display()))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }

        Ok(Some(Options {
            name: name.unwrap_or_else(|| "Rust DLNA".to_string()),
            dir,
            port,
            ip,
            watch,
        }))
    }
}

const USAGE: &str = "\
dlna-server: share a folder with any DLNA renderer on the LAN

USAGE:
    dlna-server [DIR] [OPTIONS]

ARGS:
    DIR                  Folder to share (default: $HOME/Movies)

OPTIONS:
    -p, --port <PORT>    HTTP port (default: 8200)
    -n, --name <NAME>    Name shown on the TV (default: \"Rust DLNA\")
    -w, --watch <SECS>   Rescan interval, 0 to disable (default: 30)
        --ip <ADDR>      LAN address to advertise (default: autodetected)
    -h, --help           Print this help

A rescan can also be triggered on demand at http://<addr>:<port>/rescan.
";
