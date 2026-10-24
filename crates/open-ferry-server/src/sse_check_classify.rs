//! Borrowed fields for stream classification, with the same permissive,
//! first-key-wins reading as the handlers' gjson lookups.

use crate::json::Val;

fn trim_start(mut raw: &[u8]) -> &[u8] {
    while matches!(raw.first(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        raw = &raw[1..];
    }
    raw
}

/// Selects fields in one traversal, without allocating a map or copying values.
/// In particular, a malformed suffix must not discard earlier fields.
pub(crate) fn fields<'a, const N: usize>(raw: &'a [u8], names: [&str; N]) -> [Option<Val<'a>>; N] {
    let mut found = [None; N];
    let Some(mut rest) = trim_start(raw).strip_prefix(b"{") else {
        return found;
    };
    while let Some(key) = Val::parse(rest).filter(Val::is_string) {
        rest = trim_start(&rest[key.index + key.raw.len()..]);
        let Some(value_start) = rest.strip_prefix(b":") else {
            break;
        };
        let Some(mut value) = Val::parse(value_start) else {
            break;
        };
        rest = trim_start(&value_start[value.index + value.raw.len()..]);
        value.index += raw.len() - value_start.len();
        let key_raw = &key.raw[1..key.raw.len() - 1];
        // Ordinary keys need no unescaping or allocation.
        let decoded = key_raw.contains(&b'\\').then(|| key.str());
        let key = decoded.as_deref().map(str::as_bytes).unwrap_or(key_raw);
        for (slot, name) in found.iter_mut().zip(names) {
            if slot.is_none() && key == name.as_bytes() {
                *slot = Some(value);
            }
        }
        let Some(next) = rest.strip_prefix(b",") else {
            break;
        };
        rest = next;
    }
    found
}

/// gjson's Array iteration without allocating its temporary list.
pub(crate) fn any_choice(value: Val<'_>, mut matches: impl FnMut(Val<'_>) -> bool) -> bool {
    if value.is_null() {
        return false;
    }
    if !value.is_array() {
        return matches(value);
    }
    let mut rest = trim_start(&value.raw[1..]);
    if rest.starts_with(b"]") {
        return false;
    }
    while let Some(item) = Val::parse(rest) {
        if matches(item) {
            return true;
        }
        rest = trim_start(&rest[item.index + item.raw.len()..]);
        let Some(next) = rest.strip_prefix(b",") else {
            break;
        };
        rest = trim_start(next);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn fields_match_individual_lookups_even_in_malformed_objects() {
        let corpus: &[&[u8]] = &[
            b"",
            b"null",
            b"[]",
            b"{}",
            b"{\"a\":1,\"b\":null}",
            b"{\"a\":1,\"a\":2}",
            b"{\"a\":1,broken",
            b"{\"a\":1,\"b\":{",
            b"{\"a\":{]}",
            b"{\"a\":\"\\u0061\",\"\\u0062\":[]}",
            b"\x0b{\"a\":1}",
            b"{\"a\":1,}",
            b"{\"a\":1,\"b\":nope",
        ];
        for raw in corpus {
            let got = fields(raw, ["a", "b"]);
            for (index, name) in ["a", "b"].iter().enumerate() {
                assert_eq!(
                    got[index].map(|v| (v.raw, v.index)),
                    json::get(raw, name).map(|v| (v.raw, v.index)),
                    "{raw:?}"
                );
            }
        }
    }
}
