//! A DLNA (UPnP AV) media server: point it at a folder, browse it from the TV.

mod http;
mod library;
mod net;
mod ssdp;
mod upnp;

use std::io;
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

fn run(options: Options) -> io::Result<()> {
    let interfaces = net::interfaces()?;
    let ip = match options.ip {
        Some(ip) => ip,
        None => pick_ip(&interfaces)?,
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
    let lans: Vec<net::Interface> = interfaces.into_iter().filter(|i| !i.vm).collect();
    if options.ip.is_none() && lans.len() > 1 {
        println!("several interfaces are up; if the TV is on another one, pass --ip <ADDR>:");
        print!("{}", interface_table(&lans, Some(ip), "(active)"));
    }

    net::on_terminate(on_signal);
    while !SHUTDOWN.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }

    println!("\nshutting down, withdrawing announcements");
    ssdp::byebye(&srv);
    Ok(())
}

/// Without `--ip`: the default route's address when it sits on a LAN, else the
/// first LAN. A VPN as default route is not where the TV is, nor is a VM bridge.
fn pick_ip(interfaces: &[net::Interface]) -> io::Result<Ipv4Addr> {
    pick(net::local_ipv4(), interfaces)
}

fn pick(route: io::Result<Ipv4Addr>, interfaces: &[net::Interface]) -> io::Result<Ipv4Addr> {
    let lans: Vec<&net::Interface> = interfaces.iter().filter(|i| !i.vm).collect();
    match (route, lans.first()) {
        (Ok(ip), _) if lans.iter().any(|i| i.addr == ip) => Ok(ip),
        (_, Some(lan)) => Ok(lan.addr),
        (route, None) => route.map_err(|err| {
            io::Error::other(format!(
                "no LAN interface is up and no default route ({err}); pass --ip"
            ))
        }),
    }
}

/// `--list`: every interface up, the one the server would pick on its own
/// marked, and the VM bridges it never picks marked too.
fn list_interfaces() -> Result<(), String> {
    let interfaces = net::interfaces().map_err(|err| format!("listing interfaces: {err}"))?;
    if interfaces.is_empty() {
        return Err("no LAN interface is up".to_string());
    }
    let picked = pick_ip(&interfaces).ok();
    print!("{}", interface_table(&interfaces, picked, "(default)"));
    Ok(())
}

/// One interface per line, columns aligned, `label` on the marked address and
/// `(vm bridge)` on the ones the server never picks.
fn interface_table(interfaces: &[net::Interface], marked: Option<Ipv4Addr>, label: &str) -> String {
    let width = interfaces.iter().map(|i| i.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for i in interfaces {
        let cidr = format!("{}/{}", i.addr, i.prefix);
        let mark = if marked == Some(i.addr) {
            label
        } else if i.vm {
            "(vm bridge)"
        } else {
            ""
        };
        let line = format!("    {:width$}  {cidr:18} {mark}", i.name);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
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
    /// `Ok(None)` means `--help` or `--list` was handled and there is nothing
    /// left to do.
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
                "--list" => return list_interfaces().map(|()| None),
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
        --list           List the interfaces --ip can pick, then exit
    -h, --help           Print this help

A rescan can also be triggered on demand at http://<addr>:<port>/rescan.
";

#[cfg(test)]
mod tests {
    use super::*;
    use net::Interface;

    fn lan(name: &str, addr: [u8; 4]) -> Interface {
        Interface {
            name: name.into(),
            addr: addr.into(),
            prefix: 24,
            vm: false,
        }
    }

    fn vm(name: &str, addr: [u8; 4]) -> Interface {
        Interface {
            vm: true,
            ..lan(name, addr)
        }
    }

    fn no_route() -> io::Result<Ipv4Addr> {
        Err(io::Error::other("Network is unreachable"))
    }

    #[test]
    fn default_route_wins_when_it_is_a_lan() {
        let lans = [
            lan("en0", [192, 168, 1, 69]),
            lan("en7", [192, 168, 8, 103]),
        ];
        assert_eq!(pick(Ok(lans[1].addr), &lans).unwrap(), lans[1].addr);
    }

    #[test]
    fn tunnel_or_missing_default_route_yields_to_the_first_lan() {
        let lans = [lan("en7", [192, 168, 8, 103])];
        assert_eq!(pick(Ok([10, 8, 0, 2].into()), &lans).unwrap(), lans[0].addr);
        assert_eq!(pick(no_route(), &lans).unwrap(), lans[0].addr);
    }

    #[test]
    fn vm_bridges_are_never_picked_over_a_lan() {
        let ifs = [
            vm("bridge100", [192, 168, 139, 3]),
            lan("en7", [192, 168, 8, 103]),
        ];
        assert_eq!(pick(Ok(ifs[0].addr), &ifs).unwrap(), ifs[1].addr);
        assert_eq!(pick(no_route(), &ifs).unwrap(), ifs[1].addr);
        // With no LAN at all, the route is still the only address to try.
        assert_eq!(pick(Ok(ifs[0].addr), &ifs[..1]).unwrap(), ifs[0].addr);
        assert!(pick(no_route(), &ifs[..1]).is_err());
    }

    #[test]
    fn no_lan_falls_back_to_the_route_or_fails() {
        let tunnel = Ipv4Addr::new(10, 8, 0, 2);
        assert_eq!(pick(Ok(tunnel), &[]).unwrap(), tunnel);
        assert!(pick(no_route(), &[]).is_err());
    }

    #[test]
    fn table_aligns_columns_and_marks_active_and_vm() {
        let ifs = [
            lan("en0", [192, 168, 1, 69]),
            vm("bridge100", [192, 168, 139, 3]),
            lan("en7", [192, 168, 8, 103]),
        ];
        assert_eq!(
            interface_table(&ifs, Some(ifs[0].addr), "(active)"),
            "    en0        192.168.1.69/24    (active)\n\
             \x20   bridge100  192.168.139.3/24   (vm bridge)\n\
             \x20   en7        192.168.8.103/24\n"
        );
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
