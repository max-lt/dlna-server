//! UPnP/DLNA payloads: device description, SCPDs, and the SOAP control endpoints.

use crate::Server;
use crate::gena::{self, Service};
use crate::library::{Item, Kind};
use crate::{FNV_OFFSET, fnv1a};
use crate::{thumbs, transcode};

/// `DLNA.ORG_OP=01` advertises byte-seek support, which is what makes a TV's
/// scrub bar work; the flags word marks the resource as streamable.
pub const RES_FLAGS: &str = "DLNA.ORG_OP=01;DLNA.ORG_CI=0;\
DLNA.ORG_FLAGS=01700000000000000000000000000000";

pub const SERVER_HEADER: &str =
    concat!("UPnP/1.0 DLNA/1.50 dlna-server/", env!("CARGO_PKG_VERSION"));

pub const CONTENT_DIRECTORY: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
pub const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

const DIDL_OPEN: &str = concat!(
    r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" "#,
    r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
    r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
    r#"xmlns:dlna="urn:schemas-dlna-org:metadata-1-0/" "#,
    r#"xmlns:sec="http://www.sec.co.kr/">"#
);

/// The root device document a control point fetches from `LOCATION`.
pub fn description(srv: &Server) -> String {
    let name = escape(&srv.name);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>dlna-server</manufacturer>
    <manufacturerURL>https://github.com/</manufacturerURL>
    <modelName>dlna-server</modelName>
    <modelNumber>{version}</modelNumber>
    <modelDescription>DLNA media server in Rust</modelDescription>
    <serialNumber>{uuid}</serialNumber>
    <UDN>uuid:{uuid}</UDN>
    <presentationURL>/</presentationURL>
    <dlna:X_DLNADOC xmlns:dlna="urn:schemas-dlna-org:device-1-0">DMS-1.50</dlna:X_DLNADOC>
    <serviceList>
      <service>
        <serviceType>{CONTENT_DIRECTORY}</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <SCPDURL>/scpd/cd.xml</SCPDURL>
        <controlURL>/ctl/cd</controlURL>
        <eventSubURL>/evt/cd</eventSubURL>
      </service>
      <service>
        <serviceType>{CONNECTION_MANAGER}</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/scpd/cm.xml</SCPDURL>
        <controlURL>/ctl/cm</controlURL>
        <eventSubURL>/evt/cm</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>
"#,
        uuid = srv.uuid,
        version = env!("CARGO_PKG_VERSION")
    )
}

pub const CD_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>Browse</name>
      <argumentList>
        <argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
        <argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>
        <argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>
        <argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>
        <argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>
        <argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>
        <argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
        <argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetSearchCapabilities</name>
      <argumentList><argument><name>SearchCaps</name><direction>out</direction><relatedStateVariable>SearchCapabilities</relatedStateVariable></argument></argumentList>
    </action>
    <action>
      <name>GetSortCapabilities</name>
      <argumentList><argument><name>SortCaps</name><direction>out</direction><relatedStateVariable>SortCapabilities</relatedStateVariable></argument></argumentList>
    </action>
    <action>
      <name>GetSystemUpdateID</name>
      <argumentList><argument><name>Id</name><direction>out</direction><relatedStateVariable>SystemUpdateID</relatedStateVariable></argument></argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType>
      <allowedValueList><allowedValue>BrowseMetadata</allowedValue><allowedValue>BrowseDirectChildren</allowedValue></allowedValueList>
    </stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>SearchCapabilities</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>SortCapabilities</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SystemUpdateID</name><dataType>ui4</dataType></stateVariable>
  </serviceStateTable>
</scpd>
"#;

pub const CM_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>GetProtocolInfo</name>
      <argumentList>
        <argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
        <argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetCurrentConnectionIDs</name>
      <argumentList><argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument></argumentList>
    </action>
    <action>
      <name>GetCurrentConnectionInfo</name>
      <argumentList>
        <argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>
        <argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>
        <argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>
        <argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>
        <argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>
        <argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>
  </serviceStateTable>
</scpd>
"#;

