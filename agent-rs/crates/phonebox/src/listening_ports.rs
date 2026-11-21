//! Port of `ListeningPorts.cs`: the TCP ports one process listens on that are reachable through 127.0.0.1 (or ::1).
//! Used to find local servers that pick their port at run time and don't print it on their command line. Every
//! failure yields an empty list.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Default number of ports returned, so a busy process can't use up a caller's time budget.
pub const DEFAULT_MAX: usize = 8;

/// A listening port and the loopback address to reach it on: 127.0.0.1 for an IPv4 listener, ::1 for an IPv6 one. An
/// IPv6 socket may refuse IPv4 (on Windows it does by default), so the family the process chose is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Listener {
    pub address: IpAddr,
    pub port: u16,
}

/// The loopback (or all-interfaces) TCP listening ports of `pid`, lowest first; two sockets on one port (127.0.0.1
/// and [::1]) are different servers, so both stay, IPv4 first.
pub fn for_process(pid: u32, max: usize) -> Vec<Listener> {
    if pid == 0 || max == 0 {
        return vec![];
    }
    let mut found = os_listeners(pid);
    found.sort_by_key(|l| (l.port, l.address.is_ipv6()));
    let mut seen = HashSet::new();
    found.retain(|l| seen.insert(*l));
    found.truncate(max);
    found
}

#[cfg(windows)]
fn os_listeners(pid: u32) -> Vec<Listener> {
    let mut found = parse_owner_pid_table(&windows_table::read(AF_INET), false, pid);
    found.extend(parse_owner_pid_table(
        &windows_table::read(AF_INET6),
        true,
        pid,
    ));
    found
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn os_listeners(pid: u32) -> Vec<Listener> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return vec![];
    };
    let inodes: HashSet<String> = fds
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|target| {
            let t = target.to_str()?;
            Some(t.strip_prefix("socket:[")?.strip_suffix(']')?.to_string())
        })
        .collect();
    if inodes.is_empty() {
        return vec![];
    }
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|text| parse_proc_net_tcp(&text, &inodes))
        .collect()
}

#[cfg(target_os = "macos")]
fn os_listeners(pid: u32) -> Vec<Listener> {
    use crate::unix::sh;
    // a launchd PATH often lacks /usr/sbin, where macOS keeps lsof
    let lsof = sh::which("lsof").or_else(|| {
        let p = std::path::PathBuf::from("/usr/sbin/lsof");
        p.is_file().then_some(p)
    });
    let Some(lsof) = lsof else { return vec![] };
    let pid = pid.to_string();
    let args = [
        "-nP",
        "-a",
        "-p",
        pid.as_str(),
        "-iTCP",
        "-sTCP:LISTEN",
        "-Ftn",
    ];
    // lsof exits with 1 when nothing matches; the output decides, not the exit code
    sh::run_any_exit(2000, &lsof.to_string_lossy(), &args)
        .map(|out| parse_lsof_names(&out))
        .unwrap_or_default()
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
fn os_listeners(_pid: u32) -> Vec<Listener> {
    vec![]
}

/// An IPv4-mapped IPv6 address (::ffff:127.0.0.2) as the IPv4 address it stands for.
fn plain(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(a) => a.to_ipv4_mapped().map_or(address, IpAddr::V4),
        v4 => v4,
    }
}

/// Whether a listener on this local address answers connections to the loopback address of its family.
pub fn reachable_on_loopback(address: IpAddr) -> bool {
    let address = plain(address);
    address.is_loopback() || address.is_unspecified()
}

