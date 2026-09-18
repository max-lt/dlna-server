//! A blocking HTTP/1.1 server: one thread per connection, one request each.
//!
//! Range support is what makes a TV's scrub bar work; some renderers refuse to
//! start playback without it.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::Server;
use crate::library::Kind;
use crate::{thumbs, upnp};

const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Generous: a paused renderer can sit on a stalled socket for a while.
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);
const CHUNK: usize = 128 * 1024;
/// Blank lines tolerated before a request line, so a confused client cannot
/// hold a thread open forever.
const MAX_LEADING_BLANK_LINES: usize = 8;

const XML: &str = "text/xml; charset=\"utf-8\"";
const TEXT: &str = "text/plain; charset=utf-8";

#[derive(Clone, Copy, PartialEq)]
enum Body {
    Send,
    Omit,
}

enum Resource {
    Media,
    Subtitle,
    Thumbnail,
}

/// A client's `Range` header resolved against the size of the file.
enum Range {
    Whole,
    Part { start: u64, end: u64 },
    Unsatisfiable,
}

pub struct Request {
    pub method: String,
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub fn serve(listener: TcpListener, srv: Arc<Server>) -> JoinHandle<()> {
    thread::spawn(move || {
        for stream in listener.incoming() {
            // `shutdown` connects once so that this is looked at.
            if srv.stopping() {
                break;
            }
            let Ok(stream) = stream else {
                continue;
            };
            let srv = Arc::clone(&srv);
            thread::spawn(move || {
                let peer = stream
                    .peer_addr()
                    .map_or_else(|_| "?".to_string(), |a| a.ip().to_string());
                if let Err(err) = handle(stream, &srv) {
                    // A renderer that skips ahead or stops just drops the socket.
                    if !matches!(
                        err.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::WouldBlock
                    ) {
                        eprintln!("http: {peer}: {err}");
                    }
                }
            });
        }
    })
}

fn handle(mut stream: TcpStream, srv: &Server) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_nodelay(true)?;

    let req = read_request(&stream)?;
    let body = if req.method == "HEAD" {
        Body::Omit
    } else {
        Body::Send
    };
    let path = req.path.split(['?', '#']).next().unwrap_or("/").to_string();

    match (req.method.as_str(), path.as_str()) {
        ("GET" | "HEAD", "/") => {
            let page = status_page(srv);
            let html = "text/html; charset=utf-8";
            send(&mut stream, 200, html, &[], page.as_bytes(), body)
        }
        ("GET" | "HEAD", "/desc.xml") => {
            let xml = upnp::description(srv);
            send(&mut stream, 200, XML, &[], xml.as_bytes(), body)
        }
        ("GET" | "HEAD", "/scpd/cd.xml") => {
            send(&mut stream, 200, XML, &[], upnp::CD_SCPD.as_bytes(), body)
        }
        ("GET" | "HEAD", "/scpd/cm.xml") => {
            send(&mut stream, 200, XML, &[], upnp::CM_SCPD.as_bytes(), body)
        }
        ("POST", "/ctl/cd") | ("POST", "/ctl/cm") => {
            let service = if path.ends_with("cd") {
                upnp::CONTENT_DIRECTORY
            } else {
                upnp::CONNECTION_MANAGER
            };
            let (status, xml) = upnp::control(srv, service, req.header("SOAPACTION"), &req.body);
            send(
                &mut stream,
                status,
                XML,
                &[("EXT", "")],
                xml.as_bytes(),
                Body::Send,
            )
        }
        // Eventing is advertised but carries nothing; the subscription only
        // needs acknowledging.
        ("SUBSCRIBE" | "UNSUBSCRIBE", p) if p.starts_with("/evt/") => {
            let sid = format!("uuid:{}", srv.uuid);
            let headers = [("SID", sid.as_str()), ("TIMEOUT", "Second-1800")];
            send(&mut stream, 200, TEXT, &headers, b"", Body::Send)
        }
        ("GET" | "HEAD", "/rescan") => {
            let (lib, changed) = srv.rescan();
            let state = if changed { "updated" } else { "unchanged" };
            let report = format!("{state}: {} folders, {} files\n", lib.folders, lib.files);
            send(&mut stream, 200, TEXT, &[], report.as_bytes(), body)
        }
        ("GET" | "HEAD", p) if p.starts_with("/media/") => {
            let slug = &p["/media/".len()..];
            serve_object(&mut stream, &req, srv, slug, Resource::Media, body)
        }
        ("GET" | "HEAD", p) if p.starts_with("/sub/") => {
            let slug = &p["/sub/".len()..];
            serve_object(&mut stream, &req, srv, slug, Resource::Subtitle, body)
        }
        ("GET" | "HEAD", p) if p.starts_with("/thumb/") => {
            let slug = &p["/thumb/".len()..];
            serve_object(&mut stream, &req, srv, slug, Resource::Thumbnail, body)
        }
        _ => send(&mut stream, 404, TEXT, &[], b"not found\n", body),
    }
}

