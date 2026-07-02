//! SSRF range classifier. `is_denied_range` is the single security-critical
//! predicate: it returns true for every address class a *hostname* must not be
//! permitted to resolve to (the DNS-rebinding defense). Literal-IP CONNECT
//! targets are handled by the carve-out in `proxy.rs`, not here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True iff `ip` is in a range we refuse to connect a *resolved hostname* to.
/// Covers loopback, RFC1918 private, link-local, unique-local, CGNAT,
/// multicast, unspecified, class-E reserved, and the fixed-prefix IPv4-in-IPv6
/// transition encodings (IPv4-mapped, IPv4-compatible, IPv4-translated,
/// well-known NAT64 `64:ff9b::/96`, 6to4) — each unwrapped + re-checked as v4.
/// See [`embedded_transition_v4`] for the residual gap (site-specific NAT64
/// prefixes, Teredo, ISATAP), tracked as a follow-up issue.
pub fn is_denied_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_denied_v4(v4),
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) must be unwrapped so a mapped
            // private address can't slip past as "just an IPv6 global".
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_denied_v4(v4);
            }
            // The fixed-prefix IPv4-in-IPv6 transition encodings (compatible,
            // translated, well-known NAT64, 6to4) embed a v4 the host kernel
            // may route to an internal destination — unwrap + re-classify each
            // so a private v4 can never hide inside them (fail-closed). Note
            // the residual gap documented on `embedded_transition_v4`.
            if let Some(v4) = embedded_transition_v4(v6) {
                return is_denied_v4(v4);
            }
            is_denied_v6(v6)
        }
    }
}

/// Extract the IPv4 address embedded in an IPv4/IPv6 *transition* encoding so
/// it can be re-classified by [`is_denied_v4`]. `::ffff:a.b.c.d`
/// (IPv4-mapped) is handled by the caller via `to_ipv4_mapped()` before this.
///
/// The gap these close is real on IPv6-transition networks: a DNS64 resolver
/// on an IPv6-only host synthesises NAT64 (`64:ff9b::/96`) addresses, so an
/// allowlisted hostname could resolve to an embedded private/loopback v4 and
/// bypass the v4 deny list entirely (audit finding #4).
///
/// **Not covered (documented residual, tracked as a follow-up issue):**
/// - *Site-specific NAT64 prefixes* (RFC 6052 Network-Specific Prefixes at
///   /32../64, and RFC 8215's `64:ff9b:1::/48` local-use prefix). The proxy
///   cannot know a host's configured NAT64 prefix, and for prefixes shorter
///   than /96 the embedded v4 is split around the reserved bits 64..71, so it
///   cannot be extracted soundly without that config. The well-known
///   `64:ff9b::/96` prefix — which dominates real DNS64 deployments — *is*
///   covered above.
/// - *Teredo* (`2001::/32`) and *ISATAP* (`::0:5efe:a.b.c.d`) embed a v4 in
///   positions other than the trailing 32 bits; resolvers do not synthesise
///   these for allowlisted hostnames, so they are a weaker vector.
///
/// These are unreachable on the common well-known-prefix deployment; a
/// belt-and-braces fix (or a connect-time re-check against the actual peer
/// address) is tracked separately rather than inviting a subtle split-embed
/// bug in this security-critical predicate.
fn embedded_transition_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let trailing_v4 = || {
        Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        )
    };

    // IPv4-compatible ::a.b.c.d (RFC 4291 §2.5.5.1, deprecated): top 96 bits
    // zero. `::` and `::1` carry no meaningful embedded v4 — leave them to the
    // v6 predicates.
    if s[0..6] == [0, 0, 0, 0, 0, 0] && !ip.is_unspecified() && !ip.is_loopback() {
        return Some(trailing_v4());
    }
    // IPv4-translated ::ffff:0:a.b.c.d (`::ffff:0:0/96`, SIIT): [0,0,0,0,0xffff,0,v4].
    if s[0..6] == [0, 0, 0, 0, 0xffff, 0] {
        return Some(trailing_v4());
    }
    // NAT64 well-known 64:ff9b::/96 (RFC 6052): [0x0064,0xff9b,0,0,0,0,v4].
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return Some(trailing_v4());
    }
    // 6to4 2002::/16 (RFC 3056): embeds the v4 in bits 16..48 (segs[1],[2]).
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            (s[1] & 0xff) as u8,
            (s[2] >> 8) as u8,
            (s[2] & 0xff) as u8,
        ));
    }
    None
}

