//! GENA eventing: a control point subscribes to a service with a callback URL,
//! and the server posts a `NOTIFY` there each time an evented variable moves.
//! A TV that has a folder open learns that way that SystemUpdateID changed;
//! without it, it keeps the listing, and the URLs, it browsed first.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::upnp;

/// What the server grants, whatever the subscriber asks for.
pub const TIMEOUT: Duration = Duration::from_secs(1800);
/// A subscriber that is off or gone must not hold the sender up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    ContentDirectory,
    ConnectionManager,
}

impl Service {
    /// The service behind an `eventSubURL` of the device description.
    pub fn from_path(path: &str) -> Option<Service> {
        match path {
            "/evt/cd" => Some(Service::ContentDirectory),
            "/evt/cm" => Some(Service::ConnectionManager),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Url {
    host: String,
    port: u16,
    path: String,
}

struct Subscriber {
    sid: String,
    service: Service,
    callbacks: Vec<Url>,
    /// The SEQ of the next event; 0 is the initial one.
    seq: u32,
    expires: Instant,
}

/// One event to post: where, under which SID and SEQ, and the propertyset.
pub struct Delivery {
    callbacks: Vec<Url>,
    sid: String,
    seq: u32,
    body: String,
}

pub struct Subscriptions {
    subscribers: Mutex<Vec<Subscriber>>,
    counter: AtomicU64,
}

impl Subscriptions {
    pub fn new() -> Subscriptions {
        Subscriptions {
            subscribers: Mutex::new(Vec::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// A new subscription, `None` when the `CALLBACK` header holds no HTTP
    /// URL. The SID comes back with the initial event, SEQ 0, which the
    /// caller posts once the SUBSCRIBE answer is out.
    pub fn subscribe(&self, service: Service, callback: &str, state: String) -> Option<Delivery> {
        let callbacks = parse_callbacks(callback);
        if callbacks.is_empty() {
            return None;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        let sid = format!(
            "uuid:{}",
            upnp::stable_uuid(&format!("{}|{nanos}|{count}", std::process::id()))
        );
        let now = Instant::now();
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|s| s.expires > now);
        subscribers.push(Subscriber {
            sid: sid.clone(),
            service,
            callbacks: callbacks.clone(),
            seq: 1,
            expires: now + TIMEOUT,
        });
        Some(Delivery {
            callbacks,
            sid,
            seq: 0,
            body: state,
        })
    }

    /// Extends a live subscription; false for an unknown or expired SID.
    pub fn renew(&self, sid: &str) -> bool {
        let now = Instant::now();
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|s| s.expires > now);
        match subscribers.iter_mut().find(|s| s.sid == sid) {
            Some(subscriber) => {
                subscriber.expires = now + TIMEOUT;
                true
            }
            None => false,
        }
    }

    /// False for an unknown SID.
    pub fn unsubscribe(&self, sid: &str) -> bool {
        let mut subscribers = self.subscribers.lock().unwrap();
        let before = subscribers.len();
        subscribers.retain(|s| s.sid != sid);
        subscribers.len() != before
    }

    /// Posts `body` to every live subscriber of `service`, on a thread of its
    /// own so the caller does not wait on the network.
    pub fn publish(&self, service: Service, body: &str) {
        let now = Instant::now();
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|s| s.expires > now);
        let deliveries: Vec<Delivery> = subscribers
            .iter_mut()
            .filter(|s| s.service == service)
            .map(|s| {
                let seq = s.seq;
                // SEQ wraps to 1: 0 belongs to the initial event.
                s.seq = s.seq.checked_add(1).unwrap_or(1);
                Delivery {
                    callbacks: s.callbacks.clone(),
                    sid: s.sid.clone(),
                    seq,
                    body: body.to_string(),
                }
            })
            .collect();
        drop(subscribers);
        if !deliveries.is_empty() {
            thread::spawn(move || {
                for delivery in deliveries {
                    delivery.send();
                }
            });
        }
    }
}

impl Delivery {
    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// Tries each callback URL in turn until one takes the event. A
    /// subscriber that takes none is left to expire.
    pub fn send(&self) {
        for url in &self.callbacks {
            if post(url, &self.sid, self.seq, &self.body).is_ok() {
                return;
            }
        }
    }
}

/// `<e:propertyset>` with one `<e:property>` per variable.
pub fn propertyset(variables: &[(&str, String)]) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">"#,
    );
    for (name, value) in variables {
        xml.push_str(&format!(
            "<e:property><{name}>{}</{name}></e:property>",
            upnp::escape(value)
        ));
    }
    xml.push_str("</e:propertyset>");
    xml
}

fn post(url: &Url, sid: &str, seq: u32, body: &str) -> io::Result<()> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "callback host"))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    let request = format!(
        "NOTIFY {path} HTTP/1.1\r\n\
         HOST: {host}:{port}\r\n\
         CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         CONTENT-LENGTH: {length}\r\n\
         NT: upnp:event\r\n\
         NTS: upnp:propchange\r\n\
         SID: {sid}\r\n\
         SEQ: {seq}\r\n\
         CONNECTION: close\r\n\r\n{body}",
        path = url.path,
        host = url.host,
        port = url.port,
        length = body.len(),
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut status = String::new();
    BufReader::new(&stream).read_line(&mut status)?;
    match status.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        _ => Err(io::Error::other(format!(
            "NOTIFY answered {}",
            status.trim()
        ))),
    }
}