/// Where to reach a listener: its own address when it is bound to a loopback address (127.0.0.2 is one too), the
/// loopback address of its family when it is bound to every interface.
pub fn loopback_for(listener: IpAddr) -> IpAddr {
    let listener = plain(listener);
    if listener.is_loopback() {
        listener
    } else if listener.is_ipv6() {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

/// The address as a URL host: IPv6 in brackets.
pub fn url_host(address: IpAddr) -> String {
    match address {
        IpAddr::V6(a) => format!("[{a}]"),
        IpAddr::V4(a) => a.to_string(),
    }
}

// ---- Windows: GetExtendedTcpTable, never netstat (its state column is localized) ----

#[cfg(windows)]
const AF_INET: u32 = 2;
#[cfg(windows)]
const AF_INET6: u32 = 23;
const MIB_TCP_STATE_LISTEN: i32 = 2;
/// `MIB_TCPROW_OWNER_PID` / `MIB_TCP6ROW_OWNER_PID` sizes.
pub const IPV4_ROW_SIZE: usize = 24;
pub const IPV6_ROW_SIZE: usize = 56;

#[cfg(windows)]
mod windows_table {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER,
    };

    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    /// The listener table of one address family; the table can grow between the size query and the read.
    pub(super) fn read(family: u32) -> Vec<u8> {
        let mut size = 0u32;
        for _ in 0..4 {
            // SAFETY: a null table with a size pointer asks for the size only
            let result = unsafe {
                GetExtendedTcpTable(
                    None,
                    &mut size,
                    false,
                    family,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result != ERROR_INSUFFICIENT_BUFFER || size == 0 || size > 16 << 20 {
                return vec![];
            }
            // u32s keep the rows' DWORDs aligned
            let mut buffer = vec![0u32; (size as usize).div_ceil(4)];
            let mut have = (buffer.len() * 4) as u32;
            // SAFETY: the buffer holds `have` bytes and outlives the call
            let result = unsafe {
                GetExtendedTcpTable(
                    Some(buffer.as_mut_ptr().cast()),
                    &mut have,
                    false,
                    family,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result == 0 {
                let bytes: Vec<u8> = buffer.iter().flat_map(|w| w.to_ne_bytes()).collect();
                return bytes[..(have as usize).min(bytes.len())].to_vec();
            }
            if result != ERROR_INSUFFICIENT_BUFFER {
                return vec![];
            }
            size = have;
        }
        vec![]
    }
}

/// Decodes a `MIB_TCPTABLE_OWNER_PID` or `MIB_TCP6TABLE_OWNER_PID`: a 4-byte row count, then fixed-size rows. Ports
/// are in network byte order in the low 16 bits of their DWORD.
pub fn parse_owner_pid_table(table: &[u8], ipv6: bool, pid: u32) -> Vec<Listener> {
    let mut ports = vec![];
    let Some(count) = table.get(..4) else {
        return ports;
    };
    let count = u32::from_le_bytes(count.try_into().unwrap_or_default()) as usize;
    let row_size = if ipv6 { IPV6_ROW_SIZE } else { IPV4_ROW_SIZE };
    // offsets inside a row (MIB_TCPROW_OWNER_PID / MIB_TCP6ROW_OWNER_PID)
    let (state_at, addr_at, port_at, pid_at) = if ipv6 { (48, 0, 20, 52) } else { (0, 4, 8, 20) };
    let int_at =
        |row: &[u8], at: usize| i32::from_le_bytes(row[at..at + 4].try_into().unwrap_or_default());
    for i in 0..count {
        let start = 4 + i * row_size;
        let Some(row) = table.get(start..start + row_size) else {
            break;
        };
        if int_at(row, pid_at) as u32 != pid || int_at(row, state_at) != MIB_TCP_STATE_LISTEN {
            continue;
        }
        let address = if ipv6 {
            let octets: [u8; 16] = row[addr_at..addr_at + 16].try_into().unwrap_or_default();
            IpAddr::V6(Ipv6Addr::from(octets))
        } else {
            let octets: [u8; 4] = row[addr_at..addr_at + 4].try_into().unwrap_or_default();
            IpAddr::V4(Ipv4Addr::from(octets))
        };
        if !reachable_on_loopback(address) {
            continue;
        }
        let port = u16::from_be_bytes([row[port_at], row[port_at + 1]]);
        if port > 0 {
            ports.push(Listener {
                address: loopback_for(address),
                port,
            });
        }
    }
    ports
}

// ---- Linux: the process's socket inodes, matched against the kernel's LISTEN rows ----

/// Reads `/proc/net/tcp` or `tcp6` text: listening rows (state `0A`) owned by one of the socket inodes, on a loopback
/// or all-interfaces address. Addresses are hex 32-bit words in host (little-endian) order.
pub fn parse_proc_net_tcp(text: &str, inodes: &HashSet<String>) -> Vec<Listener> {
    let mut ports = vec![];
    for line in text.split('\n') {
        let cols: Vec<&str> = line.split(' ').filter(|c| !c.is_empty()).collect();
        if cols.len() < 10 || !cols[0].ends_with(':') {
            continue; // the header row and torn lines
        }
        if cols[3] != "0A" || !inodes.contains(cols[9]) {
            continue;
        }
        let local: Vec<&str> = cols[1].split(':').collect();
        let [addr, port] = local[..] else { continue };
        let Some(address) = proc_address(addr).filter(|a| reachable_on_loopback(*a)) else {
            continue;
        };
        if let Some(port) = hex_number(port).and_then(|p| u16::try_from(p).ok())
            && port > 0
        {
            ports.push(Listener {
                address: loopback_for(address),
                port,
            });
        }
    }
    ports
}

/// A hex number as `NumberStyles.AllowHexSpecifier` reads one: hex digits only (Rust's parser would take a sign too).
fn hex_number(hex: &str) -> Option<u32> {
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

fn proc_address(hex: &str) -> Option<IpAddr> {
    if hex.len() != 8 && hex.len() != 32 {
        return None;
    }
    let mut bytes = vec![0u8; hex.len() / 2];
    for word in 0..hex.len() / 8 {
        for b in 0..4 {
            // each 32-bit word is printed as a number, so its bytes appear in reverse memory order
            let at = word * 8 + (3 - b) * 2;
            bytes[word * 4 + b] = hex_number(hex.get(at..at + 2)?)? as u8;
        }
    }
    Some(match <[u8; 4]>::try_from(&bytes[..]) {
        Ok(v4) => IpAddr::V4(Ipv4Addr::from(v4)),
        Err(_) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[..]).ok()?)),
    })
}

// ---- macOS: lsof, started without a shell ----

/// Reads `lsof -Ftn` output: per file (`f`), a `t` line with the socket type (IPv4 / IPv6) and an `n` line with
/// `host:port`, IPv6 hosts in brackets and `*` for every interface of either family, which is why the type is read
/// too. Other field lines are skipped.
pub fn parse_lsof_names(output: &str) -> Vec<Listener> {
    let mut ports = vec![];
    let mut kind: Option<&str> = None;
    for raw in output.split('\n') {
        let line = raw.trim_end_matches('\r');
        match line.as_bytes().first() {
            Some(b'p' | b'f') => {
                kind = None;
                continue;
            }
            Some(b't') if line.len() > 1 => {
                kind = Some(&line[1..]);
                continue;
            }
            Some(b'n') if line.len() >= 2 => {}
            _ => continue,
        }
        let name = &line[1..];
        let Some(colon) = name.rfind(':').filter(|c| *c > 0) else {
            continue;
        };
        if name.contains("->") {
            continue;
        }
        let host = name[..colon].trim_matches(|c| c == '[' || c == ']');
        let address = if host == "*" {
            Some(if kind == Some("IPv6") {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            })
        } else if host.eq_ignore_ascii_case("localhost") {
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            host.parse::<IpAddr>()
                .ok()
                .filter(|a| reachable_on_loopback(*a))
                .map(loopback_for)
        };
        let digits = &name[colon + 1..];
        let port = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u16>().ok())
            .flatten()
            .filter(|p| *p > 0);
        if let (Some(address), Some(port)) = (address, port) {
            ports.push(Listener { address, port });
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn v4(port: u16) -> Listener {
        Listener {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        }
    }
    fn v6(port: u16) -> Listener {
        Listener {
            address: IpAddr::V6(Ipv6Addr::LOCALHOST),
            port,
        }
    }
    fn at(address: &str, port: u16) -> Listener {
        Listener {
            address: address.parse().unwrap(),
            port,
        }
    }

    /// An IPv6-only socket on ::1 (std's listener on ::1 is already IPv6-only), or None on a host without IPv6.
    fn listen_v6(port: u16) -> Option<TcpListener> {
        TcpListener::bind((Ipv6Addr::LOCALHOST, port)).ok()
    }

    #[test]
    fn finds_a_listener_of_this_process() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // other tests run listeners in this process too, so the default cap could hide this port
        assert!(for_process(std::process::id(), usize::MAX).contains(&v4(port)));
    }

    #[test]
    fn invalid_process_ids_have_no_ports() {
        assert!(for_process(0, DEFAULT_MAX).is_empty());
        assert!(for_process(std::process::id(), 0).is_empty());
    }

    #[test]
    fn result_is_capped_sorted_and_distinct() {
        let _listeners: Vec<TcpListener> = (0..3)
            .map(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap())
            .collect();
        let ports: Vec<u16> = for_process(std::process::id(), 2)
            .iter()
            .map(|l| l.port)
            .collect();
        assert_eq!(ports.len(), 2);
        let mut sorted = ports.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, ports);
    }

    #[test]
    fn proc_net_tcp_keeps_only_own_loopback_listeners() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:F2DD 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000000000000 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 112 1 0000000000000000 100 0 0 10 0
   2: 0100007F:2328 0100007F:F2DD 01 00000000:00000000 00:00000000 00000000  1000        0 114 1 0000000000000000 20 4 30 10 -1
   3: 0A00000A:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 113 1 0000000000000000 100 0 0 10 0
   4: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 999 1 0000000000000000 100 0 0 10 0
   5: 0100007F:ZZZZ 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000000000000 100 0 0 10 0
   6: 0100007F
   7: 0100007F:+FFF 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000000000000 100 0 0 10 0
";
        let ours: HashSet<String> = ["111", "112", "113", "114"].map(String::from).into();
        assert_eq!(parse_proc_net_tcp(tcp, &ours), vec![v4(62173), v4(8080)]);
    }

    #[test]
    fn proc_net_tcp6_reads_loopback_and_any_addresses() {
        let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:F2DE 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 211 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 212 1 0000000000000000 100 0 0 10 0
   2: 000080FE00000000FF005450B6AD1DFE:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 213 1 0000000000000000 100 0 0 10 0
";
        let ours: HashSet<String> = ["211", "212", "213"].map(String::from).into();
        assert_eq!(parse_proc_net_tcp(tcp6, &ours), vec![v6(62174), v6(8082)]);
    }

    #[test]
    fn lsof_names_keep_loopback_and_wildcard_listeners() {
        let output = "p4242\nf12\ntIPv4\nn127.0.0.1:62173\nf13\ntIPv6\nn[::1]:62174\r\nf14\ntIPv4\nn*:8080\n\
                      f15\ntIPv4\nn192.168.1.5:9000\nf16\ntIPv4\nn127.0.0.1:5000->127.0.0.1:6000\nf17\ntIPv4\nnlocalhost:7000\n\
                      f18\ntIPv4\nn127.0.0.1:99999\nf19\ntIPv6\nn[::]:8081\nf20\ntIPv4\nn127.0.0.2:8083\n";
        assert_eq!(
            parse_lsof_names(output),
            vec![
                v4(62173),
                v6(62174),
                v4(8080),
                v4(7000),
                v6(8081),
                at("127.0.0.2", 8083)
            ]
        );
        // macOS lsof prints "*" for an IPv6 wildcard too; the t line of the same file tells the family
        assert_eq!(
            parse_lsof_names("p1\nf3\ntIPv6\nn*:8084\nf4\ntIPv4\nn*:8085\n"),
            vec![v6(8084), v4(8085)]
        );
        assert!(parse_lsof_names("").is_empty());
        assert!(
            parse_lsof_names("f1\ntIPv4\nn127.0.0.1:+80\n").is_empty(),
            "no sign before the port"
        );
    }

    fn table(row_size: usize, rows: &[Vec<u8>]) -> Vec<u8> {
        let mut t = (rows.len() as u32).to_le_bytes().to_vec();
        for r in rows {
            assert_eq!(r.len(), row_size);
            t.extend(r);
        }
        t
    }

    fn row4(state: i32, address: [u8; 4], port: u16, pid: i32) -> Vec<u8> {
        let mut row = vec![0u8; IPV4_ROW_SIZE];
        row[..4].copy_from_slice(&state.to_le_bytes());
        row[4..8].copy_from_slice(&address);
        row[8..10].copy_from_slice(&port.to_be_bytes());
        row[20..24].copy_from_slice(&pid.to_le_bytes());
        row
    }

    fn row6(address: Ipv6Addr, port: u16, pid: i32) -> Vec<u8> {
        let mut row = vec![0u8; IPV6_ROW_SIZE];
        row[..16].copy_from_slice(&address.octets());
        row[20..22].copy_from_slice(&port.to_be_bytes());
        row[48..52].copy_from_slice(&2i32.to_le_bytes());
        row[52..56].copy_from_slice(&pid.to_le_bytes());
        row
    }

    #[test]
    fn windows_owner_pid_tables_keep_only_the_processes_loopback_listeners() {
        let t4 = table(
            IPV4_ROW_SIZE,
            &[
                row4(2, [127, 0, 0, 1], 62173, 77),
                row4(2, [0, 0, 0, 0], 8080, 77),
                row4(2, [127, 0, 0, 1], 8081, 78),
                row4(2, [192, 168, 1, 5], 9000, 77),
                row4(5, [127, 0, 0, 1], 9001, 77),
                row4(2, [127, 0, 0, 2], 9004, 77),
            ],
        );
        // a wildcard listener is reached on 127.0.0.1, a concrete loopback one (127.0.0.2) on its own address
        assert_eq!(
            parse_owner_pid_table(&t4, false, 77),
            vec![v4(62173), v4(8080), at("127.0.0.2", 9004)]
        );

        let t6 = table(
            IPV6_ROW_SIZE,
            &[
                row6(Ipv6Addr::LOCALHOST, 62174, 77),
                row6(Ipv6Addr::UNSPECIFIED, 8082, 77),
                row6("fe80::1".parse().unwrap(), 9002, 77),
                row6(Ipv6Addr::LOCALHOST, 9003, 78),
            ],
        );
        // an IPv6 socket may refuse IPv4 (on Windows it does by default), so it is reached on ::1
        assert_eq!(
            parse_owner_pid_table(&t6, true, 77),
            vec![v6(62174), v6(8082)]
        );

        // a row count larger than the buffer stops at the last whole row instead of reading past it
        let torn = &t4[..4 + IPV4_ROW_SIZE + 10];
        assert_eq!(parse_owner_pid_table(torn, false, 77), vec![v4(62173)]);
        assert!(parse_owner_pid_table(&[1, 0], false, 77).is_empty());
    }

    #[test]
    fn an_ipv6_only_listener_is_reached_on_ipv6_loopback() {
        let Some(socket) = listen_v6(0) else { return }; // nothing to find on a host without IPv6
        let port = socket.local_addr().unwrap().port();
        assert!(for_process(std::process::id(), usize::MAX).contains(&v6(port)));
    }

    #[test]
    fn both_families_on_one_port_are_kept() {
        let ipv4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = ipv4.local_addr().unwrap().port();
        let Some(_ipv6) = listen_v6(port) else { return };
        let found = for_process(std::process::id(), usize::MAX);
        assert!(found.contains(&v4(port)), "{found:?}");
        assert!(found.contains(&v6(port)), "{found:?}");
    }

    #[test]
    fn mapped_addresses_are_read_as_ipv4() {
        for (listener, expected) in [
            ("::ffff:127.0.0.2", "127.0.0.2"),
            ("::ffff:0.0.0.0", "127.0.0.1"),
            ("::1", "::1"),
            ("::", "::1"),
        ] {
            let address: IpAddr = listener.parse().unwrap();
            assert!(reachable_on_loopback(address), "{listener}");
            assert_eq!(
                loopback_for(address),
                expected.parse::<IpAddr>().unwrap(),
                "{listener}"
            );
        }
        assert_eq!(url_host("::1".parse().unwrap()), "[::1]");
        assert_eq!(url_host("127.0.0.1".parse().unwrap()), "127.0.0.1");
    }
}