/// Serves `/media/<id>.<ext>`, or the subtitle or thumbnail of the same object.
fn serve_object(
    stream: &mut TcpStream,
    req: &Request,
    srv: &Server,
    slug: &str,
    resource: Resource,
    body: Body,
) -> io::Result<()> {
    let Some(id) = slug.split('.').next().and_then(|s| s.parse().ok()) else {
        return send(stream, 400, TEXT, &[], b"bad object id\n", body);
    };
    let lib = srv.library();
    let Some(item) = lib.get(id).filter(|i| i.kind != Kind::Folder) else {
        return send(stream, 404, TEXT, &[], b"no such object\n", body);
    };

    let (path, mime, mode, features): (PathBuf, &str, &str, String) = match resource {
        Resource::Media => (
            item.path.clone(),
            item.mime,
            "Streaming",
            upnp::RES_FLAGS.to_string(),
        ),
        Resource::Subtitle => match &item.subtitle {
            Some(subtitle) => (
                subtitle.clone(),
                "text/srt",
                "Background",
                upnp::RES_FLAGS.to_string(),
            ),
            None => return send(stream, 404, TEXT, &[], b"no subtitle\n", body),
        },
        Resource::Thumbnail => match srv.thumbnails().and_then(|t| t.fetch(item)) {
            Some(jpeg) => (
                jpeg,
                "image/jpeg",
                "Interactive",
                format!("DLNA.ORG_PN={}", thumbs::PROFILE),
            ),
            None => return send(stream, 404, TEXT, &[], b"no thumbnail\n", body),
        },
    };
    let dlna = [
        ("transferMode.dlna.org", mode),
        ("contentFeatures.dlna.org", features.as_str()),
    ];
    serve_file(stream, req, &path, mime, &dlna, body)
}

fn serve_file(
    stream: &mut TcpStream,
    req: &Request,
    path: &Path,
    mime: &str,
    extra: &[(&str, &str)],
    body: Body,
) -> io::Result<()> {
    let Ok(mut file) = File::open(path) else {
        return send(stream, 404, TEXT, &[], b"not found\n", body);
    };
    let total = file.metadata()?.len();

    let (status, start, length) = match resolve_range(req.header("Range"), total) {
        Range::Whole => (200, 0, total),
        Range::Part { start, end } => (206, start, end - start + 1),
        Range::Unsatisfiable => {
            let range = format!("bytes */{total}");
            let headers = [("Content-Range", range.as_str())];
            return send(stream, 416, TEXT, &headers, b"", body);
        }
    };

    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", mime.to_string()),
        ("Content-Length", length.to_string()),
        ("Accept-Ranges", "bytes".to_string()),
        ("Connection", "close".to_string()),
    ];
    for (name, value) in extra {
        headers.push((name, value.to_string()));
    }
    if status == 206 {
        let end = start + length - 1;
        headers.push(("Content-Range", format!("bytes {start}-{end}/{total}")));
    }

    write_head(stream, status, &headers)?;
    if body == Body::Omit || length == 0 {
        return stream.flush();
    }

    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; CHUNK];
    let mut remaining = length;
    while remaining > 0 {
        let want = remaining.min(CHUNK as u64) as usize;
        let read = file.read(&mut buf[..want])?;
        if read == 0 {
            break; // The file shrank underneath us.
        }
        stream.write_all(&buf[..read])?;
        remaining -= read as u64;
    }
    stream.flush()
}

fn resolve_range(header: Option<&str>, total: u64) -> Range {
    // A range unit we do not recognise must be ignored rather than rejected.
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Range::Whole;
    };
    match bounds(spec, total) {
        Some((start, end)) => Range::Part { start, end },
        None => Range::Unsatisfiable,
    }
}