/// Every evented variable of `service`, for the initial event a new
/// subscriber gets.
pub fn evented_state(srv: &Server, service: Service) -> String {
    match service {
        Service::ContentDirectory => {
            gena::propertyset(&[("SystemUpdateID", srv.update_id().to_string())])
        }
        Service::ConnectionManager => gena::propertyset(&[
            ("SourceProtocolInfo", protocol_info(srv)),
            ("SinkProtocolInfo", String::new()),
            ("CurrentConnectionIDs", "0".to_string()),
        ]),
    }
}

/// Returns the HTTP status and body for one SOAP call.
pub fn control(
    srv: &Server,
    service: &str,
    soap_action: Option<&str>,
    body: &[u8],
) -> (u16, String) {
    let action = soap_action
        .and_then(|a| a.rsplit_once('#'))
        .map(|(_, name)| name.trim_end_matches('"'))
        .unwrap_or("");
    let body = String::from_utf8_lossy(body);

    match (service, action) {
        (CONTENT_DIRECTORY, "Browse") => browse(srv, &body),
        (CONTENT_DIRECTORY, "GetSearchCapabilities") => {
            (200, envelope(service, action, "<SearchCaps></SearchCaps>"))
        }
        (CONTENT_DIRECTORY, "GetSortCapabilities") => (
            200,
            envelope(service, action, "<SortCaps>dc:title</SortCaps>"),
        ),
        (CONTENT_DIRECTORY, "GetSystemUpdateID") => {
            let id = srv.update_id();
            (200, envelope(service, action, &format!("<Id>{id}</Id>")))
        }
        (CONNECTION_MANAGER, "GetProtocolInfo") => (
            200,
            envelope(
                service,
                action,
                &format!(
                    "<Source>{}</Source><Sink></Sink>",
                    escape(&protocol_info(srv))
                ),
            ),
        ),
        (CONNECTION_MANAGER, "GetCurrentConnectionIDs") => (
            200,
            envelope(service, action, "<ConnectionIDs>0</ConnectionIDs>"),
        ),
        (CONNECTION_MANAGER, "GetCurrentConnectionInfo") => (
            200,
            envelope(
                service,
                action,
                "<RcsID>-1</RcsID><AVTransportID>-1</AVTransportID><ProtocolInfo></ProtocolInfo>\
                 <PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID>\
                 <Direction>Output</Direction><Status>OK</Status>",
            ),
        ),
        _ => fault(401, "Invalid Action"),
    }
}

fn browse(srv: &Server, body: &str) -> (u16, String) {
    let object_id: u32 = tag(body, "ObjectID")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let metadata_only = tag(body, "BrowseFlag") == Some("BrowseMetadata");
    let start: usize = tag(body, "StartingIndex")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let requested: usize = tag(body, "RequestedCount")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let lib = srv.library();
    let Some(item) = lib.get(object_id) else {
        return fault(701, "No such object");
    };

    let mut didl = String::from(DIDL_OPEN);
    let (returned, total) = if metadata_only {
        didl.push_str(&render(srv, item));
        (1, 1)
    } else {
        let total = item.children.len();
        // A count of zero means "everything from StartingIndex on".
        let count = if requested == 0 { total } else { requested };
        let mut returned = 0;
        for &child in item.children.iter().skip(start).take(count) {
            if let Some(child) = lib.get(child) {
                didl.push_str(&render(srv, child));
                returned += 1;
            }
        }
        (returned, total)
    };
    didl.push_str("</DIDL-Lite>");

    let payload = format!(
        "<Result>{}</Result><NumberReturned>{returned}</NumberReturned>\
         <TotalMatches>{total}</TotalMatches><UpdateID>{update}</UpdateID>",
        escape(&didl),
        update = srv.update_id()
    );
    (200, envelope(CONTENT_DIRECTORY, "Browse", &payload))
}

