//! Networking bits that `std` does not expose (socket reuse), plus route lookup.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// Address of the interface that reaches the default route. A UDP `connect`
/// only picks a route, so nothing is sent on the wire.
pub fn local_ipv4() -> io::Result<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.connect((Ipv4Addr::new(1, 1, 1, 1), 80))?;
    match sock.local_addr()? {
        SocketAddr::V4(a) => Ok(*a.ip()),
        SocketAddr::V6(_) => Err(io::Error::other("no IPv4 route")),
    }
}

/// A UDP socket bound with `SO_REUSEADDR | SO_REUSEPORT`, which SSDP needs:
/// port 1900 is shared with every other UPnP daemon on the machine.
#[cfg(unix)]
pub fn bind_reuse_udp(addr: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    use std::ffi::c_void;
    use std::os::fd::FromRawFd;

    unsafe {
        let fd = sys::socket(sys::AF_INET, sys::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let one: i32 = 1;
        let val = &one as *const i32 as *const c_void;
        let len = size_of::<i32>() as u32;
        sys::setsockopt(fd, sys::SOL_SOCKET, sys::SO_REUSEADDR, val, len);
        sys::setsockopt(fd, sys::SOL_SOCKET, sys::SO_REUSEPORT, val, len);

        let sa = sockaddr_in(addr, port);
        if sys::bind(fd, sa.as_ptr(), sa.len() as u32) < 0 {
            let err = io::Error::last_os_error();
            sys::close(fd);
            return Err(err);
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

#[cfg(not(unix))]
pub fn bind_reuse_udp(addr: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    UdpSocket::bind((addr, port))
}

#[cfg(unix)]
pub fn on_terminate(handler: extern "C" fn(i32)) {
    unsafe {
        sys::signal(sys::SIGINT, handler as usize);
        sys::signal(sys::SIGTERM, handler as usize);
    }
}

#[cfg(not(unix))]
pub fn on_terminate(_handler: extern "C" fn(i32)) {}

#[cfg(unix)]
fn sockaddr_in(addr: Ipv4Addr, port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    let port = port.to_be_bytes();
    // BSD keeps a length byte where Linux starts the 16-bit family field.
    #[cfg(target_vendor = "apple")]
    {
        sa[0] = 16;
        sa[1] = sys::AF_INET as u8;
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        sa[0] = sys::AF_INET as u8;
    }
    sa[2] = port[0];
    sa[3] = port[1];
    sa[4..8].copy_from_slice(&addr.octets());
    sa
}

#[cfg(unix)]
mod sys {
    use std::ffi::c_void;

    pub const AF_INET: i32 = 2;
    pub const SOCK_DGRAM: i32 = 2;
    pub const SIGINT: i32 = 2;
    pub const SIGTERM: i32 = 15;

    #[cfg(target_vendor = "apple")]
    pub const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_vendor = "apple")]
    pub const SO_REUSEADDR: i32 = 0x0004;
    #[cfg(target_vendor = "apple")]
    pub const SO_REUSEPORT: i32 = 0x0200;

    #[cfg(not(target_vendor = "apple"))]
    pub const SOL_SOCKET: i32 = 1;
    #[cfg(not(target_vendor = "apple"))]
    pub const SO_REUSEADDR: i32 = 2;
    #[cfg(not(target_vendor = "apple"))]
    pub const SO_REUSEPORT: i32 = 15;

    unsafe extern "C" {
        pub fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
        pub fn setsockopt(fd: i32, level: i32, name: i32, value: *const c_void, len: u32) -> i32;
        pub fn bind(fd: i32, addr: *const u8, len: u32) -> i32;
        pub fn close(fd: i32) -> i32;
        pub fn signal(sig: i32, handler: usize) -> usize;
    }
}