/// Resolves one byte-range spec against the file size, `None` if unsatisfiable.
fn bounds(spec: &str, total: u64) -> Option<(u64, u64)> {
    // Multi-range requests are legal, but no renderer sends them.
    let (from, to) = spec.split(',').next()?.split_once('-')?;
    let last = total.checked_sub(1)?;

    let (start, end) = match (from.trim(), to.trim()) {
        // `bytes=-N` asks for the final N bytes.
        ("", suffix) => (
            total.saturating_sub(suffix.parse::<u64>().ok()?.max(1)),
            last,
        ),
        (from, "") => (from.parse().ok()?, last),
        (from, to) => (from.parse().ok()?, to.parse::<u64>().ok()?.min(last)),
    };
    (start <= end && start <= last).then_some((start, end))
}

fn read_request(stream: &TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream);

    // A client may legally prefix a request with a stray CRLF, and a probe may
    // close the connection without sending anything at all.
    let mut line = String::new();
    for _ in 0..MAX_LEADING_BLANK_LINES {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before sending a request",
            ));
        }
        if !line.trim().is_empty() {
            break;
        }
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_uppercase();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length.min(1 << 20)];
    if !body.is_empty() {
        reader.read_exact(&mut body)?;
    }

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn send(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
    payload: &[u8],
    body: Body,
) -> io::Result<()> {
    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", content_type.to_string()),
        ("Content-Length", payload.len().to_string()),
        ("Connection", "close".to_string()),
    ];
    for (name, value) in extra {
        headers.push((name, value.to_string()));
    }
    write_head(stream, status, &headers)?;
    if body == Body::Send {
        stream.write_all(payload)?;
    }
    stream.flush()
}

fn write_head(stream: &mut TcpStream, status: u16, headers: &[(&str, String)]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        416 => "Requested Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nServer: {}\r\n",
        upnp::SERVER_HEADER
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn status_page(srv: &Server) -> String {
    let lib = srv.library();
    format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <body style=\"font:15px/1.6 -apple-system,sans-serif;max-width:34rem;margin:3rem auto\">\
         <h1>{name}</h1><p>DLNA media server, up and advertising.</p>\
         <ul><li>Library: <code>{root}</code></li>\
         <li>{folders} folders, {files} files</li>\
         <li>UDN: <code>uuid:{uuid}</code></li>\
         <li><a href=\"/desc.xml\">device description</a> &middot; <a href=\"/rescan\">rescan</a></li></ul>",
        name = upnp::escape(&srv.name),
        root = upnp::escape(&srv.root.display().to_string()),
        folders = lib.folders,
        files = lib.files,
        uuid = srv.uuid,
    )
}

#[cfg(test)]
mod tests {
    use super::{Range, resolve_range};

    const SIZE: u64 = 1000;

    fn part(header: &str) -> Option<(u64, u64)> {
        match resolve_range(Some(header), SIZE) {
            Range::Part { start, end } => Some((start, end)),
            _ => None,
        }
    }

    #[test]
    fn explicit_range() {
        assert_eq!(part("bytes=0-99"), Some((0, 99)));
        assert_eq!(part("bytes=500-999"), Some((500, 999)));
    }

    #[test]
    fn open_ended_range_runs_to_the_last_byte() {
        assert_eq!(part("bytes=500-"), Some((500, 999)));
    }

    #[test]
    fn suffix_range_counts_back_from_the_end() {
        assert_eq!(part("bytes=-100"), Some((900, 999)));
        assert_eq!(part("bytes=-5000"), Some((0, 999)));
    }

    #[test]
    fn end_past_the_file_is_clamped() {
        assert_eq!(part("bytes=900-99999"), Some((900, 999)));
    }

    #[test]
    fn only_the_first_range_of_a_set_is_served() {
        assert_eq!(part("bytes=0-99,200-299"), Some((0, 99)));
    }

    #[test]
    fn a_missing_or_unknown_unit_serves_the_whole_file() {
        assert!(matches!(resolve_range(None, SIZE), Range::Whole));
        assert!(matches!(
            resolve_range(Some("chunks=0-"), SIZE),
            Range::Whole
        ));
    }

    #[test]
    fn unsatisfiable_ranges_are_refused() {
        for header in ["bytes=1000-", "bytes=900-100", "bytes=abc-"] {
            assert!(matches!(
                resolve_range(Some(header), SIZE),
                Range::Unsatisfiable
            ));
        }
        assert!(matches!(
            resolve_range(Some("bytes=0-99"), 0),
            Range::Unsatisfiable
        ));
    }
}
