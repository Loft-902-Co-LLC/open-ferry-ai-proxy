//! Reading JSON with each value's text as written, for checks that depend on
//! the text upstream copies rather than on the value.

use serde_json::Value;

/// A JSON value and its text as written.
pub enum Raw<'a> {
    /// An object's members in order, keys decoded, and its text.
    Object(Vec<(String, Raw<'a>)>, &'a str),
    Array(Vec<Raw<'a>>, &'a str),
    /// A string, number, `true`, `false` or `null`.
    Scalar(&'a str),
}

impl<'a> Raw<'a> {
    /// The value's text as written.
    pub fn text(&self) -> &'a str {
        match self {
            Self::Object(_, text) | Self::Array(_, text) | Self::Scalar(text) => text,
        }
    }

    /// The first member named `key`, as gjson finds it.
    pub fn get(&self, key: &str) -> Option<&Raw<'a>> {
        match self {
            Self::Object(members, _) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// The value, read as serde_json reads it.
    pub fn value(&self) -> Value {
        serde_json::from_str(self.text()).unwrap_or(Value::Null)
    }
}

/// Reads `text`, which must be one JSON value.
pub fn parse(text: &str) -> Option<Raw<'_>> {
    serde_json::from_str::<Value>(text).ok()?;
    let mut reader = Reader { text, at: 0 };
    let value = reader.value()?;
    reader.skip_space();
    (reader.at == text.len()).then_some(value)
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Option<Raw<'a>> {
        self.skip_space();
        let start = self.at;
        match self.peek()? {
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                loop {
                    self.skip_space();
                    if self.peek()? == b'}' {
                        break;
                    }
                    let key = self.string()?;
                    let key = serde_json::from_str(key).ok()?;
                    self.skip_space();
                    self.expect(b':')?;
                    members.push((key, self.value()?));
                    self.skip_space();
                    if self.peek()? == b',' {
                        self.at += 1;
                    }
                }
                self.at += 1;
                Some(Raw::Object(members, &self.text[start..self.at]))
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_space();
                    if self.peek()? == b']' {
                        break;
                    }
                    items.push(self.value()?);
                    self.skip_space();
                    if self.peek()? == b',' {
                        self.at += 1;
                    }
                }
                self.at += 1;
                Some(Raw::Array(items, &self.text[start..self.at]))
            }
            b'"' => self.string().map(Raw::Scalar),
            _ => {
                while !matches!(
                    self.peek(),
                    None | Some(b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
                ) {
                    self.at += 1;
                }
                Some(Raw::Scalar(&self.text[start..self.at]))
            }
        }
    }

    /// A string's text, quotes and escapes included.
    fn string(&mut self) -> Option<&'a str> {
        let start = self.at;
        self.expect(b'"')?;
        loop {
            match self.peek()? {
                b'\\' => self.at += 2,
                b'"' => break,
                _ => self.at += 1,
            }
        }
        self.at += 1;
        Some(&self.text[start..self.at])
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.peek()? == byte).then(|| self.at += 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_text_of_each_value() {
        let text = "{ \"a\" : [ 1.50 , {\"b\":\"x\\\"y\"} ], \"c\\u0064\": null, \"a\": 2 }";
        let root = parse(text).unwrap();
        let a = root.get("a").unwrap();
        assert_eq!(a.text(), "[ 1.50 , {\"b\":\"x\\\"y\"} ]");
        let Raw::Array(items, _) = a else {
            panic!("not an array")
        };
        assert_eq!(items[0].text(), "1.50");
        assert_eq!(items[1].get("b").unwrap().text(), "\"x\\\"y\"");
        assert_eq!(root.get("cd").unwrap().text(), "null");
        assert!(root.get("b").is_none());
        assert!(parse("{} x").is_none());
        assert!(parse("{\"a\":}").is_none());
    }
}
