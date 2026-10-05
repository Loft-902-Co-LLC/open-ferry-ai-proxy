// Ported from Go's net/url/url.go (Parse, parse, getScheme, parseAuthority,
// parseHost, unescape, shouldEscape, validOptionalPort, validUserinfo,
// stringContainsCTLByte) and net/netip/netip.go (ParseAddr) (go1.27,
// BSD-3-Clause), as CLIProxyAPI internal/api/handlers/management/
// api_tools.go (APICall) and sdk/proxyutil/proxy.go (Parse) use them
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! URLs as Go's `url.Parse` reads them, for deciding what `api-call`
//! accepts: its `url` and `proxy_url`, and the `Location` of a redirect.
//!
//! Only what the decisions need is kept: the scheme, in lower case; the
//! host, `%`-escapes decoded; and the user name and password, decoded.
//! The URL that is sent is the `url` crate's reading of the same text.
//!
//! Go reads a host with two colons or more strictly only for `http` and
//! `https` (`urlstrictcolons=1`, the default since Go 1.26, which upstream
//! builds with): the port starts at the first colon and must be digits.
//!
//! Deviations from upstream: none.

/// What a parsed URL holds that the management API looks at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GoUrl {
    /// The scheme, in lower case, or empty.
    pub(crate) scheme: String,
    /// The host and port, decoded, or empty.
    pub(crate) host: Vec<u8>,
    /// The user name and password, decoded, when the URL has an `@`.
    pub(crate) user: Option<(Vec<u8>, Option<Vec<u8>>)>,
}

impl GoUrl {
    /// Whether the URL is absolute: it has a scheme (Go's `URL.IsAbs`).
    pub(crate) fn is_abs(&self) -> bool {
        !self.scheme.is_empty()
    }
}

/// Go's `url.Parse`, or `None` where it fails.
pub(crate) fn parse(raw: &[u8]) -> Option<GoUrl> {
    let (url, fragment) = match raw.iter().position(|&b| b == b'#') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, &b""[..]),
    };
    let parsed = parse_url(url)?;
    if !fragment.is_empty() {
        unescape(fragment, Mode::Fragment)?;
    }
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
    let rest = if rest.ends_with(b"?") && questions == 1 {
        &rest[..rest.len() - 1]
    } else {
        cut(rest, b'?').0
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
    if (!url.scheme.is_empty() || !rest.starts_with(b"///")) && rest.starts_with(b"//") {
        let authority = &rest[2..];
        let (authority, tail) = match authority.iter().position(|&b| b == b'/') {
            Some(i) => (&authority[..i], &authority[i..]),
            None => (authority, &b""[..]),
        };
        path = tail;
        let (user, host) = parse_authority(&url.scheme, authority)?;
        url.user = user;
        url.host = host;
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
                return Some((&raw[..i], &raw[i + 1..]));
            }
            _ => return Some((b"", raw)),
        }
    }
    Some((b"", raw))
}

/// The bytes before the first `sep` and those after it.
fn cut(s: &[u8], sep: u8) -> (&[u8], &[u8]) {
    match s.iter().position(|&b| b == sep) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, b""),
    }
}

type User = Option<(Vec<u8>, Option<Vec<u8>>)>;

/// Go's `parseAuthority`: the user information and the host.
fn parse_authority(scheme: &str, authority: &[u8]) -> Option<(User, Vec<u8>)> {
    let at = authority.iter().rposition(|&b| b == b'@');
    let host = parse_host(scheme, at.map_or(authority, |i| &authority[i + 1..]))?;
    let Some(at) = at else {
        return Some((None, host));
    };
    let userinfo = &authority[..at];
    if !valid_userinfo(userinfo) {
        return None;
    }
    let user = match userinfo.iter().position(|&b| b == b':') {
        None => (unescape(userinfo, Mode::UserPassword)?, None),
        Some(i) => (
            unescape(&userinfo[..i], Mode::UserPassword)?,
            Some(unescape(&userinfo[i + 1..], Mode::UserPassword)?),
        ),
    };
    Some((Some(user), host))
}

