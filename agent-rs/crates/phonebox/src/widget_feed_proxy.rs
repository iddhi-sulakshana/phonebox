//! `WidgetFeedProxy.IsPublicAddress` (the proxy itself comes with plan 6): whether an address is on the public
//! internet, so fetches the agent makes for the phone (feeds, cover art) never reach the LAN or this computer.

use std::net::IpAddr;

pub fn is_public_address(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let b = v4.octets();
            !(matches!(b[0], 0 | 10 | 127)
                || (b[0] == 100 && (b[1] & 0xC0) == 64) // 100.64.0.0/10 CGNAT
                || (b[0] == 169 && b[1] == 254) // link-local
                || (b[0] == 172 && (16..=31).contains(&b[1]))
                || (b[0] == 192 && b[1] == 168)
                || (b[0] == 192 && b[1] == 0 && matches!(b[2], 0 | 2)) // IETF / TEST-NET-1
                || (b[0] == 198 && (matches!(b[1], 18 | 19) || (b[1] == 51 && b[2] == 100))) // benchmark / TEST-NET-2
                || (b[0] == 203 && b[1] == 0 && b[2] == 113) // TEST-NET-3
                || b[0] >= 224) // multicast and reserved
        }
        IpAddr::V6(v6) => {
            let b = v6.octets();
            // global unicast 2000::/3 only: excludes ::, ::1, link- and site-local, multicast, NAT64, fc00::/7
            if (b[0] & 0xE0) != 0x20 {
                return false;
            }
            if b[0] == 0x20 && b[1] == 0x01 {
                if b[2] == 0 && b[3] == 0 {
                    return false; // 2001::/32 Teredo
                }
                if b[2] == 0 && b[3] == 0x02 && b[4] == 0 && b[5] == 0 {
                    return false; // 2001:2::/48 benchmarking
                }
                if b[2] == 0x0D && b[3] == 0xB8 {
                    return false; // 2001:db8::/32 documentation
                }
            }
            if b[0] == 0x3F && b[1] == 0xFF && (b[2] & 0xF0) == 0 {
                return false; // 3fff::/20 documentation
            }
            if b[0] == 0x20 && b[1] == 0x02 {
                // 2002::/16 6to4 must wrap a public IPv4
                return is_public_address(IpAddr::V4(std::net::Ipv4Addr::new(
                    b[2], b[3], b[4], b[5],
                )));
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_exclude_loopback_private_link_local_and_mapped() {
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "198.51.99.1",
            "2606:4700:4700::1111",
            "2002:0808:0808::1",
        ];
        let not_public = [
            "0.1.2.3",
            "10.0.0.1",
            "127.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.5",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:192.168.1.5",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "fc00::1",
            "fd12::1",
            "64:ff9b::1.1.1.1",
            "2001:0:1::1",
            "2001:2:0::1",
            "2001:db8::1",
            "3fff:0fff::1",
            "2002:c0a8:0105::1",
        ];
        for a in public {
            assert!(is_public_address(a.parse().unwrap()), "{a} is public");
        }
        for a in not_public {
            assert!(!is_public_address(a.parse().unwrap()), "{a} is not public");
        }
        assert!(
            is_public_address("::ffff:1.1.1.1".parse().unwrap()),
            "a mapped public IPv4"
        );
    }

    #[test]
    fn is_public_address_rejects_loopback_private_and_link_local() {
        for (address, expected) in [
            ("127.0.0.1", false),
            ("10.0.0.1", false),
            ("172.16.0.5", false),
            ("192.168.1.10", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("::1", false),
            ("fe80::1", false),
            ("fd00::1", false),
            ("::ffff:127.0.0.1", false),
            ("::127.0.0.1", false),
            ("::ffff:0:127.0.0.1", false),
            ("64:ff9b::127.0.0.1", false),
            ("64:ff9b:1::192.168.1.1", false),
            ("2002:7f00:1::1", false),
            ("2002:0101:0101::1", true),
            ("2001::1", false),
            ("2001:db8::1", false),
            ("2001:2::1", false),
            ("100::1", false),
            ("1.1.1.1", true),
            ("2606:4700:4700::1111", true),
        ] {
            assert_eq!(
                is_public_address(address.parse().unwrap()),
                expected,
                "{address}"
            );
        }
    }

    // StoreFollowUpTests (feed proxy addresses)
    #[test]
    fn documentation_addresses_are_not_public() {
        for (address, expected) in [
            ("3fff::1", false), // 3fff::/20 documentation
            ("3fff:0fff:ffff::1", false),
            ("3fff:1000::1", true), // just past the documentation block
            ("2001:db8::1", false),
            ("2606:4700::1111", true),
        ] {
            assert_eq!(
                is_public_address(address.parse().unwrap()),
                expected,
                "{address}"
            );
        }
    }
}
