//! The network adapters as .NET's `NetworkInterface.GetAllNetworkInterfaces()` reports them to the C# agent:
//! Linux `/sys/class/net` + `getifaddrs` + `/proc/net/route`, macOS `getifaddrs` (link-level `if_data`), Windows
//! `GetAdaptersAddresses` + `GetIfEntry2`.

use std::net::Ipv4Addr;

/// The `NetworkInterfaceType` values the C# code tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Ethernet,
    GigabitEthernet,
    Wireless80211,
    Loopback,
    Tunnel,
    Ppp,
    Unknown,
}

/// One adapter: `Id` (the counters' key), `Name`, `Description` (the name on Linux and macOS), type, whether it is
/// `OperationalStatus.Up`, its byte counters (None when they cannot be read), IPv4 unicast addresses and whether it has
/// an IPv4 gateway.
#[derive(Debug, Clone)]
pub struct NetAdapter {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: AdapterKind,
    pub up: bool,
    pub bytes: Option<(i64, i64)>,
    pub ipv4: Vec<Ipv4Addr>,
    pub has_ipv4_gateway: bool,
}

pub fn all() -> Vec<NetAdapter> {
    os::all()
}

/// Name, then for each address family entry: IPv4 address, link-level type/flags/counters (macOS).
#[cfg(unix)]
struct IfAddr {
    name: String,
    ipv4: Option<Ipv4Addr>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    link: Option<(u8, u32, i64, i64)>, // (sdl_type, flags, rx, tx)
}

/// getifaddrs in its own order: the order .NET lists the interfaces in.
#[cfg(unix)]
fn if_addrs() -> Vec<IfAddr> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list we free with freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut p = list;
    while !p.is_null() {
        // SAFETY: `p` walks the list getifaddrs returned; every field is valid until freeifaddrs.
        let ifa = unsafe { &*p };
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let mut entry = IfAddr {
            name,
            ipv4: None,
            link: None,
        };
        if !ifa.ifa_addr.is_null() {
            let family = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
            if family == libc::AF_INET {
                // SAFETY: an AF_INET address is a sockaddr_in.
                let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                entry.ipv4 = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
            }
            #[cfg(target_os = "macos")]
            if family == libc::AF_LINK && !ifa.ifa_data.is_null() {
                // SAFETY: an AF_LINK address is a sockaddr_dl and its ifa_data an if_data.
                let dl = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_dl) };
                let data = unsafe { &*(ifa.ifa_data as *const libc::if_data) };
                entry.link = Some((
                    dl.sdl_type,
                    ifa.ifa_flags,
                    i64::from(data.ifi_ibytes),
                    i64::from(data.ifi_obytes),
                ));
            }
        }
        out.push(entry);
        p = ifa.ifa_next;
    }
    // SAFETY: frees the list getifaddrs allocated, once.
    unsafe { libc::freeifaddrs(list) };
    out
}

/// Interface names in first-appearance order with their IPv4 addresses.
#[cfg(unix)]
fn group(entries: &[IfAddr]) -> Vec<(String, Vec<Ipv4Addr>)> {
    let mut out: Vec<(String, Vec<Ipv4Addr>)> = Vec::new();
    for e in entries {
        let i = match out.iter().position(|(n, _)| *n == e.name) {
            Some(i) => i,
            None => {
                out.push((e.name.clone(), Vec::new()));
                out.len() - 1
            }
        };
        if let Some(ip) = e.ipv4
            && !out[i].1.contains(&ip)
        {
            out[i].1.push(ip);
        }
    }
    out
}

#[cfg(target_os = "linux")]
mod os {
    use super::*;

    /// .NET's mapping of the ARPHRD hardware type.
    pub fn kind_of(arphrd: u32) -> AdapterKind {
        match arphrd {
            1 => AdapterKind::Ethernet,
            772 => AdapterKind::Loopback,
            801..=803 => AdapterKind::Wireless80211,
            768 | 769 => AdapterKind::Tunnel,
            512 => AdapterKind::Ppp,
            _ => AdapterKind::Unknown,
        }
    }

