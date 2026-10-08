// Ported from gin-gonic/gin v1.10.1 context.go (Query, GetQuery,
// initQueryCache) (MIT) and Go's net/url/url.go (ParseQuery, parseQuery,
// QueryUnescape) (go1.27, BSD-3-Clause), as CLIProxyAPI
// internal/api/handlers/management/auth_files.go (ListAuthFiles,
// GetAuthFileModels) uses them (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! Query parameters, read as gin's `c.Query` and `c.GetQuery` read them
//! through Go's `url.ParseQuery`.
//!
//! Deviations from upstream: none.

/// Go's limit on the number of query parameters; past it nothing is read.
const MAX_PARAMS: usize = 10_000;

/// A request's query parameters, in order. Values are bytes, as in Go, and
/// may not be UTF-8.
#[derive(Debug, Default)]
pub(crate) struct Query {
    params: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Query {
    /// Reads `raw` as Go's `url.ParseQuery` does: pairs are split on `&`, a
    /// pair holding `;` or a bad `%` escape is dropped, and `+` is a space.
    /// More than ten thousand parameters read as none.
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        let raw = raw.unwrap_or_default();
        if raw.bytes().filter(|&b| b == b'&').count() + 1 > MAX_PARAMS {
            return Self::default();
        }
        let mut params = Vec::new();
        for pair in raw.split('&') {
            if pair.is_empty() || pair.contains(';') {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if let (Some(key), Some(value)) = (unescape(key), unescape(value)) {
                params.push((key, value));
            }
        }
        Self { params }
    }

    /// The first value of `name` (gin's `GetQuery`).
    pub(crate) fn get(&self, name: &str) -> Option<&[u8]> {
        self.params
            .iter()
            .find(|(key, _)| key == name.as_bytes())
            .map(|(_, value)| value.as_slice())
    }

    /// The first value of `name`, or empty (gin's `Query`).
    pub(crate) fn value(&self, name: &str) -> &[u8] {
        self.get(name).unwrap_or_default()
    }
}

/// Go's `url.QueryUnescape`, or `None` where it fails.
fn unescape(s: &str) -> Option<Vec<u8>> {
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
    Some(out)
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
        let query = Query::parse(Some(
            "name=a+b%21&&x&bad=%zz&semi=1;2&name=second&=v&e=&raw=%ff",
        ));
        assert_eq!(query.get("name"), Some(&b"a b!"[..]));
        assert_eq!(query.get("x"), Some(&b""[..]));
        assert_eq!(query.get(""), Some(&b"v"[..]));
        assert_eq!(query.get("e"), Some(&b""[..]));
        assert_eq!(query.get("bad"), None);
        assert_eq!(query.get("semi"), None);
        assert_eq!(query.get("raw"), Some(&b"\xff"[..]));
        assert_eq!(query.value("missing"), b"");
        assert!(Query::parse(None).get("name").is_none());
    }

    #[test]
    fn too_many_parameters_read_as_none() {
        let mut raw = "name=x".to_owned();
        raw.push_str(&"&".repeat(MAX_PARAMS - 1));
        assert_eq!(Query::parse(Some(&raw)).get("name"), Some(&b"x"[..]));
        raw.push('&');
        assert_eq!(Query::parse(Some(&raw)).get("name"), None);
    }

    /// Go's answers: gin's `GetQuery` over `url.ParseQuery` (go1.27.1
    /// building for go 1.26.0).
    #[test]
    fn get_matches_go() {
        let cases: &[(&str, &str, Option<&[u8]>)] = &[
            ("name=a", "name", Some(b"a")),
            ("name=a&name=b", "name", Some(b"a")),
            ("name=a+b", "name", Some(b"a b")),
            ("name=a%20b", "name", Some(b"a b")),
            ("name=%zz", "name", None),
            ("name=%zz&name=ok", "name", Some(b"ok")),
            ("name", "name", Some(b"")),
            ("name=", "name", Some(b"")),
            ("=x", "name", None),
            ("=x", "Name", None),
            ("=x", "x;y", None),
            ("=x", "", Some(b"x")),
            ("name=a;b", "name", None),
            ("name=a&x;y=1&name=b", "name", Some(b"a")),
            ("name=a&x;y=1&name=b", "Name", None),
            ("name=a&x;y=1&name=b", "x;y", None),
            ("name=a&x;y=1&name=b", "", None),
            ("a=1&&name=2", "name", Some(b"2")),
            ("a=1&&name=2", "a", Some(b"1")),
            ("name=%E9", "name", Some(b"\xe9")),
            ("na%6De=x", "name", Some(b"x")),
            ("name=a=b", "name", Some(b"a=b")),
            ("Name=x", "name", None),
            ("Name=x", "Name", Some(b"x")),
            ("Name=x", "x;y", None),
            ("Name=x", "", None),
            ("name=%2", "name", None),
            ("page=1&page_size=%31%30", "name", None),
            ("page=1&page_size=%31%30", "page", Some(b"1")),
            ("page=1&page_size=%31%30", "page_size", Some(b"10")),
            ("name=x%00y", "name", Some(b"x\x00y")),
            ("%=1&name=2", "name", Some(b"2")),
            ("name=%gg&name=ok", "name", Some(b"ok")),
            ("", "name", None),
            ("&", "name", None),
            ("&&name=x&&", "name", Some(b"x")),
            ("name=%2B", "name", Some(b"+")),
            ("name=+", "name", Some(b" ")),
            ("name+=x", "name", None),
            ("na+me=x", "name", None),
            ("na+me=x", "na me", Some(b"x")),
            ("name=%u0041", "name", None),
            ("x=%&name=y", "name", Some(b"y")),
            ("name=a%", "name", None),
            ("name=%%", "name", None),
            ("a;b=c&name=d", "name", Some(b"d")),
            ("a;b=c&name=d", "Name", None),
            ("a;b=c&name=d", "x;y", None),
            ("a;b=c&name=d", "", None),
            ("name=;", "name", None),
            ("name=%3B", "name", Some(b";")),
            ("name=%FF%FE", "name", Some(b"\xff\xfe")),
            ("page_size=0&page=-1", "name", None),
            ("page_size=0&page=-1", "page", Some(b"-1")),
            ("page_size=0&page=-1", "page_size", Some(b"0")),
            ("name=%C3%A9", "name", Some(b"\xc3\xa9")),
        ];
        for &(raw, name, want) in cases {
            assert_eq!(Query::parse(Some(raw)).get(name), want, "{raw:?} {name:?}");
        }
    }
}
