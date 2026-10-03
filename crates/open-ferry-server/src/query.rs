//! Query strings, read as Go's `url.ParseQuery` reads them.

/// The parameters of `query` in order, as Go's `url.ParseQuery` reads them:
/// pairs are split on `&`, a pair holding `;` or a bad `%` escape is dropped,
/// and `+` is a space. Escapes that decode to invalid UTF-8 give U+FFFD,
/// where Go keeps the bytes.
pub(crate) fn parse(query: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() || pair.contains(';') {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if let (Some(key), Some(value)) = (unescape(key), unescape(value)) {
            params.push((key, value));
        }
    }
    params
}

/// The first value of `name` in `params`, as Go's `Values.Get` gives it.
pub(crate) fn first<'p>(params: &'p [(String, String)], name: &str) -> Option<&'p str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Go's `url.QueryUnescape`, or `None` where it fails.
fn unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex(*bytes.get(i + 1)?)?;
                let lo = hex(*bytes.get(i + 2)?)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_as_go_does() {
        let params = parse("key=a+b%21&&x&bad=%zz&semi=1;2&key=second&=v&e=");
        let pairs: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("key", "a b!"),
                ("x", ""),
                ("key", "second"),
                ("", "v"),
                ("e", "")
            ]
        );
        assert_eq!(first(&params, "key"), Some("a b!"));
        assert_eq!(first(&params, "e"), Some(""));
        assert_eq!(first(&params, "missing"), None);
    }
}