/// Go's `parseHost`: `host[:port]`, decoded.
fn parse_host(scheme: &str, host: &[u8]) -> Option<Vec<u8>> {
    match host.iter().rposition(|&b| b == b'[') {
        Some(open) if open > 0 => return None,
        Some(_) => {
            // An IP literal, as in "[fe80::1%25en0]:80".
            let close = host.iter().rposition(|&b| b == b']')?;
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return None;
            }
            let colon_port = unescape(colon_port, Mode::Host)?;
            let hostname = host.get(1..close)?;
            let unescaped = match find(hostname, b"%25") {
                Some(zone) => {
                    let mut host_part = unescape(&hostname[..zone], Mode::Host)?;
                    host_part.extend(unescape(&hostname[zone..], Mode::Zone)?);
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
        if !valid_optional_port(&host[start..]) {
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
        Some(i) => (&text[..i], Some(&text[i + 1..])),
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

fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

/// Go's `unescape`: `s` with its `%`-escapes decoded, or `None` for a
/// malformed escape or, in a host, a character a host can't hold.
fn unescape(s: &[u8], mode: Mode) -> Option<Vec<u8>> {
    let host_like = matches!(mode, Mode::Host | Mode::Zone);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'%' {
            if i + 2 >= s.len() || !s[i + 1].is_ascii_hexdigit() || !s[i + 2].is_ascii_hexdigit() {
                return None;
            }
            let escape = &s[i..i + 3];
            let value = unhex(s[i + 1]) << 4 | unhex(s[i + 2]);
            // A host may escape only non-ASCII bytes, and "%25" in a zone.
            if mode == Mode::Host && unhex(s[i + 1]) < 8 && escape != b"%25" {
                return None;
            }
            if mode == Mode::Zone && escape != b"%25" && value != b' ' && should_escape_host(value)
            {
                return None;
            }
            out.push(value);
            i += 3;
            continue;
        }
        if host_like && c < 0x80 && should_escape_host(c) {
            return None;
        }
        out.push(c);
        i += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(raw: &str) -> Option<String> {
        parse(raw.as_bytes()).map(|url| String::from_utf8(url.host).unwrap())
    }

    #[test]
    fn absolute_urls() {
        let url = parse(b"HTTPS://u%41:p%3A@Example.com:8443/a%20b?x#y").unwrap();
        assert_eq!(url.scheme, "https");
        assert_eq!(url.host, b"Example.com:8443");
        assert_eq!(url.user, Some((b"uA".to_vec(), Some(b"p:".to_vec()))));
        assert_eq!(host("http://h"), Some("h".into()));
        assert_eq!(host("http://h:"), Some("h:".into()));
        assert_eq!(host("http://@h/"), Some("h".into()));
        assert_eq!(parse(b"http://@h/").unwrap().user, Some((Vec::new(), None)));
        assert_eq!(host("http://a@b@h/"), Some("h".into()));
        assert_eq!(host("http:///path"), Some(String::new()));
        assert_eq!(host("http://?q"), Some(String::new()));
        assert_eq!(host("http:opaque"), Some(String::new()));
        assert_eq!(host("//h/p"), Some("h".into()));
        assert_eq!(host("ws://a:1:2"), Some("a:1:2".into()));
    }

    #[test]
    fn invalid_urls() {
        for raw in [
            ":x",
            "http://h/\x01",
            "http://h/%zz",
            "http://h/%4",
            "http://h#%",
            "http://h:x",
            "http://a:1:2",
            "http://h h/",
            "http://h%41/",
            "http://h{/",
            "http://us er@h/",
            "http://\u{e9}@h/",
            "http://x[::1]/",
            "http://[::1/",
            "http://[::1]x/",
            "http://[1.2.3.4]/",
            "http://[fe80::1%25]/",
            "http://[%25en0]/",
            "http://[fe80::1%25%0a]/",
            // A relative URL whose first segment has a colon.
            "1a:b",
        ] {
            assert_eq!(parse(raw.as_bytes()), None, "{raw:?}");
        }
        assert!(parse(b"./a:b").is_some());
        assert!(parse(b"a:b/c:d").is_some());
    }

    #[test]
    fn ip_literals() {
        assert_eq!(host("http://[::1]:80/"), Some("[::1]:80".into()));
        assert_eq!(
            host("http://[fe80::1%25en0]/"),
            Some("[fe80::1%en0]".into())
        );
        assert_eq!(
            host("http://[fe80::1%25%20x]/"),
            Some("[fe80::1% x]".into())
        );
        assert_eq!(
            host("http://[::ffff:1.2.3.4]/"),
            Some("[::ffff:1.2.3.4]".into())
        );
        assert_eq!(host("http://h%c3%a9/"), Some("h\u{e9}".into()));
        assert_eq!(host("http://h\u{e9}/"), Some("h\u{e9}".into()));
    }

    /// Go's answers (go1.27.1 building for go 1.26.0, so with
    /// `urlstrictcolons=1`): `err` where `url.Parse` fails, else the
    /// scheme, the host, and the user name and password.
    #[test]
    fn parse_matches_go() {
        let cases: &[(&[u8], &str)] = &[
            (b"http://example.com", "http|example.com|-"),
            (
                b"https://example.com:443/path?q=1#frag",
                "https|example.com:443|-",
            ),
            (b"http://", "http||-"),
            (b"http://a b", "err"),
            (b"http://a%20b", "err"),
            (b"http://%41", "err"),
            (b"http://a%zz", "err"),
            (b"/relative", "||-"),
            (b"//host/path", "|host|-"),
            (b"example.com", "||-"),
            (b"example.com:80", "example.com||-"),
            (b"localhost:8080/x", "localhost||-"),
            (b"http:example.com", "http||-"),
            (b"http://user:pass@host", "http|host|user:pass"),
            (b"http://user@host", "http|host|user:-"),
            (b"http://us%40er:p%3Ass@host", "http|host|us@er:p:ss"),
            (b"http://user:pa ss@host", "err"),
            (b"http://[::1]:80", "http|[::1]:80|-"),
            (b"http://[::1]", "http|[::1]|-"),
            (b"http://[::1", "err"),
            (b"http://::1", "err"),
            (b"http://a:b:c", "err"),
            (b"http://host:port", "err"),
            (b"http://host:", "http|host:|-"),
            (b"http://host:80:90", "err"),
            (b"ftp://a:b:c", "err"),
            (b"socks5://a:b:c", "err"),
            (b"http://[fe80::1%25en0]:8080", "http|[fe80::1%en0]:8080|-"),
            (b"http://[fe80::1%en0]", "err"),
            (b"http://[fe80::1%25]", "err"),
            (b"http://1.2.3.4", "http|1.2.3.4|-"),
            (b"http://[1.2.3.4]", "err"),
            (b"http://[::ffff:1.2.3.4]", "http|[::ffff:1.2.3.4]|-"),
            (b"HTTP://EXAMPLE.COM", "http|EXAMPLE.COM|-"),
            (b"hTtP://x", "http|x|-"),
            (b"1http://x", "err"),
            (b"-x://y", "err"),
            (b"a+b.c-d://h", "a+b.c-d|h|-"),
            (b"http://x/%zz", "err"),
            (b"http://x/?%zz", "http|x|-"),
            (b"http://x/#%zz", "err"),
            (b"http://x#%zz", "err"),
            (b"http://x/\x7f", "err"),
            (b"http://x/\x00", "err"),
            (b"http://x\t", "err"),
            (b" http://x", "err"),
            (b"http://x ", "err"),
            (b"http://\xc3\xa9.com", "http|\u{e9}.com|-"),
            (b"http://%C3%A9.com", "http|\u{e9}.com|-"),
            (b"http://x:65536", "http|x:65536|-"),
            (b"http://x:0080", "http|x:0080|-"),
            (b"*", "||-"),
            (b"http://a@b@c", "http|c|a@b:-"),
            (b"http://a/b@c", "http|a|-"),
            (b"mailto:joe@x", "mailto||-"),
            (b"http://[::1]:80x", "err"),
            (b"http://[::1]x", "err"),
            (b"http://[]", "err"),
            (b"http://[v1.fe]", "err"),
            (b"http://a]b", "http|a]b|-"),
            (b"http://a[b", "err"),
            (b"http://a%2Fb", "err"),
            (b"http://a%3Ab", "err"),
            (b"http://h/p;q", "http|h|-"),
            (b"http://h?x", "http|h|-"),
            (b"http://h#", "http|h|-"),
            (b"#frag", "||-"),
            (b"", "||-"),
            (b"?q", "||-"),
            (b"http://x:y@", "http||x:y"),
            (b"http://:80", "http|:80|-"),
            (b"http://@host", "http|host|:-"),
            (b"http://%", "err"),
            (b"http://h%2", "err"),
            (b"https://h:443:", "err"),
            (b"http://h:+1", "err"),
            (b"http://h:-1", "err"),
            (b"http://h: 80", "err"),
            (b"javascript:alert(1)", "javascript||-"),
            (b"http:/x", "http||-"),
            (b"http:///x", "http||-"),
            (b"http:////x", "http||-"),
            (b"file:///etc", "file||-"),
            (b"file://host/x", "file|host|-"),
            (b"http://h\\x", "err"),
            (b"http://h|x", "err"),
            (b"http://h{x}", "err"),
            (b"http://h^x", "err"),
            (b"http://h`x", "err"),
            (b"http://h\"x", "http|h\"x|-"),
            (b"http://h'x", "http|h'x|-"),
            (b"http://h<x>", "http|h<x>|-"),
            (b"http://h!$&'()*+,;=x", "http|h!$&'()*+,;=x|-"),
            (b"http://h~x", "http|h~x|-"),
            (b"http://h_x", "http|h_x|-"),
            (b"http://a%00b", "err"),
            (b"http://a%7fb", "err"),
            (b"socks5://u:p@127.0.0.1:1080", "socks5|127.0.0.1:1080|u:p"),
            (b"socks5h://h:1", "socks5h|h:1|-"),
            (b"http://[::1%25lo]:1", "http|[::1%lo]:1|-"),
            (b"http://[::1]:", "http|[::1]:|-"),
            (b"http://[::1]::", "err"),
            (b"https://[::1]:a", "err"),
            (b"ftp://[::1]:a", "err"),
            (b"ftp://h:a", "err"),
            (b"ftp://h::", "ftp|h::|-"),
            (b"custom://a:b:c:d", "err"),
            (b"http://user:p@ss@host", "http|host|user:p@ss"),
            (b"http://us er@host", "err"),
            (b"http://%zz@host", "err"),
            (b"http://u:%zz@host", "err"),
            (b"http://\xc3\xa9@host", "err"),
            (b"http://h/%2", "err"),
            (b"http://h/a%2Fb", "http|h|-"),
            (b"http://h?q=%zz#f", "http|h|-"),
            (b"HTTPS://H.COM:8443", "https|H.COM:8443|-"),
            (b"Http://[::1]:8080/p", "http|[::1]:8080|-"),
            (b"http://h/\x80", "http|h|-"),
            (b"http://h#\x80", "http|h|-"),
            (b"http://x/\xe2\x80\xa8", "http|x|-"),
            (b"http://h:80/p?q#f#g", "http|h:80|-"),
            (b"http://[2001:db8::1]:80", "http|[2001:db8::1]:80|-"),
            (b"http://[2001:db8::1%25eth0]", "http|[2001:db8::1%eth0]|-"),
            (
                b"http://[::ffff:127.0.0.1]:1",
                "http|[::ffff:127.0.0.1]:1|-",
            ),
            (b"http://[0:0:0:0:0:0:0:1]", "http|[0:0:0:0:0:0:0:1]|-"),
            (b"http://[1:2:3:4:5:6:7:8:9]", "err"),
            (b"http://[1.2.3.4.5]", "err"),
            (b"http://[::1]%41", "err"),
            (b"http://h%41:80", "err"),
            (b"http://h:8%30", "err"),
            (b"ws://h:1:2", "ws|h:1:2|-"),
            (b"HTTP://h:1:2", "err"),
            (b"http+x://h:1:2", "http+x|h:1:2|-"),
        ];
        for &(raw, want) in cases {
            let got = match parse(raw) {
                None => "err".to_owned(),
                Some(url) => {
                    let user = match &url.user {
                        None => "-".to_owned(),
                        Some((name, password)) => format!(
                            "{}:{}",
                            String::from_utf8_lossy(name),
                            password
                                .as_deref()
                                .map_or_else(|| "-".into(), String::from_utf8_lossy)
                        ),
                    };
                    let host = String::from_utf8_lossy(&url.host);
                    format!("{}|{host}|{user}", url.scheme)
                }
            };
            assert_eq!(got, want, "{}", String::from_utf8_lossy(raw));
        }

        // The printable ASCII bytes Go accepts between a prefix and a
        // suffix.
        let positions = [
            (
                "http://a",
                "b",
                "!\"#$&'()*+,-./0123456789;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ]_abcdefghijklmnopqrstuvwxyz~",
            ),
            (
                "http://a",
                "b@h",
                "!#$&'()*+,-./0123456789:;=?@ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~",
            ),
            ("http://h:", "", "#/0123456789?@"),
            (
                "http://u:a",
                "b@h",
                "!$&'()*+,-.0123456789:;=@ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz~",
            ),
            (
                "ftp://a",
                "b",
                "!\"#$&'()*+,-./0123456789;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ]_abcdefghijklmnopqrstuvwxyz~",
            ),
        ];
        for (prefix, suffix, accepted) in positions {
            for c in 0x20..0x80u8 {
                let raw = [prefix.as_bytes(), &[c], suffix.as_bytes()].concat();
                let want = accepted.as_bytes().contains(&c);
                assert_eq!(
                    parse(&raw).is_some(),
                    want,
                    "{}",
                    String::from_utf8_lossy(&raw)
                );
            }
        }
    }
}
