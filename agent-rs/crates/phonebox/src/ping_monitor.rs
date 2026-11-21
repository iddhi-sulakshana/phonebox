//! Port of `PingMonitor.cs`: internet latency to `ping.host` (1.1.1.1 by default) every 5 s, with the average and the
//! packet loss over the last 60 samples (5 minutes). An empty host turns it off (no packets, no snapshot).

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::config::store::ConfigStore;

/// `PingSnapshot`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingSnapshot {
    pub host: String,
    pub ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub loss_pct: f64,
}

/// One probe every 5 s; the average and the loss cover the last 60 (5 minutes).
const INTERVAL: Duration = Duration::from_secs(5);
const WINDOW: usize = 60;
/// `SendPingAsync(host, 2000)`.
const TIMEOUT: Duration = Duration::from_millis(2000);
/// .NET resolves the host without a limit; this one gives up so a hung resolver cannot stop the probes for good.
const RESOLVE_LIMIT: Duration = Duration::from_secs(5);
/// .NET's default echo payload.
const PAYLOAD: &[u8; 32] = b"abcdefghijklmnopqrstuvwabcdefghi";

#[derive(Default)]
struct Window {
    samples: VecDeque<Option<f64>>,
    snap: Option<PingSnapshot>,
    /// The host the samples belong to; None before the first pass (so the first pass always starts clean).
    host: Option<String>,
}

pub struct PingMonitor {
    cfg: Arc<ConfigStore>,
    window: Mutex<Window>,
}

impl PingMonitor {
    pub fn new(cfg: Arc<ConfigStore>) -> Arc<PingMonitor> {
        Arc::new(PingMonitor {
            cfg,
            window: Mutex::default(),
        })
    }

    fn window(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The configured target ("" = off; config normalization trims it and replaces junk with 1.1.1.1).
    fn current_host(&self) -> String {
        self.cfg.get().ping.host.clone().unwrap_or_default()
    }

    /// The last snapshot, unless the target changed since (the average must not mix two hosts).
    pub fn snapshot(&self) -> Option<PingSnapshot> {
        let snap = self.window().snap.clone()?;
        (snap.host == self.current_host()).then_some(snap)
    }

    /// Adds one probe result (None = lost) for `host` and recomputes the snapshot.
    pub fn sample(&self, ms: Option<f64>, host: &str) {
        let mut w = self.window();
        w.samples.push_back(ms);
        while w.samples.len() > WINDOW {
            w.samples.pop_front();
        }
        let ok: Vec<f64> = w.samples.iter().flatten().copied().collect();
        let count = w.samples.len();
        let avg_ms = (!ok.is_empty()).then(|| {
            crate::system_monitor::round_dotnet(ok.iter().sum::<f64>() / ok.len() as f64, 1)
        });
        let loss_pct = crate::system_monitor::round_dotnet(
            100.0 * (count - ok.len()) as f64 / count as f64,
            1,
        );
        w.snap = Some(PingSnapshot {
            host: host.to_string(),
            ms,
            avg_ms,
            loss_pct,
        });
    }

    /// The probe loop, until `stop`.
    pub fn start(self: &Arc<Self>, stop: crate::server::Stop) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            while !stop.is_stopped() {
                let host = me.current_host();
                {
                    let mut w = me.window();
                    if w.host.as_deref() != Some(host.as_str()) {
                        // a new target (or none): the average and the loss must not mix two hosts
                        w.host = Some(host.clone());
                        w.samples.clear();
                        w.snap = None;
                    }
                }
                if !host.is_empty() {
                    tokio::select! {
                        _ = stop.stopped() => break,
                        ms = ping_once(&host) => me.sample(ms, &host),
                    }
                }
                tokio::select! {
                    _ = stop.stopped() => break,
                    _ = tokio::time::sleep(INTERVAL) => {}
                }
            }
        })
    }
}

impl crate::state::StateSource for PingMonitor {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(PingMonitor::snapshot(self)).unwrap_or(serde_json::Value::Null)
    }
}

