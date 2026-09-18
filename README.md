# dlna-server

A DLNA (UPnP AV) media server. Point it at a folder, browse it from the TV.

No dependencies, `std` alone. SSDP discovery, the ContentDirectory SOAP service
and HTTP range streaming are all implemented here, which is why the release
binary is small and links nothing but libSystem. The one outside program it
uses is `ffmpeg`, when installed, to pull thumbnails and to convert audio a
TV cannot decode.

The crate is a library with a command line on top; see "As a library" for
embedding it in something else.

## Usage

```sh
cargo build --release
./target/release/dlna-server ~/Movies --name "Living Room"
```

```
USAGE:
    dlna-server [DIR] [OPTIONS]

ARGS:
    DIR                  Folder to share (default: $HOME/Movies)

OPTIONS:
    -p, --port <PORT>    HTTP port (default: 8200)
    -n, --name <NAME>    Name shown on the TV (default: "Rust DLNA")
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
```

The server shows up in the TV's media player within a few seconds. On webOS it
appears in the Media Player app's device list. `Ctrl-C` withdraws the
advertisement, so it disappears again straight away.

Files added or removed while it runs are picked up within the rescan interval.
`curl http://<addr>:<port>/rescan` forces one immediately; that URL and the
status page at `/` are the whole management interface.

## What it does

- **Discovery.** Answers `M-SEARCH` probes and multicasts `ssdp:alive` every ten
  minutes, withdrawing with `ssdp:byebye` on exit. `BOOTID.UPNP.ORG` carries the
  start time, so a TV can tell a restart from a server that never went away —
  the UUID alone cannot say, being stable on purpose.
- **Browsing.** `ContentDirectory:1` `Browse` over folders and files, with
  pagination. Folders holding no playable media are pruned from the tree.
- **Streaming.** HTTP `Range` requests, including the suffix and open-ended
  forms, so seeking and resume work. Files are streamed in 128 KB chunks.
- **Subtitles.** A `.srt` next to a video is offered as a second `<res>` and via
  `sec:CaptionInfoEx`, which LG and Samsung sets read.
- **Thumbnails.** A `.jpg` sharing the video's stem is its poster, as Kodi,
  Plex and Jellyfin have it, and is not listed as a photo. Without one, and
  with `ffmpeg` in `PATH`, a frame is pulled two minutes in (the first frame
  of a shorter clip), scaled to fit 1024x768 and cached under the system temp
  directory; nothing is ever written next to the videos. Either way the item
  carries a `upnp:albumArtURI` and a `JPEG_MED` `<res>`, which is what a TV's
  grid reads. Frames are made on first request, one `ffmpeg` at a time.
- **Audio conversion.** Most sets have no decoder for DTS, TrueHD, Opus or
  Vorbis and play such a file mute. With `ffmpeg` and `ffprobe` in `PATH`,
  every video is probed on a background thread, and one whose audio is on
  that list is converted there and then: the video and subtitles copied, the
  track re-encoded to AC-3, into a complete Matroska file under the system
  temp directory. A 7 GB film takes about twenty seconds on an SSD. Once it
  is there the item offers the converted file in place of the original, with
  its real size and byte ranges, exactly like any other file, and
  `SystemUpdateID` moves so the TV browses again. Until then the original is
  offered as it is. A stream converted on the fly was tried first and does
  not work: a TV probes a file by byte offsets before it plays, and nothing
  but a real file answers those.

The video is never transcoded: the renderer gets the original picture bytes.
A codec it cannot decode fails to play rather than being converted; only the
audio tracks listed above are.

## Layout

| File | Contents |
| --- | --- |
| `src/lib.rs` | `Config`, `Server`: start, rescan, shutdown |
| `src/main.rs` | the command line: options, banner, Ctrl-C |
| `src/ssdp.rs` | discovery: `M-SEARCH` replies and `NOTIFY` announcements |
| `src/http.rs` | HTTP/1.1 server, routing, range streaming |
| `src/upnp.rs` | device description, SCPDs, SOAP and DIDL-Lite |
| `src/library.rs` | filesystem scan into numeric UPnP object IDs |
| `src/thumbs.rs` | thumbnails: the poster sidecar, the `ffmpeg` frame, the cache |
| `src/transcode.rs` | audio conversion: the `ffprobe` pass, the `ffmpeg` run, the cache |
| `src/net.rs` | route lookup, interface list, `SO_REUSEPORT` binding, signal handling |

Thumbnails touch the rest of the code in three places: the `thumbnail` field
on an item, the `albumArtURI` in the DIDL, and the `/thumb/` route. Setting
`Config::thumbnails` to `None` turns all three off. Audio conversion likewise:
the converting worker, the `<res>` in the DIDL, and the `/transcode/`
route, all behind `Config::transcode`.

## As a library

```rust
use dlna_server::{Config, Event, Server};

let mut config = Config::new("Living Room", "/Users/max/Movies".into());
config.rescan_every = Some(std::time::Duration::from_secs(30));
config.thumbnails = Some(std::env::temp_dir().join("dlna-server"));
config.transcode = Some(std::env::temp_dir().join("dlna-server"));
config.on_event = Some(Box::new(|event| match event {
    Event::LibraryChanged(lib) => println!("{} files", lib.files),
    Event::Streaming { peer, item, .. } => println!("{peer} plays {}", item.title),
    Event::Converted(item) => println!("{} now plays with sound", item.title),
    Event::Request { .. } => {}
}));

let srv = Server::start(config)?;
println!("serving at {}", srv.base_url());
// ... later, from any thread:
srv.shutdown();
```

`start` binds the port, scans, and returns once discovery is running. `rescan`
can be called at any time. `on_event` fires from whichever thread has the
news: the timer's or the caller's for a library change, an HTTP thread for a
renderer starting to read. `shutdown` withdraws the SSDP announcement and
joins every worker, so a new `start` on the same port succeeds straight
after. Errors on the way go to stderr; nothing is written to stdout.

## Notes

macOS asks to allow incoming connections on first run. Denying it is the usual
reason a DLNA server stays invisible to the TV: the daemon runs fine and answers
nothing.

The advertised address is the default route's, as long as that is a LAN and
not a VPN tunnel. A TV on another network never sees the announcement, so when
more than one interface is up the banner lists them, the active one marked;
`--list` shows them all, and `--ip` pins the one the TV is on. The bridges VM
and container tools create (`bridgeNNN` on macOS; `docker0`, `br-*`, `virbr0`,
`vmnet*` on Linux) are told by name, never picked and left out of the banner;
`--list` still shows them, marked.

A converted file is as big as the original and lives in the temp directory,
which the system clears in its own time; a replaced original gets a fresh
conversion, keyed on path, size and mtime. `shutdown` kills a conversion
under way and leaves no half-written file behind.

A generated thumbnail is keyed on the video's path, size and mtime, so a
replaced file gets a fresh one; the old file stays in the temp directory until
the system clears it.

Change detection is a `stat` sweep, not FSEvents: no `unsafe` FFI, and it works
the same on Linux. The sweep is served from the VFS cache in the normal case,
but on a library kept on an external disk that is allowed to spin down, prefer
`--watch 0` and the `/rescan` endpoint.
