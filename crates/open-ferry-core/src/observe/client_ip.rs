// Ported from gin-gonic/gin v1.10.1 context.go (ClientIP, RemoteIP) and
// gin.go (prepareTrustedCIDRs, isTrustedProxy, validateHeader, parseIP)
// (MIT), and Go's net/ip.go and net/parse.go (ParseIP, ParseCIDR, CIDRMask,
// IP.To4, IP.Mask, IP.String, networkNumberAndMask, IPNet.Contains, dtoi)
// (go1.27, BSD-3-Clause), as CLIProxyAPI internal/api/server.go (NewServer)
// and internal/api/handlers/management/handler.go (Middleware) use them,
// and CLIProxyAPI sdk/api/handlers/handlers.go (requestClientIP) (v8.0.10,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! The client address the management API calls local or remote, and keys
//! its bans on, and that the request context records for the logs: gin's
//! `ClientIP` with the config's `trusted-proxies` as the engine's trusted
//! proxies, and gin's default headers, `X-Forwarded-For` then `X-Real-IP`.
//!
//! The headers are read only when the connection itself comes from a
//! trusted proxy, so a client that isn't one can't look local by sending
//! them. With no `trusted-proxies`, no proxy is trusted and the address is
//! always the connection's. An address from a header is returned as it was
//! written; the connection's is written as Go writes an IP.
//!
//! The trusted proxies are read once, when the server starts, as upstream
//! reads them.
//!
//! [`remote_ip`] gives the connection's own address, which the request
//! context records beside gin's.
//!
//! Deviations from upstream: [`remote_ip`] writes an IPv6 zone as the
//! scope's number, where Go writes the interface's name.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use http::HeaderMap;

/// The headers gin reads a forwarded client address from, in order
/// (`RemoteIPHeaders`).
const REMOTE_IP_HEADERS: [&str; 2] = ["x-forwarded-for", "x-real-ip"];

/// Go's `big` in `dtoi`: a prefix length this long or longer is an error.
const BIG: u32 = 0xff_ffff;

/// The proxies whose forwarded-address headers are believed.
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies {
    networks: Vec<IpNet>,
}

impl TrustedProxies {
    /// The proxies in `entries`, each an IP address or a CIDR, as gin's
    /// `SetTrustedProxies` reads them. As upstream does, a list with an
    /// entry gin can't read trusts no proxy at all. The config loader
    /// rejects such a list, so this only matters for a config built in
    /// code.
    pub fn new(entries: &[String]) -> Self {
        let mut networks = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut text = entry.clone();
            if !entry.contains('/') {
                let Some(ip) = parse_ip(entry) else {
                    return Self::default();
                };
                text.push_str(if to4(&ip).is_some() { "/32" } else { "/128" });
            }
            let Some(network) = parse_cidr(&text) else {
                return Self::default();
            };
            networks.push(network);
        }
        Self { networks }
    }

    /// gin's `isTrustedProxy`.
    fn contains(&self, ip: &[u8; 16]) -> bool {
        self.networks.iter().any(|network| network.contains(ip))
    }
}

/// gin's `ClientIP`: the connection's address, unless it comes from a
/// trusted proxy and a forwarded-address header names the client. Empty
/// when the connection's address is unknown or has an IPv6 zone.
pub fn client_ip(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    trusted: &TrustedProxies,
) -> String {
    let remote = match peer {
        None => return String::new(),
        Some(SocketAddr::V4(addr)) => addr.ip().to_ipv6_mapped().octets(),
        // Go writes the scope as a zone, and `net.ParseIP` rejects zones.
        Some(SocketAddr::V6(addr)) if addr.scope_id() != 0 => return String::new(),
        Some(SocketAddr::V6(addr)) => addr.ip().octets(),
    };
    if trusted.contains(&remote) {
        for name in REMOTE_IP_HEADERS {
            let value = headers
                .get(name)
                .map(|value| lossy(value.as_bytes()))
                .unwrap_or_default();
            if let Some(ip) = validate_header(&value, trusted) {
                return ip;
            }
        }
    }
    ip_string(&remote)
}