/// One echo request with a 2 s timeout (`Ping.SendPingAsync(host, 2000)`): the round trip in whole milliseconds, or
/// None when the host does not resolve, does not answer or the ping fails. Like .NET it pings the first address the
/// name resolves to.
pub async fn ping_once(host: &str) -> Option<f64> {
    let addr = match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, 0),
        Err(_) => tokio::time::timeout(RESOLVE_LIMIT, tokio::net::lookup_host((host, 0)))
            .await
            .ok()?
            .ok()?
            .next()?,
    };
    tokio::task::spawn_blocking(move || echo(addr, TIMEOUT))
        .await
        .ok()?
}

/// Unix: an ICMP socket (the unprivileged datagram kind where the OS allows it, else a raw one), else the system
/// `ping`, as .NET does when it cannot open a raw socket. Windows: `IcmpSendEcho` / `Icmp6SendEcho2`.
fn echo(addr: SocketAddr, timeout: Duration) -> Option<f64> {
    #[cfg(unix)]
    {
        for raw in [false, true] {
            if let Ok(ms) = icmp::echo(addr, timeout, raw) {
                return ms;
            }
        }
        ping_tool(addr.ip(), timeout)
    }
    #[cfg(windows)]
    {
        windows_icmp::echo(addr, timeout)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (addr, timeout);
        None
    }
}

/// The ICMP echo request: type, code, checksum, identifier, sequence number (big-endian), then the payload.
#[cfg(any(unix, test))]
fn request(v6: bool, id: u16, seq: u16) -> Vec<u8> {
    let mut p = vec![if v6 { 128 } else { 8 }, 0, 0, 0];
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(PAYLOAD);
    if !v6 {
        // ICMPv6 checksums cover a pseudo-header the kernel fills in; ICMPv4 ones are the sender's job
        let sum = checksum(&p);
        p[2..4].copy_from_slice(&sum.to_be_bytes());
    }
    p
}

/// The internet checksum (RFC 1071).
#[cfg(any(unix, test))]
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = data
        .chunks(2)
        .map(|c| u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])))
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Whether a received datagram is the echo reply to (`id`, `seq`). A received ICMPv4 message may start with its IP
/// header (macOS datagram sockets and every raw socket) and is skipped past; a kernel that rewrites the identifier
/// (Linux datagram sockets) is matched on the sequence number alone.
#[cfg(any(unix, test))]
fn is_reply(packet: &[u8], v6: bool, id: u16, seq: u16, check_id: bool) -> bool {
    let mut p = packet;
    if !v6 && p.len() >= 20 && p[0] >> 4 == 4 {
        let header = usize::from(p[0] & 0x0f) * 4;
        p = p.get(header..).unwrap_or_default();
    }
    p.len() >= 8
        && p[0] == if v6 { 129 } else { 0 }
        && p[1] == 0
        && u16::from_be_bytes([p[6], p[7]]) == seq
        && (!check_id || u16::from_be_bytes([p[4], p[5]]) == id)
}

/// The round trip from the system `ping`'s "time=12.3 ms" (rounded to whole ms like .NET); "time<1 ms" is 0.
#[cfg(any(unix, test))]
fn parse_ping_time(output: &str) -> Option<f64> {
    if output.contains("time<") {
        return Some(0.0);
    }
    let rest = &output[output.find("time=")? + 5..];
    let number: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number.parse::<f64>().ok().map(f64::round)
}

#[cfg(unix)]
fn ping_tool(ip: IpAddr, timeout: Duration) -> Option<f64> {
    let target = ip.to_string();
    let secs = timeout.as_secs().max(1).to_string();
    // Linux (iputils, busybox): -W is seconds; macOS: -W is milliseconds, -t the whole run in seconds
    let mut args: Vec<&str> = vec!["-c", "1"];
    if cfg!(target_os = "macos") {
        args.extend(["-t", secs.as_str()]);
    } else {
        args.extend(["-W", secs.as_str()]);
    }
    let tool = if ip.is_ipv6() && cfg!(target_os = "macos") {
        "ping6"
    } else {
        if ip.is_ipv6() {
            args.insert(0, "-6");
        }
        "ping"
    };
    args.push(target.as_str());
    let budget = timeout.as_millis() as u64 + 1000;
    parse_ping_time(&crate::unix::sh::run_with_timeout(budget, tool, &args)?)
}