/// One DIDL-Lite `<container>` or `<item>` element.
fn render(srv: &Server, item: &Item) -> String {
    // The root container reports a parent of -1 per the ContentDirectory spec.
    let parent = if item.id == 0 {
        "-1".to_string()
    } else {
        item.parent.to_string()
    };
    let title = escape(&item.title);
    let class = item.kind.upnp_class();

    if item.kind == Kind::Folder {
        return format!(
            r#"<container id="{}" parentID="{parent}" restricted="1" childCount="{}" searchable="0">"#,
            item.id,
            item.children.len()
        ) + &format!(
            "<dc:title>{title}</dc:title><upnp:class>{class}</upnp:class></container>"
        );
    }

    let base = srv.base_url();
    let mut out = format!(
        r#"<item id="{}" parentID="{parent}" restricted="1"><dc:title>{title}</dc:title>"#,
        item.id
    );
    out.push_str(&format!("<upnp:class>{class}</upnp:class>"));

    let thumbnail = srv
        .thumbnails()
        .filter(|t| t.offered(item))
        .map(|_| format!("{base}/thumb/{}.jpg", item.id));
    if let Some(url) = &thumbnail {
        out.push_str(&format!(
            r#"<upnp:albumArtURI dlna:profileID="{}">{url}</upnp:albumArtURI>"#,
            thumbs::PROFILE
        ));
    }
    if item.subtitle.is_some() {
        // Samsung's extension, which LG's webOS player also honours.
        out.push_str(&format!(
            r#"<sec:CaptionInfoEx sec:type="srt">{base}/sub/{id}.srt</sec:CaptionInfoEx>"#,
            id = item.id
        ));
    }

    // The media first: a TV plays the first <res> it can, and a JPEG counts.
    // A converted file stands in for the original rather than beside it:
    // given both, an LG set went for the original and played it mute.
    // Some renderers sniff the URL extension before reading the MIME type.
    let (mime, size, url) = match srv.transcoder().and_then(|t| t.ready(item)) {
        Some(converted) => (
            transcode::MIME,
            std::fs::metadata(&converted).map_or(0, |m| m.len()),
            format!("{base}/transcode/{}.{}", item.id, transcode::EXTENSION),
        ),
        None => {
            let ext = item.path.extension().map_or_else(String::new, |ext| {
                format!(".{}", ext.to_string_lossy().to_lowercase())
            });
            (
                item.mime,
                item.size,
                format!("{base}/media/{}{ext}", item.id),
            )
        }
    };
    out.push_str(&format!(
        r#"<res protocolInfo="http-get:*:{mime}:{RES_FLAGS}" size="{size}">{url}</res>"#
    ));
    if item.subtitle.is_some() {
        out.push_str(&format!(
            r#"<res protocolInfo="http-get:*:text/srt:*">{base}/sub/{id}.srt</res>"#,
            id = item.id
        ));
    }
    if let Some(url) = &thumbnail {
        out.push_str(&format!(
            r#"<res protocolInfo="http-get:*:image/jpeg:DLNA.ORG_PN={}">{url}</res>"#,
            thumbs::PROFILE
        ));
    }
    out.push_str("</item>");
    out
}

/// Every MIME type actually present, so a picky renderer sees a matching entry.
fn protocol_info(srv: &Server) -> String {
    let lib = srv.library();
    let mut mimes: Vec<&str> = lib
        .items
        .values()
        .filter(|item| item.kind != Kind::Folder)
        .map(|item| item.mime)
        .collect();
    mimes.sort_unstable();
    mimes.dedup();
    if mimes.is_empty() {
        mimes.push("video/mp4");
    }
    let mut entries: Vec<String> = mimes
        .iter()
        .map(|m| format!("http-get:*:{m}:{RES_FLAGS}"))
        .collect();
    if srv.thumbnails().is_some() {
        entries.push(format!(
            "http-get:*:image/jpeg:DLNA.ORG_PN={}",
            thumbs::PROFILE
        ));
    }
    entries.join(",")
}

fn envelope(service: &str, action: &str, payload: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:{action}Response xmlns:u="{service}">{payload}</u:{action}Response></s:Body></s:Envelope>
"#
    )
}

fn fault(code: u32, description: &str) -> (u16, String) {
    (
        500,
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>
<detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
<errorCode>{code}</errorCode><errorDescription>{description}</errorDescription>
</UPnPError></detail></s:Fault></s:Body></s:Envelope>
"#
        ),
    )
}

/// Reads `<Name>value</Name>`, ignoring any namespace prefix on the tag.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("{name}>");
    let rest = &xml[xml.find(&open)? + open.len()..];
    Some(rest[..rest.find("</")?].trim())
}

/// Escapes `text` for XML, dropping the C0 controls, which XML 1.0 forbids
/// outright: a filename may legally carry one, and a parser rejects the whole
/// DIDL over it rather than the one item.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if xml_allows(c) => out.push(c),
            _ => {}
        }
    }
    out
}

/// The `Char` production of XML 1.0. A Rust `char` cannot hold a surrogate, so
/// that gap in the ranges needs no guard.
fn xml_allows(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | ' '..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)
}

