//! SSDP: the discovery half of DLNA. We answer `M-SEARCH` probes and announce
//! ourselves periodically so a TV finds the server without being asked twice.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Server;
use crate::net;
use crate::upnp::SERVER_HEADER;

const MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;
/// Lifetime we claim for our advertisements, in seconds.
const MAX_AGE: u32 = 1800;
/// Re-announce well inside `MAX_AGE` so an announcement lost to UDP costs nothing.
const REANNOUNCE: Duration = Duration::from_secs(MAX_AGE as u64 / 3);

/// A TV that sees the UUID and the BOOTID both unchanged keeps its cached tree,
/// and our UUID is stable by design. Seconds since the epoch, in the 31 bits
/// UPnP allows.
static BOOT_ID: LazyLock<u32> = LazyLock::new(|| {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| (d.as_secs() as u32) & 0x7fff_ffff)
});

/// The five identities a MediaServer must announce, as `(NT/ST, USN)` pairs.
fn identities(uuid: &str) -> Vec<(String, String)> {
    let device = "urn:schemas-upnp-org:device:MediaServer:1";
    vec![
        (
            "upnp:rootdevice".into(),
            format!("uuid:{uuid}::upnp:rootdevice"),
        ),
        (format!("uuid:{uuid}"), format!("uuid:{uuid}")),
        (device.into(), format!("uuid:{uuid}::{device}")),
        (
            crate::upnp::CONTENT_DIRECTORY.into(),
            format!("uuid:{uuid}::{}", crate::upnp::CONTENT_DIRECTORY),
        ),
        (
            crate::upnp::CONNECTION_MANAGER.into(),
            format!("uuid:{uuid}::{}", crate::upnp::CONNECTION_MANAGER),
        ),
    ]
}

/// Starts the responder and the announcer, each on its own thread.
pub fn start(srv: Arc<Server>) -> io::Result<()> {
    let socket = net::bind_reuse_udp(Ipv4Addr::UNSPECIFIED, PORT)?;
    socket.join_multicast_v4(&MULTICAST, &srv.ip)?;

    let responder = Arc::clone(&srv);
    let listen = socket.try_clone()?;
    thread::spawn(move || {
        if let Err(err) = respond(&listen, &responder) {
            eprintln!("ssdp: responder stopped: {err}");
        }
    });

    thread::spawn(move || {
        loop {
            // Three copies: SSDP is unreliable by design and TVs miss packets.
            for _ in 0..3 {
                if let Err(err) = notify(&srv, "ssdp:alive") {
                    eprintln!("ssdp: announce failed: {err}");
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
            thread::sleep(REANNOUNCE);
        }
    });
    Ok(())
}

/// Withdraws our advertisements, so renderers drop us instead of waiting out
/// `MAX_AGE`.
pub fn byebye(srv: &Server) {
    for _ in 0..2 {
        let _ = notify(srv, "ssdp:byebye");
    }
}

fn respond(socket: &UdpSocket, srv: &Server) -> io::Result<()> {
    let boot = *BOOT_ID;
    let mut buf = [0u8; 2048];
    loop {
        let (len, from) = socket.recv_from(&mut buf)?;
        let msg = String::from_utf8_lossy(&buf[..len]);
        if !msg.starts_with("M-SEARCH") {
            continue;
        }
        let Some(search_target) = header(&msg, "ST") else {
            continue;
        };
        let matches: Vec<(String, String)> = identities(&srv.uuid)
            .into_iter()
            .filter(|(nt, _)| search_target == "ssdp:all" || search_target == nt)
            .collect();
        if matches.is_empty() {
            continue;
        }

        // MX is how long the searcher waits; spreading replies over it keeps a
        // busy network from stampeding.
        let mx = header(&msg, "MX")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(1)
            .clamp(1, 3);
        let delay = Duration::from_millis(jitter(mx * 1000));

        let socket = socket.try_clone()?;
        let location = format!("{}/desc.xml", srv.base_url());
        thread::spawn(move || {
            thread::sleep(delay);
            for (st, usn) in matches {
                let reply = format!(
                    "HTTP/1.1 200 OK\r\n\
                     CACHE-CONTROL: max-age={MAX_AGE}\r\n\
                     EXT:\r\n\
                     LOCATION: {location}\r\n\
                     SERVER: {SERVER_HEADER}\r\n\
                     ST: {st}\r\n\
                     USN: {usn}\r\n\
                     BOOTID.UPNP.ORG: {boot}\r\n\
                     CONFIGID.UPNP.ORG: 1\r\n\r\n"
                );
                let _ = socket.send_to(reply.as_bytes(), from);
            }
        });
    }
}

fn notify(srv: &Server, nts: &str) -> io::Result<()> {
    // Binding to the LAN address pins the outgoing interface, which matters as
    // soon as a VM bridge or VPN adds a second route.
    let socket = UdpSocket::bind(SocketAddrV4::new(srv.ip, 0))?;
    let target = SocketAddrV4::new(MULTICAST, PORT);
    let location = format!("{}/desc.xml", srv.base_url());

    let boot = *BOOT_ID;

    for (nt, usn) in identities(&srv.uuid) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: {MULTICAST}:{PORT}\r\n\
             CACHE-CONTROL: max-age={MAX_AGE}\r\n\
             LOCATION: {location}\r\n\
             NT: {nt}\r\n\
             NTS: {nts}\r\n\
             SERVER: {SERVER_HEADER}\r\n\
             USN: {usn}\r\n\
             BOOTID.UPNP.ORG: {boot}\r\n\
             CONFIGID.UPNP.ORG: 1\r\n\r\n"
        );
        socket.send_to(msg.as_bytes(), target)?;
    }
    Ok(())
}

fn header<'a>(msg: &'a str, name: &str) -> Option<&'a str> {
    msg.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Clock-derived spread; SSDP only needs replies scattered, not unpredictable.
fn jitter(max_ms: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % max_ms.max(1)
}
