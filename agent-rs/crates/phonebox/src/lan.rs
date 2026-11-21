//! Port of the adapter part of `Lan.cs` (`Addresses`, `ShouldSkipAdapter`); the Wi-Fi server comes with plan 4.

/// Addresses the phone can try: private LAN IPv4 addresses first, then Tailscale (100.64.0.0/10).
pub fn addresses() -> Vec<String> {
    use crate::net_adapters::AdapterKind;
    let mut lan: Vec<String> = Vec::new();
    let mut tailscale: Vec<String> = Vec::new();
    for ni in crate::net_adapters::all() {
        if !ni.up || ni.kind == AdapterKind::Loopback {
            continue;
        }
        let desc = format!("{} {}", ni.description, ni.name);
        let is_tailscale = desc.to_lowercase().contains("tailscale");
        if !is_tailscale && should_skip_adapter(&desc, ni.has_ipv4_gateway) {
            continue;
        }
        for ip in &ni.ipv4 {
            let [b0, b1, ..] = ip.octets();
            if b0 == 100 && (64..=127).contains(&b1) {
                tailscale.push(ip.to_string());
            } else if b0 == 10 || (b0 == 172 && (16..=31).contains(&b1)) || (b0 == 192 && b1 == 168)
            {
                lan.push(ip.to_string());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    lan.into_iter()
        .chain(tailscale)
        .filter(|a| seen.insert(a.clone()))
        .collect()
}

/// Virtual adapters (Hyper-V, WSL, VMware, Bluetooth PAN) are skipped unless one owns an IPv4 default gateway: a
/// Hyper-V external switch takes over the physical NIC's address and route.
pub fn should_skip_adapter(desc: &str, has_ipv4_gateway: bool) -> bool {
    let d = desc.to_lowercase();
    !has_ipv4_gateway
        && [
            "virtual",
            "vethernet",
            "hyper-v",
            "wsl",
            "vmware",
            "bluetooth",
        ]
        .iter()
        .any(|w| d.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    // LanTests (ConfigTests.cs): a Hyper-V external switch owns the physical NIC's address and gateway (#175).
    #[test]
    fn virtual_adapters_are_skipped_unless_they_carry_the_default_route() {
        for (desc, has_gateway, skip) in [
            (
                "Hyper-V Virtual Ethernet Adapter vEthernet (WAN)",
                true,
                false,
            ),
            (
                "Hyper-V Virtual Ethernet Adapter vEthernet (Default Switch)",
                false,
                true,
            ),
            ("WSL adapter vEthernet (WSL)", false, true),
            ("VMware Virtual Ethernet Adapter VMnet8", false, true),
            ("Bluetooth Device (Personal Area Network)", false, true),
            (
                "Intel(R) Ethernet Controller I219-LM Ethernet",
                false,
                false,
            ),
            ("Intel(R) Ethernet Controller I219-LM Ethernet", true, false),
        ] {
            assert_eq!(
                should_skip_adapter(desc, has_gateway),
                skip,
                "{desc} {has_gateway}"
            );
        }
    }

    #[test]
    fn addresses_are_private_lan_then_tailscale() {
        for a in addresses() {
            let ip: std::net::Ipv4Addr = a.parse().unwrap();
            let [b0, b1, ..] = ip.octets();
            assert!(
                b0 == 10
                    || (b0 == 172 && (16..=31).contains(&b1))
                    || (b0 == 192 && b1 == 168)
                    || (b0 == 100 && (64..=127).contains(&b1)),
                "{a}"
            );
        }
    }
}
