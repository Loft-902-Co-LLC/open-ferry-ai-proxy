// Ported from Go's net/url/url.go (Parse, parse, getScheme, parseAuthority,
// parseHost, unescape, shouldEscape, validOptionalPort, validUserinfo,
// stringContainsCTLByte) and net/netip/netip.go (ParseAddr) (go1.26,
// BSD-3-Clause), as CLIProxyAPI internal/watcher/diff/config_diff.go
// (formatURL) uses them (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! URLs as Go's `url.Parse` reads them, for the scheme and host a change
//! line shows of a base or proxy URL, and for the video handler's check of
//! the URL a finished video is fetched from.
//!
//! Only the scheme, in lower case, and the host, `%`-escapes decoded, are
//! kept; the rest is only checked, so a URL Go refuses is refused here too.
//! Go reads a host with two colons or more strictly only for `http` and
//! `https` (`urlstrictcolons=1`, the default since Go 1.26, which upstream
//! builds with): the port starts at the first colon and must be digits.
//!
//! This is the management crate's `go_url` without the user information,
//! which that crate keeps to itself.
//!
//! Deviations from upstream: none.

/// What a change line shows of a parsed URL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoUrl {
    /// The scheme, in lower case, or empty.
    pub scheme: String,
    /// The host and port, decoded, or empty.
    pub host: Vec<u8>,
}

/// Go's `url.Parse`, or `None` where it fails.
pub fn parse(raw: &[u8]) -> Option<GoUrl> {
    let (url, fragment) = cut(raw, b'#');
    let parsed = parse_url(url)?;
    unescape(fragment, Mode::Fragment)?;
    Some(parsed)
}

/// Go's `parse` for a URL without its fragment, not from a request line.
fn parse_url(raw: &[u8]) -> Option<GoUrl> {
    if raw.iter().any(|&b| b < b' ' || b == 0x7f) {
        return None;
    }
    let mut url = GoUrl::default();
    if raw == b"*" {
        return Some(url);
    }
    let (scheme, rest) = scheme(raw)?;
    url.scheme = String::from_utf8_lossy(scheme).to_ascii_lowercase();

    let questions = rest.iter().filter(|&&b| b == b'?').count();
    let rest = match rest.strip_suffix(b"?") {
        Some(rest) if questions == 1 => rest,
        _ => cut(rest, b'?').0,
    };

    if !rest.starts_with(b"/") {
        if !url.scheme.is_empty() {
            // A rootless path: opaque, no host.
            return Some(url);
        }
        if cut(rest, b'/').0.contains(&b':') {
            // A colon in the first segment of a relative URL.
            return None;
        }
    }

    let mut path = rest;
    if let Some(authority) = rest.strip_prefix(b"//")
        && (!url.scheme.is_empty() || !rest.starts_with(b"///"))
    {
        let split = authority
            .iter()
            .position(|&b| b == b'/')
            .unwrap_or(authority.len());
        let (authority, tail) = authority.split_at(split);
        path = tail;
        url.host = parse_authority(&url.scheme, authority)?;
    }
    unescape(path, Mode::Path)?;
    Some(url)
}

/// Go's `getScheme`: the scheme and the rest, an empty scheme when there
/// is none, or `None` for a URL starting with a colon.
fn scheme(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    for (i, &c) in raw.iter().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if i == 0 {
                    return Some((b"", raw));
                }
            }
            b':' => {
                if i == 0 {
                    return None;
                }
                let (scheme, rest) = raw.split_at(i);
                return Some((scheme, rest.get(1..).unwrap_or_default()));
            }
            _ => return Some((b"", raw)),
        }
    }
    Some((b"", raw))
}

/// The bytes before the first `sep` and those after it.
fn cut(s: &[u8], sep: u8) -> (&[u8], &[u8]) {
    match s.iter().position(|&b| b == sep) {
        Some(i) => {
            let (before, after) = s.split_at(i);
            (before, after.get(1..).unwrap_or_default())
        }
        None => (s, b""),
    }
}

/// Go's `parseAuthority`: the host, once the user information is checked.
fn parse_authority(scheme: &str, authority: &[u8]) -> Option<Vec<u8>> {
    let Some(at) = authority.iter().rposition(|&b| b == b'@') else {
        return parse_host(scheme, authority);
    };
    let (userinfo, host) = authority.split_at(at);
    let host = parse_host(scheme, host.get(1..).unwrap_or_default())?;
    if !valid_userinfo(userinfo) {
        return None;
    }
    let (user, password) = cut(userinfo, b':');
    unescape(user, Mode::UserPassword)?;
    unescape(password, Mode::UserPassword)?;
    Some(host)
}

/// Go's `parseHost`: `host[:port]`, decoded.
fn parse_host(scheme: &str, host: &[u8]) -> Option<Vec<u8>> {
    match host.iter().rposition(|&b| b == b'[') {
        Some(open) if open > 0 => return None,
        Some(_) => {
            // An IP literal, as in "[fe80::1%25en0]:80".
            let close = host.iter().rposition(|&b| b == b']')?;
            let colon_port = host.get(close + 1..)?;
            if !valid_optional_port(colon_port) {
                return None;
            }
            let colon_port = unescape(colon_port, Mode::Host)?;
            let hostname = host.get(1..close)?;
            let unescaped = match find(hostname, b"%25") {
                Some(zone) => {
                    let (address, zone) = hostname.split_at(zone);
                    let mut host_part = unescape(address, Mode::Host)?;
                    host_part.extend(unescape(zone, Mode::Zone)?);
                    host_part
                }
                None => unescape(hostname, Mode::Host)?,
            };
            if !is_ipv6_literal(&unescaped) {
                return None;
            }
            let mut out = Vec::with_capacity(unescaped.len() + colon_port.len() + 2);
            out.push(b'[');
            out.extend(unescaped);
            out.push(b']');
            out.extend(colon_port);
            return Some(out);
        }
        None => {}
    }
    if let Some(first) = host.iter().position(|&b| b == b':') {
        let last = host.iter().rposition(|&b| b == b':').unwrap_or(first);
        let strict = scheme == "http" || scheme == "https";
        let start = if strict { first } else { last };
        if !valid_optional_port(host.get(start..).unwrap_or_default()) {
            return None;
        }
    }
    unescape(host, Mode::Host)
}

