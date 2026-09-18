//! The command line front end: options, the banner, Ctrl-C.

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use dlna_server::{Config, DEFAULT_PORT, Event, Server, net};

/// Long enough that the stat sweep stays invisible, short enough that a file
/// dropped in shows up before you reach for the remote.
const DEFAULT_WATCH: Duration = Duration::from_secs(30);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

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
    let mut config = Config::new(options.name, options.dir);
    config.ip = options.ip;
    config.port = options.port;
    config.rescan_every = options.watch;
    config.thumbnails = options.thumbnails;
    config.transcode = options.transcode.clone();
    let verbose = options.verbose;
    config.on_event = Some(Box::new(move |event| match event {
        Event::Request {
            peer,
            method,
            path,
            range,
        } => {
            if verbose {
                let range = range.map_or(String::new(), |r| format!("  [{r}]"));
                println!("{peer} {method} {path}{range}");
            }
        }
        Event::Converted(item) => {
            println!("audio converted, plays with sound now: {}", item.title);
        }
        Event::LibraryChanged(lib) => {
            println!(
                "library changed: {} folders, {} files",
                lib.folders, lib.files
            );
        }
        Event::Streaming {
            peer,
            item,
            converted,
            from,
        } => {
            let how = if converted { " (audio converted)" } else { "" };
            let from = if from.is_zero() {
                String::new()
            } else {
                format!(" from {}", clock(from))
            };
            println!("{peer} plays {}{how}{from}", item.title);
        }
    }));
    let srv = Server::start(config)?;

    let lib = srv.library();
    println!(
        "{}: {} folders, {} files under {}",
        srv.name,
        lib.folders,
        lib.files,
        srv.root.display()
    );
    if lib.files == 0 {
        eprintln!("warning: no playable media found, the server will look empty on the TV");
    }
    println!("listening on {}  (uuid:{})", srv.base_url(), srv.uuid);
    match options.watch {
        Some(interval) => println!("rescanning every {}s", interval.as_secs()),
        None => println!("rescan on demand at {}/rescan", srv.base_url()),
    }
    if srv.thumbnails().is_some_and(|t| !t.ffmpeg_found()) {
        eprintln!("warning: no ffmpeg in PATH, only a .jpg next to a video shows as its thumbnail");
    }
    if options.transcode.is_some() && srv.transcoder().is_none() {
        eprintln!("warning: no ffmpeg and ffprobe in PATH, a DTS or TrueHD track plays mute");
    }
    println!("announcing over SSDP; pick it up from the TV's media player");
    let lans: Vec<net::Interface> = interfaces.into_iter().filter(|i| !i.vm).collect();
    if options.ip.is_none() && lans.len() > 1 {
        println!("several interfaces are up; if the TV is on another one, pass --ip <ADDR>:");
        print!("{}", interface_table(&lans, Some(srv.ip), "(active)"));
    }

    net::on_terminate(on_signal);
    while !SHUTDOWN.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }

    println!("\nshutting down, withdrawing announcements");
    srv.shutdown();
    Ok(())
}

fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

extern "C" fn on_signal(_signal: i32) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// `--list`: every interface up, the one the server would pick on its own
/// marked, and the VM bridges it never picks marked too.
fn list_interfaces() -> Result<(), String> {
    let interfaces = net::interfaces().map_err(|err| format!("listing interfaces: {err}"))?;
    if interfaces.is_empty() {
        return Err("no LAN interface is up".to_string());
    }
    let picked = net::pick_ip(&interfaces).ok();
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

struct Options {
    dir: PathBuf,
    name: String,
    port: u16,
    ip: Option<Ipv4Addr>,
    watch: Option<Duration>,
    thumbnails: Option<PathBuf>,
    transcode: Option<PathBuf>,
    verbose: bool,
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
        // Generated frames go under the system temp directory, never next to
        // the videos.
        let cache = std::env::temp_dir().join("dlna-server");
        let mut thumbnails = Some(cache.clone());
        let mut transcode = Some(cache);
        let mut verbose = false;

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
                "--no-thumbs" => thumbnails = None,
                "--no-transcode" => transcode = None,
                "-v" | "--verbose" => verbose = true,
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
            thumbnails,
            transcode,
            verbose,
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
        --no-thumbs      Serve no thumbnails (default: a .jpg next to the
                         video, else a frame pulled by ffmpeg when installed)
        --no-transcode   Serve every file as it is (default: a file whose audio
                         a TV cannot decode, DTS and the like, is converted to
                         AC-3 in the background; the video is never touched)
    -v, --verbose        Print every HTTP request, to watch what a TV does
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

    #[test]
    fn table_aligns_columns_and_marks_active_and_vm() {
        let ifs = [
            lan("en0", [192, 168, 1, 69]),
            Interface {
                vm: true,
                ..lan("bridge100", [192, 168, 139, 3])
            },
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
