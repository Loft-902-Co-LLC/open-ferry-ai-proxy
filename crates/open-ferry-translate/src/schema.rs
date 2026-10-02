// Ported from CLIProxyAPI internal/util/claude_schema.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! JSON Schema keyword tables and checks shared by tool-schema cleaners.

/// Keywords whose value is a map of subschemas.
pub(crate) const MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

/// Keywords whose value is a single subschema or a list of subschemas.
pub(crate) const VALUE_KEYWORDS: [&str; 16] = [
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];

/// Reports whether a regex uses `\p{..}`/`\P{..}` property escapes or `\0`,
/// which some upstream schema validators reject. Escaped backslashes are skipped,
/// so a literal `\\p{..}` is allowed.
pub(crate) fn has_unsupported_unicode_property_escape(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else {
            break;
        };
        if matches!(next, b'p' | b'P') && bytes.get(i + 2) == Some(&b'{') {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_property_escapes_but_not_escaped_backslashes() {
        assert!(has_unsupported_unicode_property_escape(r"[^\p{Cc}]"));
        assert!(has_unsupported_unicode_property_escape(r"\P{L}+"));
        assert!(has_unsupported_unicode_property_escape(r"a\0"));
        assert!(!has_unsupported_unicode_property_escape(r"^\\p{Cc}$"));
        assert!(!has_unsupported_unicode_property_escape(r"^[0-9a-f]{32}$"));
        assert!(!has_unsupported_unicode_property_escape(r"\p"));
        assert!(!has_unsupported_unicode_property_escape("trailing\\"));
    }
}