/// Where `needle` first appears in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether a bracketed host is what Go's `netip.ParseAddr` reads as an
/// address other than IPv4: an IPv6 address, with a zone if any.
fn is_ipv6_literal(text: &[u8]) -> bool {
    // Go reads an IPv4 address, an IPv6 one or an error by the first of
    // these it meets.
    if text.iter().find(|&&b| matches!(b, b'.' | b':' | b'%')) != Some(&b':') {
        return false;
    }
    let (address, zone) = match text.iter().position(|&b| b == b'%') {
        Some(i) => {
            let (address, zone) = text.split_at(i);
            (address, Some(zone.get(1..).unwrap_or_default()))
        }
        None => (text, None),
    };
    if zone.is_some_and(<[u8]>::is_empty) {
        return false;
    }
    std::str::from_utf8(address).is_ok_and(|a| a.parse::<std::net::Ipv6Addr>().is_ok())
}

/// Go's `validOptionalPort`: empty, or a colon and digits.
fn valid_optional_port(port: &[u8]) -> bool {
    match port.split_first() {
        None => true,
        Some((b':', digits)) => digits.iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

/// Go's `validUserinfo`.
fn valid_userinfo(s: &[u8]) -> bool {
    s.iter().all(|&b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b':'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'%'
                    | b'@'
            )
    })
}

/// The parts of a URL Go unescapes differently.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Path,
    Host,
    Zone,
    UserPassword,
    Fragment,
}

/// Go's `shouldEscape` in host and zone mode.
fn should_escape_host(c: u8) -> bool {
    !(c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
                | b'-'
                | b'_'
                | b'.'
                | b'~'
        ))
}

/// The value of a hex digit; only called on one.
fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c.wrapping_sub(b'A').wrapping_add(10),
    }
}

/// Go's `unescape`: `s` with its `%`-escapes decoded, or `None` for a
/// malformed escape or, in a host, a character a host can't hold.
fn unescape(s: &[u8], mode: Mode) -> Option<Vec<u8>> {
    let host_like = matches!(mode, Mode::Host | Mode::Zone);
    let mut out = Vec::with_capacity(s.len());
    let mut rest = s;
    while let Some((&c, tail)) = rest.split_first() {
        if c == b'%' {
            let (&high, &low) = match tail {
                [high, low, ..] if high.is_ascii_hexdigit() && low.is_ascii_hexdigit() => {
                    (high, low)
                }
                _ => return None,
            };
            let value = unhex(high) << 4 | unhex(low);
            let is_25 = high == b'2' && low == b'5';
            // A host may escape only non-ASCII bytes, and "%25" in a zone.
            if mode == Mode::Host && unhex(high) < 8 && !is_25 {
                return None;
            }
            if mode == Mode::Zone && !is_25 && value != b' ' && should_escape_host(value) {
                return None;
            }
            out.push(value);
            rest = tail.get(2..).unwrap_or_default();
            continue;
        }
        if host_like && c < 0x80 && should_escape_host(c) {
            return None;
        }
        out.push(c);
        rest = tail;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(raw: &str) -> Option<String> {
        parse(raw.as_bytes()).map(|url| String::from_utf8_lossy(&url.host).into_owned())
    }

    // Not upstream's: what Go 1.26's url.Parse gives for the URLs a change
    // line shows.
    #[test]
    fn parse_reads_hosts_as_go_does() {
        assert_eq!(
            host("http://user:pass@example.com:8080/p?x=1#f").as_deref(),
            Some("example.com:8080")
        );
        assert_eq!(
            host("socks5://192.168.1.1:1080/path").as_deref(),
            Some("192.168.1.1:1080")
        );
        assert_eq!(host("http://[::1]:80/").as_deref(), Some("[::1]:80"));
        assert_eq!(
            host("http://ex%C3%A9.com/").as_deref(),
            Some("ex\u{e9}.com")
        );
        // "example.com" is read as a scheme.
        assert_eq!(host("example.com:1234/path").as_deref(), Some(""));
        assert_eq!(host("/just/path").as_deref(), Some(""));
        assert_eq!(host("old/repo").as_deref(), Some(""));
        assert_eq!(
            parse(b"HTTP://h").map(|u| u.scheme).as_deref(),
            Some("http")
        );
        assert_eq!(parse(b"x:opaque").map(|u| u.host), Some(Vec::new()));
        for bad in [
            "http://[::1",
            ":no-scheme",
            "http://a b/",
            "http://h:80:90/",
            "http://h/%zz",
            "http://h/#%z",
            "http://us er@h/",
            "http://h%41/",
            "http://x[::1]/",
            "http://[1.2.3.4]/",
            "a\u{7f}b",
        ] {
            assert_eq!(parse(bad.as_bytes()), None, "{bad}");
        }
        // Outside http and https, the port starts at the last colon.
        assert_eq!(host("socks5://h:x:1080").as_deref(), Some("h:x:1080"));
    }
}