/// The connection's address without its port, as upstream's
/// `requestClientIP` reads it from Go's `RemoteAddr`: written as Go writes
/// an IP, with an IPv6 zone after a `%`. Empty when unknown.
pub fn remote_ip(peer: Option<SocketAddr>) -> String {
    match peer {
        None => String::new(),
        Some(SocketAddr::V4(addr)) => addr.ip().to_string(),
        Some(SocketAddr::V6(addr)) if addr.scope_id() != 0 => {
            format!("{}%{}", addr.ip().to_canonical(), addr.scope_id())
        }
        Some(SocketAddr::V6(addr)) => addr.ip().to_canonical().to_string(),
    }
}

/// `bytes` as text, each byte of a broken UTF-8 sequence read as U+FFFD,
/// as Go's string conversion reads it.
fn lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
    out
}

/// gin's `validateHeader`: walking a forwarded-address list from its end,
/// the first address that isn't a trusted proxy, or the list's first.
/// `None` when an entry before that isn't an address.
fn validate_header(header: &str, trusted: &TrustedProxies) -> Option<String> {
    if header.is_empty() {
        return None;
    }
    let items: Vec<&str> = header.split(',').collect();
    for (i, item) in items.iter().enumerate().rev() {
        let text = item.trim();
        let ip = parse_ip(text)?;
        if i == 0 || !trusted.contains(&ip) {
            return Some(text.to_owned());
        }
    }
    None
}

/// Go's `net.ParseIP`: the address in sixteen bytes, an IPv4 address
/// mapped into IPv6. `None` for anything else, including an address with a
/// zone.
fn parse_ip(text: &str) -> Option<[u8; 16]> {
    match text.parse::<IpAddr>().ok()? {
        IpAddr::V4(ip) => Some(ip.to_ipv6_mapped().octets()),
        IpAddr::V6(ip) => Some(ip.octets()),
    }
}

/// Go's `IP.String` for a sixteen-byte address: dotted for a mapped IPv4
/// address, else IPv6 text.
fn ip_string(ip: &[u8; 16]) -> String {
    Ipv6Addr::from(*ip).to_canonical().to_string()
}

/// Go's `IP.To4`.
fn to4(ip: &[u8]) -> Option<&[u8]> {
    match ip.len() {
        4 => Some(ip),
        16 if ip[..10].iter().all(|&b| b == 0) && ip[10] == 0xff && ip[11] == 0xff => {
            Some(&ip[12..])
        }
        _ => None,
    }
}

/// A network as Go's `net.IPNet` holds it: an address and a mask of four
/// or sixteen bytes each, not always the same length.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IpNet {
    ip: Vec<u8>,
    mask: Vec<u8>,
}

impl IpNet {
    /// Go's `IPNet.Contains`.
    fn contains(&self, ip: &[u8]) -> bool {
        let Some((network, mask)) = self.network_number_and_mask() else {
            return false;
        };
        let ip = to4(ip).unwrap_or(ip);
        ip.len() == network.len() && (0..ip.len()).all(|i| network[i] & mask[i] == ip[i] & mask[i])
    }

    /// Go's `networkNumberAndMask`: the address and mask at one length, or
    /// `None` where they can't be.
    fn network_number_and_mask(&self) -> Option<(&[u8], &[u8])> {
        let ip = match to4(&self.ip) {
            Some(ip) => ip,
            None if self.ip.len() == 16 => &self.ip,
            None => return None,
        };
        let mask = match self.mask.len() {
            4 if ip.len() != 4 => return None,
            4 => &self.mask[..],
            16 if ip.len() == 4 => &self.mask[12..],
            16 => &self.mask[..],
            _ => return None,
        };
        Some((ip, mask))
    }
}

