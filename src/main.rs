//! A DLNA (UPnP AV) media server: point it at a folder, browse it from the TV.

mod http;
mod library;
mod net;
mod ssdp;
mod upnp;

use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use library::Library;

const DEFAULT_PORT: u16 = 8200;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub struct Server {
    pub name: String,
    pub uuid: String,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub root: PathBuf,
    lib: RwLock<Arc<Library>>,
}

impl Server {
    pub fn library(&self) -> Arc<Library> {
        Arc::clone(&self.lib.read().unwrap())
    }

    pub fn rescan(&self) -> Arc<Library> {
        let fresh = Arc::new(Library::scan(&self.root, &self.name));
        *self.lib.write().unwrap() = Arc::clone(&fresh);
        fresh
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }
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

    let library = Library::scan(&options.dir, &options.name);
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
    });

    http::serve(listener, Arc::clone(&srv));
    ssdp::start(Arc::clone(&srv))?;

    println!("listening on {}  (uuid:{})", srv.base_url(), srv.uuid);
    println!("announcing over SSDP; pick it up from the TV's media player");

    net::on_terminate(on_signal);
    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }

    println!("\nshutting down, withdrawing announcements");
    ssdp::byebye(&srv);
    Ok(())
}

extern "C" fn on_signal(_signal: i32) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

struct Options {
    dir: PathBuf,
    name: String,
    port: u16,
    ip: Option<Ipv4Addr>,
}

impl Options {
    /// `Ok(None)` means `--help` was handled and there is nothing left to do.
    fn parse() -> Result<Option<Options>, String> {
        let mut dir = None;
        let mut name = None;
        let mut port = DEFAULT_PORT;
        let mut ip = None;

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
        --ip <ADDR>      LAN address to advertise (default: autodetected)
    -h, --help           Print this help

Visit http://<addr>:<port>/rescan to pick up files added since startup.
";
