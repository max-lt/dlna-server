# dlna-server

A DLNA (UPnP AV) media server. Point it at a folder, browse it from the TV.

No dependencies, `std` alone. SSDP discovery, the ContentDirectory SOAP service
and HTTP range streaming are all implemented here, which is why the release
binary is 442 KB and links nothing but libSystem.

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
        --ip <ADDR>      LAN address to advertise (default: autodetected)
    -h, --help           Print this help
```

The server shows up in the TV's media player within a few seconds. On webOS it
appears in the Media Player app's device list. `Ctrl-C` withdraws the
advertisement, so it disappears again straight away.

Files added after startup are picked up by `curl http://<addr>:<port>/rescan`.
That URL and the status page at `/` are the whole management interface.

## What it does

- **Discovery.** Answers `M-SEARCH` probes and multicasts `ssdp:alive` every ten
  minutes, withdrawing with `ssdp:byebye` on exit.
- **Browsing.** `ContentDirectory:1` `Browse` over folders and files, with
  pagination. Folders holding no playable media are pruned from the tree.
- **Streaming.** HTTP `Range` requests, including the suffix and open-ended
  forms, so seeking and resume work. Files are streamed in 128 KB chunks.
- **Subtitles.** A `.srt` next to a video is offered as a second `<res>` and via
  `sec:CaptionInfoEx`, which LG and Samsung sets read.

There is no transcoding: the renderer gets the original bytes. That covers most
containers an LG set will meet, but an exotic codec fails to play rather than
being converted.

## Layout

| File | Contents |
| --- | --- |
| `src/main.rs` | CLI, shared `Server` state, startup and shutdown |
| `src/ssdp.rs` | discovery: `M-SEARCH` replies and `NOTIFY` announcements |
| `src/http.rs` | HTTP/1.1 server, routing, range streaming |
| `src/upnp.rs` | device description, SCPDs, SOAP and DIDL-Lite |
| `src/library.rs` | filesystem scan into numeric UPnP object IDs |
| `src/net.rs` | route lookup, `SO_REUSEPORT` binding, signal handling |

## Notes

macOS asks to allow incoming connections on first run. Denying it is the usual
reason a DLNA server stays invisible to the TV: the daemon runs fine and answers
nothing.

The advertised address is taken from the default route. With a VM bridge or a
VPN in play, pass `--ip` to pin the interface the TV is actually on.