/// Go's `net.ParseCIDR`, keeping only the network: an address without a
/// zone, `/`, and a decimal prefix length no longer than the address. An
/// IPv4 address gets a four-byte network and mask; any IPv6 address,
/// including a mapped IPv4 one, sixteen.
fn parse_cidr(text: &str) -> Option<IpNet> {
    let (address, bits) = text.split_once('/')?;
    let address = address.parse::<IpAddr>().ok()?;
    let (ones, used) = dtoi(bits)?;
    let bit_len = if address.is_ipv4() { 32 } else { 128 };
    if used != bits.len() || ones > bit_len {
        return None;
    }
    let mask = cidr_mask(ones, bit_len);
    let address = match address {
        IpAddr::V4(ip) => ip.to_ipv6_mapped().octets(),
        IpAddr::V6(ip) => ip.octets(),
    };
    Some(IpNet {
        ip: mask_ip(&address, &mask),
        mask,
    })
}

/// Go's `dtoi`: the decimal number at the start of `text` and how many
/// bytes it took, or `None` for no digits or a number of `BIG` or more.
fn dtoi(text: &str) -> Option<(u32, usize)> {
    let mut n = 0;
    let mut used = 0;
    for b in text.bytes() {
        if !b.is_ascii_digit() {
            break;
        }
        n = n * 10 + u32::from(b - b'0');
        if n >= BIG {
            return None;
        }
        used += 1;
    }
    (used > 0).then_some((n, used))
}

/// Go's `CIDRMask` for a valid prefix length: `ones` one bits, then zeros,
/// `bits / 8` bytes long.
fn cidr_mask(ones: u32, bits: u32) -> Vec<u8> {
    let mut left = ones;
    (0..bits / 8)
        .map(|_| {
            let byte = if left >= 8 { 0xff } else { !(0xffu8 >> left) };
            left = left.saturating_sub(8);
            byte
        })
        .collect()
}

