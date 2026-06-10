//! SSRF range classifier. `is_denied_range` is the single security-critical
//! predicate: it returns true for every address class a *hostname* must not be
//! permitted to resolve to (the DNS-rebinding defense). Literal-IP CONNECT
//! targets are handled by the carve-out in `proxy.rs`, not here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True iff `ip` is in a range we refuse to connect a *resolved hostname* to.
/// Covers loopback, RFC1918 private, link-local, unique-local, CGNAT,
/// multicast, unspecified, and IPv4-mapped-IPv6 (unwrapped + re-checked).
pub fn is_denied_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_denied_v4(v4),
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) must be unwrapped so a mapped
            // private address can't slip past as "just an IPv6 global".
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_denied_v4(v4);
            }
            // IPv4-compatible (::a.b.c.d, RFC 4291 §2.5.5.1 deprecated): top 96
            // bits zero. Not issued by real resolvers and routed as pure IPv6 on
            // modern kernels, but unwrap + re-classify anyway so a private v4
            // can never hide inside this legacy form (fail-closed). `::` and
            // `::1` carry no meaningful embedded v4 and stay with the v6
            // predicates below.
            let segs = v6.segments();
            if segs[0..6] == [0, 0, 0, 0, 0, 0] && !v6.is_unspecified() && !v6.is_loopback() {
                let v4 = Ipv4Addr::new(
                    (segs[6] >> 8) as u8,
                    (segs[6] & 0xff) as u8,
                    (segs[7] >> 8) as u8,
                    (segs[7] & 0xff) as u8,
                );
                return is_denied_v4(v4);
            }
            is_denied_v6(v6)
        }
    }
}

fn is_denied_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()            // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254.0.0/16
        || ip.is_multicast()    // 224.0.0.0/4
        || ip.is_unspecified()  // 0.0.0.0
        || ip.is_broadcast()    // 255.255.255.255
        || is_cgnat_v4(ip)      // 100.64.0.0/10
}

/// RFC6598 carrier-grade NAT space (`is_shared` is unstable in std, so inline).
fn is_cgnat_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

fn is_denied_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()            // ::1
        || ip.is_unspecified()  // ::
        || ip.is_multicast()    // ff00::/8
        || is_unique_local_v6(ip) // fc00::/7
        || is_link_local_v6(ip)   // fe80::/10
}

/// fc00::/7 (unique-local). `Ipv6Addr::is_unique_local` is unstable; inline.
fn is_unique_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10 (link-local). `Ipv6Addr::is_unicast_link_local` is unstable; inline.
fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    #[test]
    fn public_v4_is_allowed() {
        assert!(!is_denied_range(v4("203.0.113.5")));
        assert!(!is_denied_range(v4("8.8.8.8")));
    }

    #[test]
    fn private_and_loopback_v4_are_denied() {
        for s in ["127.0.0.1", "10.0.0.1", "172.16.5.5", "192.168.1.1",
                  "169.254.1.1", "100.64.0.1", "224.0.0.1", "0.0.0.0",
                  "255.255.255.255"] {
            assert!(is_denied_range(v4(s)), "{s} should be denied");
        }
    }

    #[test]
    fn cgnat_boundaries() {
        assert!(is_denied_range(v4("100.64.0.0")));
        assert!(is_denied_range(v4("100.127.255.255")));
        assert!(!is_denied_range(v4("100.63.255.255")));
        assert!(!is_denied_range(v4("100.128.0.0")));
    }

    #[test]
    fn public_v6_is_allowed() {
        assert!(!is_denied_range(v6("2606:4700:4700::1111")));
    }

    #[test]
    fn private_and_loopback_v6_are_denied() {
        for s in ["::1", "::", "ff02::1", "fc00::1", "fd12:3456::1", "fe80::1"] {
            assert!(is_denied_range(v6(s)), "{s} should be denied");
        }
    }

    #[test]
    fn ipv4_mapped_private_is_denied() {
        // ::ffff:10.0.0.1 must be unwrapped and denied.
        assert!(is_denied_range(v6("::ffff:10.0.0.1")));
        // ::ffff:8.8.8.8 unwraps to a public v4 → allowed.
        assert!(!is_denied_range(v6("::ffff:8.8.8.8")));
    }

    #[test]
    fn ipv4_compatible_private_is_denied() {
        // Deprecated ::a.b.c.d form must not bypass the v4 deny (fail-closed).
        assert!(is_denied_range(v6("::10.0.0.1")));
        assert!(is_denied_range(v6("::127.0.0.1")));
        // ::1 and :: stay covered by the v6 predicates.
        assert!(is_denied_range(v6("::1")));
        assert!(is_denied_range(v6("::")));
    }
}