#[cfg(unix)]
mod icmp {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::time::{Duration, Instant};

    static SEQ: AtomicU16 = AtomicU16::new(1);

    struct Fd(i32);
    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: the descriptor came from socket() and is closed once
            unsafe { libc::close(self.0) };
        }
    }

    /// Err(()) when the OS refuses the socket (no permission for that kind); Ok(None) for a lost ping.
    pub(super) fn echo(addr: SocketAddr, timeout: Duration, raw: bool) -> Result<Option<f64>, ()> {
        let v6 = addr.is_ipv6();
        let (domain, proto) = if v6 {
            (libc::AF_INET6, libc::IPPROTO_ICMPV6)
        } else {
            (libc::AF_INET, libc::IPPROTO_ICMP)
        };
        let kind = if raw {
            libc::SOCK_RAW
        } else {
            libc::SOCK_DGRAM
        };
        // SAFETY: plain socket() call; the result is checked
        let fd = unsafe { libc::socket(domain, kind, proto) };
        if fd < 0 {
            return Err(());
        }
        let fd = Fd(fd);
        let id = (std::process::id() as u16) ^ 0x5042;
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let packet = super::request(v6, id, seq);
        // a Linux datagram socket puts its own identifier in the request and only hands back its own replies
        let check_id = raw || !cfg!(any(target_os = "linux", target_os = "android"));
        let started = Instant::now();
        if !send(fd.0, &packet, addr) {
            return Ok(None);
        }
        let mut buf = [0u8; 1500];
        loop {
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(None);
            }
            let tv = libc::timeval {
                tv_sec: left.as_secs() as _,
                tv_usec: left.subsec_micros().max(1) as _,
            };
            // SAFETY: `tv` is a valid timeval for SO_RCVTIMEO and its size is passed
            unsafe {
                libc::setsockopt(
                    fd.0,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&tv as *const libc::timeval).cast(),
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            // SAFETY: `buf` is writable for its whole length
            let n = unsafe { libc::recv(fd.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Ok(None); // the timeout, or an ICMP error the kernel reports on the socket
            }
            if super::is_reply(&buf[..n as usize], v6, id, seq, check_id) {
                return Ok(Some(started.elapsed().as_millis() as f64));
            }
        }
    }

    fn send(fd: i32, packet: &[u8], addr: SocketAddr) -> bool {
        // SAFETY: the sockaddr structs are zeroed and then filled in; each is passed with its own size
        let sent = unsafe {
            match addr {
                SocketAddr::V4(a) => {
                    let mut sa: libc::sockaddr_in = std::mem::zeroed();
                    sa.sin_family = libc::AF_INET as _;
                    sa.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
                    libc::sendto(
                        fd,
                        packet.as_ptr().cast(),
                        packet.len(),
                        0,
                        (&sa as *const libc::sockaddr_in).cast(),
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
                SocketAddr::V6(a) => {
                    let mut sa: libc::sockaddr_in6 = std::mem::zeroed();
                    sa.sin6_family = libc::AF_INET6 as _;
                    sa.sin6_addr.s6_addr = a.ip().octets();
                    sa.sin6_scope_id = a.scope_id();
                    libc::sendto(
                        fd,
                        packet.as_ptr().cast(),
                        packet.len(),
                        0,
                        (&sa as *const libc::sockaddr_in6).cast(),
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
        };
        sent == packet.len() as isize
    }
}

#[cfg(windows)]
mod windows_icmp {
    use std::net::SocketAddr;
    use std::time::Duration;

    use windows::Win32::NetworkManagement::IpHelper::{
        ICMP_ECHO_REPLY, ICMPV6_ECHO_REPLY_LH, Icmp6CreateFile, Icmp6SendEcho2, IcmpCloseHandle,
        IcmpCreateFile, IcmpSendEcho,
    };
    use windows::Win32::Networking::WinSock::{AF_INET6, SOCKADDR_IN6};

    /// IP_SUCCESS.
    const SUCCESS: u32 = 0;

    pub(super) fn echo(addr: SocketAddr, timeout: Duration) -> Option<f64> {
        let ms = timeout.as_millis() as u32;
        let data = super::PAYLOAD;
        // room for the reply struct, the echoed data and an ICMP error message; u64s keep it 8-byte aligned
        let mut reply = [0u64; 128];
        let size = std::mem::size_of_val(&reply) as u32;
        // SAFETY: the handles come from Icmp*CreateFile and are closed once; the request and reply buffers outlive the
        // calls and are passed with their sizes; the reply struct is read only after a reply was written
        unsafe {
            match addr {
                SocketAddr::V4(a) => {
                    let handle = IcmpCreateFile().ok()?;
                    let n = IcmpSendEcho(
                        handle,
                        u32::from_ne_bytes(a.ip().octets()),
                        data.as_ptr().cast(),
                        data.len() as u16,
                        None,
                        reply.as_mut_ptr().cast(),
                        size,
                        ms,
                    );
                    let _ = IcmpCloseHandle(handle);
                    if n == 0 {
                        return None;
                    }
                    let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
                    (r.Status == SUCCESS).then_some(f64::from(r.RoundTripTime))
                }
                SocketAddr::V6(a) => {
                    let handle = Icmp6CreateFile().ok()?;
                    let mut source: SOCKADDR_IN6 = std::mem::zeroed();
                    source.sin6_family = AF_INET6;
                    let mut target: SOCKADDR_IN6 = std::mem::zeroed();
                    target.sin6_family = AF_INET6;
                    target.sin6_addr.u.Byte = a.ip().octets();
                    target.Anonymous.sin6_scope_id = a.scope_id();
                    let n = Icmp6SendEcho2(
                        handle,
                        None,
                        None,
                        None,
                        &source,
                        &target,
                        data.as_ptr().cast(),
                        data.len() as u16,
                        None,
                        reply.as_mut_ptr().cast(),
                        size,
                        ms,
                    );
                    let _ = IcmpCloseHandle(handle);
                    if n == 0 {
                        return None;
                    }
                    let r = &*(reply.as_ptr() as *const ICMPV6_ECHO_REPLY_LH);
                    (r.Status == SUCCESS).then_some(f64::from(r.RoundTripTime))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_config(json: &str) -> (tempfile::TempDir, Arc<ConfigStore>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        let store = Arc::new(ConfigStore::new(dir.path().join("config.json")));
        (dir, store)
    }

    fn host(store: &ConfigStore) -> String {
        store.get().ping.host.clone().unwrap_or_default()
    }

    #[test]
    fn existing_configs_keep_pinging_the_default_host() {
        let (_d, store) = with_config("{}");
        assert_eq!(host(&store), crate::config::DEFAULT_PING_HOST);
    }

    #[test]
    fn host_is_trimmed_empty_means_off_and_junk_falls_back() {
        for (json, expected) in [
            (r#"{"ping":{"host":""}}"#, ""),
            (r#"{"ping":{"host":null}}"#, ""),
            (r#"{"ping":{"host":"  192.168.1.1 "}}"#, "192.168.1.1"),
            (
                r#"{"ping":{"host":"bad host"}}"#,
                crate::config::DEFAULT_PING_HOST,
            ),
        ] {
            let (_d, store) = with_config(json);
            assert_eq!(host(&store), expected, "{json}");
        }
    }

    #[test]
    fn changing_the_target_immediately_hides_previous_measurements() {
        for (new_host, visible) in [
            ("current.example", true),
            ("next.example", false),
            ("", false),
        ] {
            let (_d, store) = with_config(r#"{"ping":{"host":"current.example"}}"#);
            let monitor = PingMonitor::new(store.clone());
            monitor.sample(Some(12.0), "current.example");
            assert_eq!(
                monitor.snapshot().map(|s| s.host).as_deref(),
                Some("current.example")
            );
            store
                .save(
                    &serde_json::json!({"ping": {"host": new_host}}).to_string(),
                    None,
                )
                .unwrap();
            monitor.sample(Some(15.0), "current.example"); // an already-started old probe completes after the save
            let expected = visible.then_some("current.example");
            assert_eq!(
                monitor.snapshot().map(|s| s.host).as_deref(),
                expected,
                "{new_host}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_host_sends_nothing_and_clears_the_snapshot() {
        let (_d, store) = with_config(r#"{"ping":{"host":""}}"#);
        let monitor = PingMonitor::new(store);
        monitor.sample(Some(12.0), "stale"); // as if a host had been set before
        let stop = crate::server::Stop::default();
        let task = monitor.start(stop.clone());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while monitor.snapshot().is_some() && std::time::Instant::now() < end {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(monitor.snapshot(), None);
        stop.stop();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    // Review Focus 4: a host that does not resolve counts as a lost ping, quickly and without a panic.
    #[tokio::test]
    async fn an_unresolvable_host_counts_as_loss() {
        let started = std::time::Instant::now();
        assert_eq!(ping_once("phonebox-no-such-host.invalid").await, None);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let (_d, store) = with_config(r#"{"ping":{"host":"phonebox-no-such-host.invalid"}}"#);
        let monitor = PingMonitor::new(store);
        monitor.sample(Some(20.0), "phonebox-no-such-host.invalid");
        monitor.sample(None, "phonebox-no-such-host.invalid");
        let s = monitor.snapshot().unwrap();
        assert_eq!((s.ms, s.avg_ms, s.loss_pct), (None, Some(20.0), 50.0));
        let json = crate::state::StateSource::snapshot(&*monitor);
        assert_eq!(
            json,
            serde_json::json!({"host": "phonebox-no-such-host.invalid", "ms": null, "avgMs": 20.0, "lossPct": 50.0})
        );
    }

    #[test]
    fn the_request_and_reply_bytes_follow_icmp() {
        let req = request(false, 0x1234, 7);
        assert_eq!(&req[..2], &[8, 0]);
        assert_eq!(&req[4..8], &[0x12, 0x34, 0, 7]);
        assert_eq!(checksum(&req), 0, "a packet with its checksum sums to zero");
        assert_eq!(&request(true, 1, 2)[..4], &[128, 0, 0, 0]);
        let mut reply = req.clone();
        reply[0] = 0;
        assert!(is_reply(&reply, false, 0x1234, 7, true));
        assert!(!is_reply(&reply, false, 0x1235, 7, true));
        assert!(
            is_reply(&reply, false, 0x1235, 7, false),
            "Linux datagram sockets rewrite the identifier"
        );
        assert!(!is_reply(&reply, false, 0x1234, 8, true));
        let mut with_ip = vec![0x45u8; 1];
        with_ip.extend([0u8; 19]);
        with_ip.extend(&reply);
        assert!(
            is_reply(&with_ip, false, 0x1234, 7, true),
            "macOS and raw sockets hand back the IP header too"
        );
        assert!(
            !is_reply(&req, false, 0x1234, 7, true),
            "our own request is not the reply"
        );
        let mut v6 = request(true, 1, 2);
        v6[0] = 129;
        assert!(is_reply(&v6, true, 1, 2, true));
    }

    #[test]
    fn the_system_ping_time_is_read_in_whole_milliseconds() {
        assert_eq!(
            parse_ping_time("64 bytes from 1.1.1.1: icmp_seq=1 ttl=57 time=12.6 ms"),
            Some(13.0)
        );
        assert_eq!(
            parse_ping_time("64 bytes from ::1: icmp_seq=1 ttl=64 time=0.045 ms"),
            Some(0.0)
        );
        assert_eq!(
            parse_ping_time("Reply from 1.1.1.1: bytes=32 time<1ms TTL=57"),
            Some(0.0)
        );
        assert_eq!(
            parse_ping_time("1 packets transmitted, 0 received, 100% packet loss"),
            None
        );
    }

    // the path a Linux machine takes when it allows neither datagram nor raw ICMP sockets
    #[cfg(unix)]
    #[test]
    fn the_system_ping_answers_for_the_loopback() {
        let ms = ping_tool("127.0.0.1".parse().unwrap(), TIMEOUT);
        assert!(ms.is_some_and(|ms| (0.0..2000.0).contains(&ms)), "{ms:?}");
    }

    #[tokio::test]
    async fn the_loopback_answers_a_ping() {
        assert!(
            ping_once("127.0.0.1")
                .await
                .is_some_and(|ms| (0.0..2000.0).contains(&ms))
        );
    }
}