/// Go's `IP.Mask` for a sixteen-byte address, which `parse_cidr` always
/// has: a four-byte mask applies to a mapped IPv4 address's last four
/// bytes. Empty where Go returns nil.
fn mask_ip(ip: &[u8; 16], mask: &[u8]) -> Vec<u8> {
    let ip = if mask.len() == 4 && to4(ip).is_some() {
        &ip[12..]
    } else {
        &ip[..]
    };
    if ip.len() != mask.len() {
        return Vec::new();
    }
    ip.iter().zip(mask).map(|(a, m)| a & m).collect()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddrV6;

    use http::HeaderValue;

    use super::*;

    fn proxies(entries: &[&str]) -> TrustedProxies {
        TrustedProxies::new(&entries.iter().map(|e| (*e).to_owned()).collect::<Vec<_>>())
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn ip(peer: &str, pairs: &[(&'static str, &str)], trusted: &TrustedProxies) -> String {
        client_ip(Some(peer.parse().unwrap()), &headers(pairs), trusted)
    }

    // server_test.go TestNewServerAppliesTrustedProxyConfiguration.
    #[test]
    fn trusted_proxy_configuration() {
        let trusted = proxies(&["192.0.2.0/24"]);
        let forwarded = [("x-forwarded-for", "203.0.113.5")];
        assert_eq!(ip("192.0.2.10:43123", &forwarded, &trusted), "203.0.113.5");
        assert_eq!(
            ip("198.51.100.20:43123", &forwarded, &trusted),
            "198.51.100.20"
        );
        let none = TrustedProxies::default();
        assert_eq!(
            ip("198.51.100.20:43123", &forwarded, &none),
            "198.51.100.20"
        );
    }

    #[test]
    fn untrusted_peers_cannot_claim_to_be_local() {
        let trusted = proxies(&["10.0.0.1"]);
        for pairs in [
            &[("x-forwarded-for", "127.0.0.1")][..],
            &[("x-real-ip", "::1")][..],
        ] {
            assert_eq!(ip("203.0.113.9:1", pairs, &trusted), "203.0.113.9");
            assert_eq!(
                ip("203.0.113.9:1", pairs, &TrustedProxies::default()),
                "203.0.113.9"
            );
        }
        // A loopback peer that isn't trusted stays local, whatever it sends.
        assert_eq!(
            ip(
                "127.0.0.1:1",
                &[("x-forwarded-for", "203.0.113.9")],
                &trusted
            ),
            "127.0.0.1"
        );
    }

    #[test]
    fn forwarded_lists_are_walked_from_the_end() {
        let trusted = proxies(&["10.0.0.0/8", "2001:db8::1"]);
        let peer = "10.1.1.1:1";
        let read = |value: &str| ip(peer, &[("x-forwarded-for", value)], &trusted);
        assert_eq!(read("198.51.100.1, 10.0.0.2"), "198.51.100.1");
        assert_eq!(read("198.51.100.1, 203.0.113.7 , 10.0.0.2"), "203.0.113.7");
        // Every entry trusted: the first one.
        assert_eq!(read(" 10.0.0.3 ,10.0.0.2"), "10.0.0.3");
        // Returned as written, not rewritten.
        assert_eq!(read("::FFFF:127.0.0.1"), "::FFFF:127.0.0.1");
        assert_eq!(read("0:0:0:0:0:0:0:1"), "0:0:0:0:0:0:0:1");
        // Something that isn't an address stops the walk; then X-Real-IP.
        assert_eq!(read("198.51.100.1, junk"), "10.1.1.1");
        assert_eq!(read(""), "10.1.1.1");
        assert_eq!(
            ip(
                peer,
                &[("x-forwarded-for", "junk"), ("x-real-ip", "198.51.100.4")],
                &trusted
            ),
            "198.51.100.4"
        );
        // Only the first X-Forwarded-For line is read.
        assert_eq!(
            ip(
                peer,
                &[
                    ("x-forwarded-for", "10.0.0.2"),
                    ("x-forwarded-for", "198.51.100.1")
                ],
                &trusted
            ),
            "10.0.0.2"
        );
        // A zone isn't an address.
        assert_eq!(read("fe80::1%eth0"), "10.1.1.1");
    }

    #[test]
    fn peers_are_written_as_go_writes_them() {
        let none = TrustedProxies::default();
        assert_eq!(ip("[::1]:5", &[], &none), "::1");
        assert_eq!(ip("[::ffff:127.0.0.1]:5", &[], &none), "127.0.0.1");
        assert_eq!(
            ip("[2001:db8:0:0:1:0:0:1]:5", &[], &none),
            "2001:db8::1:0:0:1"
        );
        assert_eq!(ip("[::1.2.3.4]:5", &[], &none), "::102:304");
        assert_eq!(ip("[::]:5", &[], &none), "::");
        let scoped = SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 5, 0, 3));
        assert_eq!(client_ip(Some(scoped), &HeaderMap::new(), &none), "");
        assert_eq!(client_ip(None, &HeaderMap::new(), &none), "");
    }

    /// Not upstream's: the connection's own address loses its port and
    /// keeps its zone, as `requestClientIP` gives it.
    #[test]
    fn remote_addresses_keep_their_zone() {
        let remote = |peer: &str| remote_ip(Some(peer.parse().unwrap()));
        assert_eq!(remote("10.0.0.1:5"), "10.0.0.1");
        assert_eq!(remote("[::ffff:127.0.0.1]:5"), "127.0.0.1");
        assert_eq!(remote("[2001:db8:0:0:1:0:0:1]:5"), "2001:db8::1:0:0:1");
        let scoped = SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 5, 0, 3));
        assert_eq!(remote_ip(Some(scoped)), "fe80::1%3");
        assert_eq!(remote_ip(None), "");
    }

    #[test]
    fn networks_follow_go() {
        let net = |text: &str| parse_cidr(text).unwrap();
        assert_eq!(
            net("192.0.2.77/24"),
            IpNet {
                ip: vec![192, 0, 2, 0],
                mask: vec![255, 255, 255, 0]
            }
        );
        assert!(parse_cidr("192.0.2.0/33").is_none());
        assert!(parse_cidr("192.0.2.0/").is_none());
        assert!(parse_cidr("192.0.2.0/+8").is_none());
        assert!(parse_cidr("192.0.2.0/8x").is_none());
        assert!(parse_cidr("192.0.2.0").is_none());
        assert!(parse_cidr("fe80::1%1/64").is_none());
        assert!(net("192.0.2.0/00000000024").contains(&parse_ip("192.0.2.9").unwrap()));
        assert!(parse_cidr("::/16777215").is_none());

        // ::ffff:0:0/96 holds every IPv4 address.
        let mapped = net("::ffff:0:0/96");
        assert!(mapped.contains(&parse_ip("203.0.113.1").unwrap()));
        assert!(!mapped.contains(&parse_ip("2001:db8::1").unwrap()));

        // A mapped address alone becomes ::/32 under gin's rules: it trusts
        // IPv6 addresses starting 0:0, and no IPv4 address.
        let trusted = proxies(&["::ffff:192.0.2.1"]);
        assert!(trusted.contains(&parse_ip("::1").unwrap()));
        assert!(!trusted.contains(&parse_ip("192.0.2.1").unwrap()));

        // One unreadable entry trusts nothing.
        let broken = proxies(&["192.0.2.0/24", "nope"]);
        assert!(!broken.contains(&parse_ip("192.0.2.1").unwrap()));
        assert!(proxies(&[]).networks.is_empty());
    }

    /// Go's answers: gin v1.10.1's `ClientIP` after `SetTrustedProxies`
    /// with the trusted proxies, falling back to none as upstream does
    /// (go1.27.1 building for go 1.26.0).
    #[test]
    fn client_ip_matches_gin() {
        // Trusted proxies, peer, X-Forwarded-For and X-Real-IP values, and
        // the address.
        type Case<'a> = (
            &'a [&'a str],
            &'a str,
            &'a [&'a str],
            &'a [&'a str],
            &'a str,
        );
        let cases: &[Case] = &[
            (
                &[],
                "203.0.113.7:4000",
                &["127.0.0.1"],
                &["127.0.0.1"],
                "203.0.113.7",
            ),
            (&[], "127.0.0.1:1", &["203.0.113.9"], &[], "127.0.0.1"),
            (&[], "[::ffff:127.0.0.1]:9", &[], &[], "127.0.0.1"),
            (&[], "[fe80::1%1]:80", &[], &[], ""),
            (&[], "[2001:db8::1]:1", &[], &[], "2001:db8::1"),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["203.0.113.9"],
                &[],
                "203.0.113.9",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &[],
                &["203.0.113.9"],
                "203.0.113.9",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["bad, 1.2.3.4"],
                &["203.0.113.9"],
                "1.2.3.4",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["1.2.3.4, bad"],
                &[],
                "127.0.0.1",
            ),
            (&["127.0.0.1"], "127.0.0.1:1", &[""], &[" ::1 "], "::1"),
            (&["127.0.0.1"], "127.0.0.1:1", &[","], &[], "127.0.0.1"),
            (&["127.0.0.1"], "127.0.0.1:1", &["[::1]"], &[], "127.0.0.1"),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["1.2.3.4:80"],
                &[],
                "127.0.0.1",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["fe80::1%eth0"],
                &[],
                "127.0.0.1",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["01.2.3.4"],
                &[],
                "127.0.0.1",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["1.2.3.4\t"],
                &[],
                "1.2.3.4",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["1.2.3.4\u{a0}"],
                &[],
                "1.2.3.4",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["::FFFF:127.0.0.1"],
                &[],
                "::FFFF:127.0.0.1",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["2001:DB8::1"],
                &[],
                "2001:DB8::1",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["203.0.113.9", "127.0.0.1"],
                &[],
                "203.0.113.9",
            ),
            (
                &["127.0.0.1"],
                "127.0.0.1:1",
                &["10.0.0.1,,1.2.3.4"],
                &[],
                "1.2.3.4",
            ),
            (&["127.0.0.1"], "127.0.0.1:1", &[], &["bad"], "127.0.0.1"),
            (
                &["127.0.0.1"],
                "[::ffff:127.0.0.1]:9",
                &["203.0.113.9"],
                &[],
                "203.0.113.9",
            ),
            (&["127.0.0.1"], "[::1]:1", &["203.0.113.9"], &[], "::1"),
            (
                &["10.0.0.0/8"],
                "10.1.2.3:5",
                &["127.0.0.1, 203.0.113.9"],
                &[],
                "203.0.113.9",
            ),
            (
                &["10.0.0.0/8"],
                "10.1.2.3:5",
                &["203.0.113.9, 10.0.0.5"],
                &[],
                "203.0.113.9",
            ),
            (
                &["10.0.0.0/8"],
                "10.1.2.3:5",
                &["10.0.0.1, 10.0.0.2"],
                &[],
                "10.0.0.1",
            ),
            (
                &["10.0.0.0/8"],
                "10.1.2.3:5",
                &[" 1.2.3.4 , 10.0.0.1"],
                &[],
                "1.2.3.4",
            ),
            (
                &["10.0.0.0/8"],
                "10.1.2.3:5",
                &["1.2.3.4, 203.0.113.5, 10.0.0.9"],
                &[],
                "203.0.113.5",
            ),
            (
                &["10.0.0.0/8"],
                "[::ffff:10.1.2.3]:5",
                &["203.0.113.9"],
                &[],
                "203.0.113.9",
            ),
            (
                &["10.0.0.0/8"],
                "203.0.113.7:4000",
                &["127.0.0.1"],
                &["127.0.0.1"],
                "203.0.113.7",
            ),
            (
                &["10.0.0.0/8", "::1"],
                "[::1]:1",
                &["127.0.0.1"],
                &[],
                "127.0.0.1",
            ),
            (
                &["::ffff:0:0/96"],
                "10.1.2.3:5",
                &["127.0.0.1"],
                &[],
                "127.0.0.1",
            ),
            (&["::ffff:0:0/96"], "[::1]:1", &["127.0.0.1"], &[], "::1"),
            (
                &["10.0.0.0/8", "bad"],
                "10.1.2.3:5",
                &["127.0.0.1"],
                &[],
                "10.1.2.3",
            ),
            (
                &["127.0.0.1/8"],
                "127.0.0.1:1",
                &["203.0.113.9"],
                &[],
                "203.0.113.9",
            ),
            (
                &["0.0.0.0/0", "::/0"],
                "203.0.113.7:4000",
                &["127.0.0.1"],
                &[],
                "127.0.0.1",
            ),
            (
                &["0.0.0.0/0", "::/0"],
                "[2001:db8::1]:1",
                &["::1"],
                &[],
                "::1",
            ),
            (
                &["0.0.0.0/0", "::/0"],
                "[fe80::1%1]:80",
                &["127.0.0.1"],
                &[],
                "",
            ),
            (
                &["1.2.3.4/33"],
                "127.0.0.1:1",
                &["203.0.113.9"],
                &[],
                "127.0.0.1",
            ),
            (
                &[" 10.0.0.1"],
                "10.1.2.3:5",
                &["127.0.0.1"],
                &[],
                "10.1.2.3",
            ),
            (
                &["::ffff:10.0.0.0/104"],
                "10.1.2.3:5",
                &["127.0.0.1"],
                &[],
                "127.0.0.1",
            ),
            (&["0.0.0.0/0"], "[::1]:1", &["127.0.0.1"], &[], "::1"),
            (
                &["0.0.0.0/0"],
                "[::ffff:10.1.2.3]:5",
                &["127.0.0.1"],
                &[],
                "127.0.0.1",
            ),
        ];
        for &(trusted, peer, forwarded, real, want) in cases {
            let mut headers = HeaderMap::new();
            for value in forwarded {
                let value = HeaderValue::from_bytes(value.as_bytes()).unwrap();
                headers.append("x-forwarded-for", value);
            }
            for value in real {
                let value = HeaderValue::from_bytes(value.as_bytes()).unwrap();
                headers.append("x-real-ip", value);
            }
            let got = client_ip(Some(peer.parse().unwrap()), &headers, &proxies(trusted));
            assert_eq!(got, want, "{trusted:?} {peer} {forwarded:?} {real:?}");
        }
    }
}