fn is_denied_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()            // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254.0.0/16
        || ip.is_multicast()    // 224.0.0.0/4
        || ip.is_unspecified()  // 0.0.0.0
        || ip.is_broadcast()    // 255.255.255.255
        || is_cgnat_v4(ip)      // 100.64.0.0/10
        || is_reserved_v4(ip)   // 240.0.0.0/4 (class E)
}

/// RFC6598 carrier-grade NAT space (`is_shared` is unstable in std, so inline).
fn is_cgnat_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

/// RFC1112 class-E reserved space 240.0.0.0/4 (generally unroutable;
/// `Ipv4Addr::is_reserved` is unstable, so inline). 255.255.255.255 is also
/// caught by `is_broadcast`; 224–239 (multicast) is a distinct range.
fn is_reserved_v4(ip: Ipv4Addr) -> bool {
    ip.octets()[0] >= 240
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

    #[test]
    fn class_e_reserved_v4_is_denied() {
        // 240.0.0.0/4 (audit finding #8) — generally unroutable, deny anyway.
        assert!(is_denied_range(v4("240.0.0.1")));
        assert!(is_denied_range(v4("250.1.2.3")));
        // 239.x (top of multicast) is a distinct range, still denied via multicast.
        assert!(is_denied_range(v4("239.255.255.255")));
        // 223.x is the top of ordinary unicast space — must stay allowed.
        assert!(!is_denied_range(v4("223.0.113.5")));
    }

    #[test]
    fn nat64_embedded_private_is_denied() {
        // 64:ff9b::/96 (RFC 6052) embedding a private/loopback v4 — the DNS64
        // SSRF vector (audit finding #4). Build explicitly to avoid ambiguity.
        let loop64 = IpAddr::V6(Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0x7f00, 0x0001));
        assert!(is_denied_range(loop64), "NAT64-embedded 127.0.0.1 must be denied");
        let meta64 = IpAddr::V6(Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0xa9fe, 0xa9fe));
        assert!(is_denied_range(meta64), "NAT64-embedded 169.254.169.254 must be denied");
        // NAT64-embedded *public* v4 (8.8.8.8) is still routable → allowed.
        let pub64 = IpAddr::V6(Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808));
        assert!(!is_denied_range(pub64), "NAT64-embedded 8.8.8.8 must be allowed");
    }

    #[test]
    fn ipv4_translated_private_is_denied() {
        // ::ffff:0:0/96 (SIIT) embedding 127.0.0.1 (audit finding #4).
        let t = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, 0x7f00, 0x0001));
        assert!(is_denied_range(t), "IPv4-translated 127.0.0.1 must be denied");
    }

    #[test]
    fn sixtofour_embedded_private_is_denied() {
        // 2002::/16 (RFC 3056) embeds the v4 in bits 16..48; 2002:7f00:1::
        // → 127.0.0.1 (audit finding #8).
        let s = IpAddr::V6(Ipv6Addr::new(0x2002, 0x7f00, 0x0001, 0, 0, 0, 0, 0));
        assert!(is_denied_range(s), "6to4-embedded 127.0.0.1 must be denied");
        // 6to4 wrapping a public v4 (2002:0808:0808:: → 8.8.8.8) stays allowed.
        let p = IpAddr::V6(Ipv6Addr::new(0x2002, 0x0808, 0x0808, 0, 0, 0, 0, 0));
        assert!(!is_denied_range(p), "6to4-embedded 8.8.8.8 must be allowed");
    }
}