/// Derived from the server's identity, so a restart reuses the TV's existing
/// entry instead of adding a duplicate.
pub fn stable_uuid(seed: &str) -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&fnv1a(FNV_OFFSET, seed.as_bytes()).to_be_bytes());
    bytes[8..].copy_from_slice(&fnv1a(0x9e37_79b9_7f4a_7c15, seed.as_bytes()).to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{browse, escape, stable_uuid, tag};
    use crate::test_support::TempDir;
    use crate::{Config, Server};

    fn server_over(tmp: &TempDir) -> Server {
        Server::build(
            Config::new("Test", tmp.path().to_path_buf()),
            Ipv4Addr::LOCALHOST,
            8200,
        )
    }

    /// A root holding `clip0.mp4` .. `clip4.mp4`.
    fn five_clips() -> (TempDir, Server) {
        let tmp = TempDir::new();
        for n in 0..5 {
            tmp.file(&format!("clip{n}.mp4"), 1);
        }
        let srv = server_over(&tmp);
        (tmp, srv)
    }

    fn request(object_id: u32, flag: &str, start: usize, count: usize) -> String {
        format!(
            "<u:Browse><ObjectID>{object_id}</ObjectID><BrowseFlag>{flag}</BrowseFlag>\
             <Filter>*</Filter><StartingIndex>{start}</StartingIndex>\
             <RequestedCount>{count}</RequestedCount><SortCriteria></SortCriteria></u:Browse>"
        )
    }

    #[test]
    fn tag_ignores_namespace_prefixes() {
        assert_eq!(tag("<u:ObjectID>42</u:ObjectID>", "ObjectID"), Some("42"));
        assert_eq!(tag("<ObjectID> 7 </ObjectID>", "ObjectID"), Some("7"));
        assert_eq!(tag("<Other>1</Other>", "ObjectID"), None);
    }

    #[test]
    fn titles_are_xml_safe() {
        assert_eq!(escape("Fish & Chips <2>"), "Fish &amp; Chips &lt;2&gt;");
    }

    #[test]
    fn characters_xml_cannot_carry_are_dropped() {
        assert_eq!(escape("bad\u{1}name\u{8}"), "badname");
        assert_eq!(escape("\u{b}\u{c}\u{1f}\u{fffe}\u{ffff}"), "");
        assert_eq!(escape("tab\tlf\ncr\r"), "tab\tlf\ncr\r");
        assert_eq!(escape("café — naïve 🎬"), "café — naïve 🎬");
    }

    #[test]
    fn uuid_is_stable_and_well_formed() {
        let uuid = stable_uuid("Rust DLNA|/Users/max/Movies");
        assert_eq!(uuid, stable_uuid("Rust DLNA|/Users/max/Movies"));
        assert_ne!(uuid, stable_uuid("Rust DLNA|/Users/max/Films"));
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
    }

    #[test]
    fn browse_pages_through_a_container() {
        let (_tmp, srv) = five_clips();
        let (status, page) = browse(&srv, &request(0, "BrowseDirectChildren", 1, 2));

        assert_eq!(status, 200);
        assert!(page.contains("<NumberReturned>2</NumberReturned>"));
        assert!(page.contains("<TotalMatches>5</TotalMatches>"));
    }

    #[test]
    fn a_requested_count_of_zero_means_everything_left() {
        let (_tmp, srv) = five_clips();
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 3, 0));

        assert!(page.contains("<NumberReturned>2</NumberReturned>"));
        assert!(page.contains("<TotalMatches>5</TotalMatches>"));
    }

    #[test]
    fn a_start_past_the_end_still_reports_the_total() {
        let (_tmp, srv) = five_clips();
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 99, 10));

        assert!(page.contains("<NumberReturned>0</NumberReturned>"));
        assert!(page.contains("<TotalMatches>5</TotalMatches>"));
    }

    #[test]
    fn browse_metadata_returns_the_object_itself() {
        let (_tmp, srv) = five_clips();
        let (status, page) = browse(&srv, &request(0, "BrowseMetadata", 0, 0));

        assert_eq!(status, 200);
        assert!(page.contains("<NumberReturned>1</NumberReturned>"));
        assert!(page.contains("<TotalMatches>1</TotalMatches>"));
        // The ContentDirectory spec has the root report a parent of -1; the DIDL
        // is escaped into <Result>, so its attribute quotes arrive as entities.
        assert!(page.contains("parentID=&quot;-1&quot;"));
    }

    #[test]
    fn browsing_an_object_that_does_not_exist_faults() {
        let (_tmp, srv) = five_clips();
        let (status, body) = browse(&srv, &request(9999, "BrowseDirectChildren", 0, 0));

        assert_eq!(status, 500);
        assert!(body.contains("<errorCode>701</errorCode>"));
    }

    #[test]
    fn a_video_with_a_sidecar_offers_the_subtitle_twice() {
        let tmp = TempDir::new();
        tmp.file("film.mkv", 1);
        tmp.file("film.srt", 1);
        let srv = server_over(&tmp);
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 0, 0));

        // Once as sec:CaptionInfoEx, which LG and Samsung read, and once as a
        // second <res> for everything else.
        assert!(page.contains("sec:CaptionInfoEx"));
        assert_eq!(page.matches("/sub/").count(), 2);
    }

    #[test]
    fn a_poster_is_offered_as_album_art_and_as_a_res() {
        let tmp = TempDir::new();
        tmp.file("film.mkv", 1);
        tmp.file("film.jpg", 1);
        let mut config = Config::new("Test", tmp.path().to_path_buf());
        config.thumbnails = Some(tmp.path().join("cache"));
        let srv = Server::build(config, Ipv4Addr::LOCALHOST, 8200);
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 0, 0));

        assert!(page.contains("upnp:albumArtURI"));
        assert_eq!(page.matches("/thumb/1.jpg").count(), 2);
        assert!(page.contains("<TotalMatches>1</TotalMatches>"));
    }

    /// A TV plays the first <res> it can, and a JPEG counts: a thumbnail
    /// listed first turned every film into a still on an LG set.
    #[test]
    fn the_video_is_the_first_res() {
        let tmp = TempDir::new();
        tmp.file("film.mkv", 1);
        tmp.file("film.srt", 1);
        tmp.file("film.jpg", 1);
        let mut config = Config::new("Test", tmp.path().to_path_buf());
        config.thumbnails = Some(tmp.path().join("cache"));
        let srv = Server::build(config, Ipv4Addr::LOCALHOST, 8200);
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 0, 0));

        let res: Vec<usize> = ["/media/", "/sub/1.srt&lt;/res", "/thumb/1.jpg&lt;/res"]
            .iter()
            .map(|marker| page.find(marker).expect(marker))
            .collect();
        assert!(res[0] < res[1] && res[1] < res[2], "{res:?}");
    }

    /// Runs only where ffmpeg is installed: once converted, a DTS clip is
    /// offered as the converted file, with its real size, and not as it is.
    #[test]
    fn a_dts_clip_is_offered_converted_only() {
        let tmp = TempDir::new();
        if crate::test_support::dts_clip(tmp.path()).is_none() {
            eprintln!("no ffmpeg in PATH, skipping");
            return;
        }
        let mut config = Config::new("Test", tmp.path().to_path_buf());
        config.transcode = Some(tmp.path().join("cache"));
        let srv = Server::build(config, Ipv4Addr::LOCALHOST, 8200);
        let transcoder = srv.transcoder().expect("ffmpeg is there");
        let (_, before) = browse(&srv, &request(0, "BrowseDirectChildren", 0, 0));
        assert!(
            before.contains("/media/1.mkv"),
            "the original, until converted"
        );

        transcoder.work(&srv.library(), &|| false, &|_| {});
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 0, 0));
        assert!(page.contains("/transcode/1.mkv"));
        assert!(
            !page.contains("/media/1.mkv"),
            "the original would be picked and play mute"
        );
        assert!(page.contains("DLNA.ORG_OP=01"));
        assert!(
            !page.contains("size=&quot;0&quot;"),
            "the converted file's own size"
        );
    }

    #[test]
    fn thumbnails_off_leaves_the_didl_alone() {
        let tmp = TempDir::new();
        tmp.file("film.mkv", 1);
        tmp.file("film.jpg", 1);
        let srv = server_over(&tmp);
        let (_, page) = browse(&srv, &request(0, "BrowseDirectChildren", 0, 0));

        assert!(!page.contains("albumArtURI"));
        assert!(!page.contains("/thumb/"));
    }
}
