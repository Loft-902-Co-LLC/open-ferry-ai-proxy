// Ported from Go's net/url/url.go (Parse and its helpers, with
// stringContainsCTLByte) and the IPv6 and zone checks of net/netip's
// ParseAddr (go1.26.4, BSD-3-Clause), as CLIProxyAPI internal/misc/oauth.go
// uses them (v8.0.27, MIT).
// https://github.com/golang/go
// https://github.com/router-for-me/CLIProxyAPI

//! The validation performed by Go's `url.Parse`, retaining only the raw query
//! and fragment. Decoded paths, hosts and userinfo are not used by callbacks.

use super::CallbackParseError;
use super::CallbackParseError::InvalidUrl;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Path,
    Fragment,
    Host,
    Zone,
    UserPassword,
}

// Parse cuts the fragment before parse checks stringContainsCTLByte.
pub(super) fn parse(raw_url: &str) -> Result<(&str, &str), CallbackParseError> {
    let (raw_url, fragment) = raw_url.split_once('#').unwrap_or((raw_url, ""));
    let query = parse_url(raw_url)?;
    // setFragment: validate escapes, but preserve raw bytes for L16's one decode.
    unescape(fragment, Encoding::Fragment)?;
    Ok((query, fragment))
}

// parse(rawURL, false); the request-URI branch is not used for pasted callbacks.
fn parse_url(raw_url: &str) -> Result<&str, CallbackParseError> {
    if raw_url.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(InvalidUrl);
    }
    if raw_url == "*" {
        return Ok("");
    }
    let (scheme, rest) = get_scheme(raw_url)?;
    // ForceQuery's trailing '?' has the same empty RawQuery as this cut.
    let (mut rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // An opaque scheme:rest does not go through setPath.
            return Ok(query);
        }
        if rest.split('/').next().unwrap_or("").contains(':') {
            return Err(InvalidUrl);
        }
    }
    if (!scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let mut authority = &rest[2..];
        rest = "";
        if let Some(index) = authority.find('/') {
            rest = &authority[index..];
            authority = &authority[..index];
        }
        parse_authority(scheme, authority)?;
    }
    // setPath checks escapes before any path normalization.
    unescape(rest, Encoding::Path)?;
    Ok(query)
}

// getScheme: [a-zA-Z][a-zA-Z0-9+.-]*, or a relative reference.
fn get_scheme(raw_url: &str) -> Result<(&str, &str), CallbackParseError> {
    for (index, byte) in raw_url.bytes().enumerate() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if index == 0 {
                    return Ok(("", raw_url));
                }
            }
            b':' => {
                if index == 0 {
                    return Err(InvalidUrl);
                }
                return Ok((&raw_url[..index], &raw_url[index + 1..]));
            }
            _ => return Ok(("", raw_url)),
        }
    }
    Ok(("", raw_url))
}

// parseAuthority validates the host first, then validUserinfo and unescape.
fn parse_authority(scheme: &str, authority: &str) -> Result<(), CallbackParseError> {
    let (userinfo, host) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(user, host)| (Some(user), host));
    parse_host(scheme, host)?;
    if let Some(userinfo) = userinfo {
        if !valid_userinfo(userinfo) {
            return Err(InvalidUrl);
        }
        let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        unescape(username, Encoding::UserPassword)?;
        unescape(password, Encoding::UserPassword)?;
    }
    Ok(())
}

// parseHost, including Go 1.26.4's default strict colons for HTTP(S).
fn parse_host(scheme: &str, host: &str) -> Result<(), CallbackParseError> {
    match host.rfind('[') {
        Some(0) => {
            let close = host.rfind(']').ok_or(InvalidUrl)?;
            let port = &host[close + 1..];
            if !valid_optional_port(port) {
                return Err(InvalidUrl);
            }
            unescape(port, Encoding::Host)?;
            let hostname = &host[1..close];
            let decoded = if let Some(zone) = hostname.find("%25") {
                let mut decoded = unescape(&hostname[..zone], Encoding::Host)?;
                decoded.extend(unescape(&hostname[zone..], Encoding::Zone)?);
                decoded
            } else {
                unescape(hostname, Encoding::Host)?
            };
            // parseHost with netip.ParseAddr: brackets require IPv6, with an
            // optional nonempty zone. Zone bytes need not be UTF-8; the address
            // itself is ASCII.
            let address = if let Some(zone) = decoded.iter().position(|byte| *byte == b'%') {
                if zone + 1 == decoded.len() {
                    return Err(InvalidUrl);
                }
                &decoded[..zone]
            } else {
                &decoded
            };
            std::str::from_utf8(address)
                .map_err(|_| InvalidUrl)?
                .parse::<std::net::Ipv6Addr>()
                .map_err(|_| InvalidUrl)?;
            return Ok(());
        }
        Some(_) => return Err(InvalidUrl),
        None => {}
    }
    if let Some(first) = host.find(':') {
        let colon = if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            first
        } else {
            host.rfind(':').unwrap()
        };
        if !valid_optional_port(&host[colon..]) {
            return Err(InvalidUrl);
        }
    }
    unescape(host, Encoding::Host)?;
    Ok(())
}

// validOptionalPort checks digits, not the numeric value or a u16 bound.
fn valid_optional_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
}

// validUserinfo permits ASCII letters, digits, RFC 3986 unreserved and
// sub-delims bytes, ':' and '%', plus '@' (Go's compatibility exception).
fn valid_userinfo(userinfo: &str) -> bool {
    userinfo
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&byte))
}

// unescape: path, fragment and userinfo only require well-formed escapes.
// Host and zone modes additionally restrict raw ASCII and escaped host bytes.
fn unescape(value: &str, mode: Encoding) -> Result<Vec<u8>, CallbackParseError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let high = bytes.get(index + 1).and_then(|b| (*b as char).to_digit(16));
            let low = bytes.get(index + 2).and_then(|b| (*b as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(InvalidUrl);
            };
            let byte = (high * 16 + low) as u8;
            let percent = &bytes[index..index + 3] == b"%25";
            if mode == Encoding::Host && byte < 0x80 && !percent {
                return Err(InvalidUrl);
            }
            if mode == Encoding::Zone
                && !percent
                && byte != b' '
                && should_escape(byte, Encoding::Host)
            {
                return Err(InvalidUrl);
            }
            decoded.push(byte);
            index += 3;
        } else {
            if matches!(mode, Encoding::Host | Encoding::Zone)
                && byte < 0x80
                && should_escape(byte, mode)
            {
                return Err(InvalidUrl);
            }
            decoded.push(byte);
            index += 1;
        }
    }
    Ok(decoded)
}

// shouldEscape, from net/url's gen_encoding_table.go reference implementation.
fn should_escape(byte: u8, mode: Encoding) -> bool {
    if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
        return false;
    }
    if matches!(mode, Encoding::Host | Encoding::Zone) && b"!$&'()*+,;=:[]<>\"".contains(&byte) {
        return false;
    }
    if b"$&+,/:;=?@".contains(&byte) {
        return match mode {
            Encoding::Path => byte == b'?',
            Encoding::UserPassword => b"@/?:".contains(&byte),
            Encoding::Fragment => false,
            Encoding::Host | Encoding::Zone => true,
        };
    }
    !(mode == Encoding::Fragment && b"!()*".contains(&byte))
}
