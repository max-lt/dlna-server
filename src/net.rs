//! Networking bits that `std` does not expose (socket reuse, the interface
//! list), plus route lookup.

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

/// An interface a renderer could sit behind: up, running and broadcast-capable,
/// which leaves out loopback and tunnels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub addr: Ipv4Addr,
    /// Netmask length, so `192.168.8.103/24` shows the network at a glance.
    pub prefix: u8,
    /// The host end of a VM or container network. A TV is never behind one.
    pub vm: bool,
}

/// Whether the name is one a VM or container tool gives its bridge: vmnet on
/// macOS (OrbStack, Docker Desktop, UTM); Docker, libvirt and VMware on Linux.
/// The dash in `br-` matters: `br0` is what a LAN bridge is usually called.
#[cfg(unix)]
fn vm_bridge(name: &str) -> bool {
    const PREFIXES: &[&str] = if cfg!(target_vendor = "apple") {
        &["bridge"]
    } else {
        &["docker", "br-", "virbr", "vmnet"]
    };
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Every LAN interface holding an IPv4 address, in kernel order.
#[cfg(unix)]
pub fn interfaces() -> io::Result<Vec<Interface>> {
    use std::ffi::CStr;
    use std::ptr;

    const LAN: u32 = sys::IFF_UP | sys::IFF_RUNNING | sys::IFF_BROADCAST;

    let mut head: *mut sys::IfAddrs = ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list that only freeifaddrs
    // releases, and nothing here keeps a pointer into it past that call.
    unsafe {
        if sys::getifaddrs(&mut head) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut found = Vec::new();
        let mut node = head;
        while !node.is_null() {
            let ifa = &*node;
            node = ifa.ifa_next;
            if ifa.ifa_flags & LAN != LAN || ifa.ifa_name.is_null() {
                continue;
            }
            let Some(addr) = sockaddr_ipv4(ifa.ifa_addr) else {
                continue;
            };
            let mask = sockaddr_ipv4(ifa.ifa_netmask).unwrap_or(Ipv4Addr::BROADCAST);
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            found.push(Interface {
                vm: vm_bridge(&name),
                name,
                addr,
                prefix: u32::from(mask).count_ones() as u8,
            });
        }
        sys::freeifaddrs(head);
        Ok(found)
    }
}

#[cfg(not(unix))]
pub fn interfaces() -> io::Result<Vec<Interface>> {
    Ok(Vec::new())
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

/// The address in a `sockaddr`, when it is an IPv4 one. `getifaddrs` leaves
/// the pointer null on an interface without that field.
#[cfg(unix)]
unsafe fn sockaddr_ipv4(sa: *const u8) -> Option<Ipv4Addr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: every sockaddr starts with its family, and an AF_INET one is the
    // 16-byte sockaddr_in that `sockaddr_in` above lays out.
    unsafe {
        let family = if cfg!(target_vendor = "apple") {
            u16::from(*sa.add(1))
        } else {
            u16::from_ne_bytes([*sa, *sa.add(1)])
        };
        if i32::from(family) != sys::AF_INET {
            return None;
        }
        Some(Ipv4Addr::new(
            *sa.add(4),
            *sa.add(5),
            *sa.add(6),
            *sa.add(7),
        ))
    }
}

#[cfg(unix)]
mod sys {
    use std::ffi::{c_char, c_void};

    pub const AF_INET: i32 = 2;
    pub const SOCK_DGRAM: i32 = 2;
    pub const SIGINT: i32 = 2;
    pub const SIGTERM: i32 = 15;
    pub const IFF_UP: u32 = 0x1;
    pub const IFF_BROADCAST: u32 = 0x2;
    pub const IFF_RUNNING: u32 = 0x40;

    /// `struct ifaddrs`, laid out the same on BSD and Linux: the sixth field
    /// is `ifa_dstaddr` on one and the `ifa_ifu` union on the other, a pointer
    /// either way.
    #[repr(C)]
    pub struct IfAddrs {
        pub ifa_next: *mut IfAddrs,
        pub ifa_name: *const c_char,
        pub ifa_flags: u32,
        pub ifa_addr: *const u8,
        pub ifa_netmask: *const u8,
        pub ifa_dstaddr: *const u8,
        pub ifa_data: *mut c_void,
    }

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
        pub fn getifaddrs(head: *mut *mut IfAddrs) -> i32;
        pub fn freeifaddrs(head: *mut IfAddrs);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const ADDR: Ipv4Addr = Ipv4Addr::new(192, 168, 8, 103);

    #[test]
    fn sockaddr_reads_back_what_it_wrote() {
        let sa = sockaddr_in(ADDR, 1900);
        assert_eq!(unsafe { sockaddr_ipv4(sa.as_ptr()) }, Some(ADDR));
    }

    #[test]
    fn sockaddr_skips_other_families_and_null() {
        let mut sa = sockaddr_in(ADDR, 1900);
        // Both layouts keep the family in the first two bytes; zero is AF_UNSPEC.
        sa[0] = 0;
        sa[1] = 0;
        assert_eq!(unsafe { sockaddr_ipv4(sa.as_ptr()) }, None);
        assert_eq!(unsafe { sockaddr_ipv4(std::ptr::null()) }, None);
    }

    #[test]
    fn vm_bridges_are_told_by_name() {
        let (vm, lan): (&[&str], &[&str]) = if cfg!(target_vendor = "apple") {
            (&["bridge100"], &["en0", "en7", "utun0"])
        } else {
            (
                &["docker0", "br-1a2b3c4d5e6f", "virbr0", "vmnet8"],
                &["eth0", "wlp3s0", "br0", "enp0s31f6"],
            )
        };
        for name in vm {
            assert!(vm_bridge(name), "{name} should read as a VM bridge");
        }
        for name in lan {
            assert!(!vm_bridge(name), "{name} should read as a LAN");
        }
    }

    #[test]
    fn interfaces_are_lan_only() {
        for i in interfaces().expect("getifaddrs") {
            assert!(!i.name.is_empty());
            assert!(!i.addr.is_loopback(), "{} is loopback", i.name);
            assert!(i.prefix <= 32);
        }
    }
}