    /// Interfaces with an IPv4 gateway in /proc/net/route (RTF_GATEWAY set, gateway not 0.0.0.0).
    pub fn gateway_interfaces(route: &str) -> Vec<String> {
        route
            .lines()
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
                (flags & 0x2 != 0 && *f.get(2)? != "00000000").then(|| f[0].to_string())
            })
            .collect()
    }

    pub fn all() -> Vec<NetAdapter> {
        let gateways =
            gateway_interfaces(&std::fs::read_to_string("/proc/net/route").unwrap_or_default());
        group(&if_addrs())
            .into_iter()
            .map(|(name, ipv4)| {
                let sys = std::path::Path::new("/sys/class/net").join(&name);
                let read = |f: &str| {
                    std::fs::read_to_string(sys.join(f))
                        .ok()
                        .map(|s| s.trim().to_string())
                };
                let num = |f: &str| read(f).and_then(|s| s.parse::<i64>().ok());
                let bytes = num("statistics/rx_bytes").zip(num("statistics/tx_bytes"));
                NetAdapter {
                    id: name.clone(),
                    description: name.clone(),
                    kind: read("type")
                        .and_then(|t| t.parse().ok())
                        .map_or(AdapterKind::Unknown, kind_of),
                    up: read("operstate").as_deref() == Some("up"),
                    bytes,
                    has_ipv4_gateway: gateways.contains(&name),
                    ipv4,
                    name,
                }
            })
            .collect()
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;

    /// .NET's mapping of the link-level `sdl_type` (if_types.h).
    pub fn kind_of(sdl_type: u8) -> AdapterKind {
        match sdl_type {
            6 => AdapterKind::Ethernet,
            24 => AdapterKind::Loopback,
            71 => AdapterKind::Wireless80211,
            55 | 57 => AdapterKind::Tunnel,
            23 => AdapterKind::Ppp,
            _ => AdapterKind::Unknown,
        }
    }

    pub fn all() -> Vec<NetAdapter> {
        let entries = if_addrs();
        group(&entries)
            .into_iter()
            .map(|(name, ipv4)| {
                let link = entries.iter().find(|e| e.name == name).and_then(|_| {
                    entries
                        .iter()
                        .filter(|e| e.name == name)
                        .find_map(|e| e.link)
                });
                NetAdapter {
                    id: name.clone(),
                    description: name.clone(),
                    kind: link.map_or(AdapterKind::Unknown, |l| kind_of(l.0)),
                    up: link.is_some_and(|l| l.1 & libc::IFF_RUNNING as u32 != 0),
                    bytes: link.map(|l| (l.2, l.3)),
                    has_ipv4_gateway: false,
                    ipv4,
                    name,
                }
            })
            .collect()
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, GetIfEntry2, IP_ADAPTER_ADDRESSES_LH,
        MIB_IF_ROW2,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    /// `NetworkInterfaceType` is the IANA ifType itself.
    pub fn kind_of(if_type: u32) -> AdapterKind {
        match if_type {
            6 => AdapterKind::Ethernet,
            117 => AdapterKind::GigabitEthernet,
            71 => AdapterKind::Wireless80211,
            24 => AdapterKind::Loopback,
            131 => AdapterKind::Tunnel,
            23 => AdapterKind::Ppp,
            _ => AdapterKind::Unknown,
        }
    }

    pub fn all() -> Vec<NetAdapter> {
        let mut size = 16 * 1024u32;
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..3 {
            buf = vec![0u64; (size as usize).div_ceil(8)];
            // SAFETY: `buf` holds `size` bytes, 8-byte aligned as IP_ADAPTER_ADDRESSES requires.
            let rc = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC.0),
                    GAA_FLAG_INCLUDE_GATEWAYS,
                    None,
                    Some(buf.as_mut_ptr().cast()),
                    &mut size,
                )
            };
            match rc {
                0 => break,
                111 => continue, // ERROR_BUFFER_OVERFLOW: `size` now says how much is needed
                _ => return Vec::new(),
            }
        }
        let mut out = Vec::new();
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            // SAFETY: `p` walks the list GetAdaptersAddresses wrote into `buf`, which outlives this loop.
            let a = unsafe { &*p };
            let wide = |w: windows::core::PWSTR| unsafe { w.to_string() }.unwrap_or_default();
            let mut ipv4 = Vec::new();
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = unsafe { &*u };
                let sa = ua.Address.lpSockaddr;
                if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                    let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                    ipv4.push(Ipv4Addr::from(u32::from_be(unsafe {
                        sin.sin_addr.S_un.S_addr
                    })));
                }
                u = ua.Next;
            }
            let mut has_gateway = false;
            let mut g = a.FirstGatewayAddress;
            while !g.is_null() {
                let ga = unsafe { &*g };
                let sa = ga.Address.lpSockaddr;
                has_gateway |= !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET;
                g = ga.Next;
            }
            let mut row = MIB_IF_ROW2 {
                InterfaceLuid: a.Luid,
                ..Default::default()
            };
            // SAFETY: `row` names the adapter by its LUID, as GetIfEntry2 requires.
            let bytes = (unsafe { GetIfEntry2(&mut row) }.0 == 0)
                .then_some((row.InOctets as i64, row.OutOctets as i64));
            out.push(NetAdapter {
                id: unsafe { a.AdapterName.to_string() }.unwrap_or_default(),
                name: wide(a.FriendlyName),
                description: wide(a.Description),
                kind: kind_of(a.IfType),
                up: a.OperStatus.0 == 1,
                bytes,
                ipv4,
                has_ipv4_gateway: has_gateway,
            });
            p = a.Next;
        }
        out
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod os {
    use super::*;

    pub fn all() -> Vec<NetAdapter> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_has_a_loopback_and_named_adapters() {
        let list = all();
        assert!(
            list.iter().any(|a| a.kind == AdapterKind::Loopback),
            "{list:?}"
        );
        for a in &list {
            assert!(!a.id.is_empty() && !a.name.is_empty(), "{a:?}");
            if !cfg!(windows) {
                assert_eq!(a.description, a.name);
            }
        }
        assert!(
            list.iter()
                .any(|a| a.up && a.kind != AdapterKind::Loopback && !a.ipv4.is_empty()),
            "{list:?}"
        );
    }
}