/// `<http://a/b><http://c/d>` into the URLs, in order. Anything but plain
/// HTTP is skipped.
fn parse_callbacks(header: &str) -> Vec<Url> {
    header
        .split('<')
        .filter_map(|part| part.split_once('>'))
        .filter_map(|(url, _)| parse_url(url.trim()))
        .collect()
}

fn parse_url(url: &str) -> Option<Url> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    (!host.is_empty()).then(|| Url {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, TcpListener};

    use super::{Service, Subscriptions, Url, parse_callbacks, propertyset};
    use crate::test_support::receive_notify as receive;

    #[test]
    fn a_callback_header_yields_every_http_url_in_order() {
        let urls = parse_callbacks("<http://192.168.1.20:4004/evt/1><ftp://x/><http://tv/cb>");
        assert_eq!(
            urls,
            vec![
                Url {
                    host: "192.168.1.20".into(),
                    port: 4004,
                    path: "/evt/1".into()
                },
                Url {
                    host: "tv".into(),
                    port: 80,
                    path: "/cb".into()
                },
            ]
        );
        assert!(parse_callbacks("http://no-brackets/").is_empty());
    }

    #[test]
    fn renew_and_unsubscribe_know_only_live_sids() {
        let subs = Subscriptions::new();
        let first = subs
            .subscribe(
                Service::ContentDirectory,
                "<http://127.0.0.1:9/>",
                String::new(),
            )
            .expect("a subscription");
        assert!(subs.renew(first.sid()));
        assert!(!subs.renew("uuid:unknown"));
        assert!(subs.unsubscribe(first.sid()));
        assert!(!subs.unsubscribe(first.sid()));
        assert!(!subs.renew(first.sid()));
        assert!(
            subs.subscribe(Service::ContentDirectory, "", String::new())
                .is_none()
        );
    }

    #[test]
    fn events_reach_the_subscribers_of_their_service_in_sequence() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let callback = format!("<http://{}/evt>", listener.local_addr().unwrap());
        let subs = Subscriptions::new();
        let initial = subs
            .subscribe(
                Service::ContentDirectory,
                &callback,
                propertyset(&[("SystemUpdateID", "1".into())]),
            )
            .unwrap();
        initial.send();
        let (seq, body) = receive(&listener);
        assert_eq!(seq, "0");
        assert!(
            body.contains("<SystemUpdateID>1</SystemUpdateID>"),
            "{body}"
        );

        subs.publish(
            Service::ConnectionManager,
            &propertyset(&[("CurrentConnectionIDs", "0".into())]),
        );
        subs.publish(
            Service::ContentDirectory,
            &propertyset(&[("SystemUpdateID", "2".into())]),
        );
        let (seq, body) = receive(&listener);
        assert_eq!(seq, "1", "the ConnectionManager event is not for us");
        assert!(
            body.contains("<SystemUpdateID>2</SystemUpdateID>"),
            "{body}"
        );
    }
}
