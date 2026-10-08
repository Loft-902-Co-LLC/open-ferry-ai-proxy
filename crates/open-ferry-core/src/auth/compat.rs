// Ported from CLIProxyAPI internal/util/provider.go
// (OpenAICompatibleProviderKey) and sdk/cliproxy/service_auth.go
// (openAICompatInfoFromAuth) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! How OpenAI-compatible providers are named: the key their executor,
//! models and credentials go by, and how a credential says which provider
//! it belongs to.
//!
//! Deviations from upstream: none.

use open_ferry_translate::go::to_lower;

use super::Auth;
use super::go::equal_fold;

/// The provider of an OpenAI-compatible credential that names no provider
/// of its own, and the key of the executor that serves it.
pub const OPENAI_COMPATIBILITY: &str = "openai-compatibility";

/// The prefix of a named OpenAI-compatible provider's key.
const PROVIDER_PREFIX: &str = "openai-compatible-";

/// The attribute holding the name of a credential's OpenAI-compatible
/// provider, as configured.
pub const ATTRIBUTE_COMPAT_NAME: &str = "compat_name";

/// The attribute holding the key of a credential's OpenAI-compatible
/// provider.
pub const ATTRIBUTE_PROVIDER_KEY: &str = "provider_key";

/// The key an OpenAI-compatible provider called `name` goes by: the name in
/// lower case after `openai-compatible-`, or `openai-compatibility` for no
/// name. A name that already is such a key is kept.
pub fn openai_compatible_provider_key(name: &str) -> String {
    let name = to_lower(name.trim());
    if name.is_empty() {
        return OPENAI_COMPATIBILITY.to_owned();
    }
    if name == OPENAI_COMPATIBILITY || name.starts_with(PROVIDER_PREFIX) {
        return name;
    }
    format!("{PROVIDER_PREFIX}{name}")
}

impl Auth {
    /// The provider key and configured name of the OpenAI-compatible
    /// provider the credential belongs to, or `None` for a credential of
    /// another provider.
    ///
    /// A credential with a `compat_name` attribute goes by its
    /// `provider_key` attribute, else by that name; one whose provider is
    /// `openai-compatibility` goes by its label.
    pub fn openai_compat_info(&self) -> Option<(String, String)> {
        let compat_name = self.trimmed_attribute(ATTRIBUTE_COMPAT_NAME);
        if !compat_name.is_empty() {
            let provider_key = self.trimmed_attribute(ATTRIBUTE_PROVIDER_KEY);
            let key = if provider_key.is_empty() {
                compat_name
            } else {
                provider_key
            };
            return Some((openai_compatible_provider_key(key), compat_name.to_owned()));
        }
        if equal_fold(self.provider.trim(), OPENAI_COMPATIBILITY) {
            let label = self.label.trim();
            let key = if label.is_empty() {
                OPENAI_COMPATIBILITY
            } else {
                label
            };
            return Some((openai_compatible_provider_key(key), label.to_owned()));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys() {
        for (name, want) in [
            ("", "openai-compatibility"),
            ("  ", "openai-compatibility"),
            (" Kimi ", "openai-compatible-kimi"),
            ("OpenAI-Compatibility", "openai-compatibility"),
            ("openai-compatible-kimi", "openai-compatible-kimi"),
            ("OPENAI-COMPATIBLE-X", "openai-compatible-x"),
        ] {
            assert_eq!(openai_compatible_provider_key(name), want, "{name:?}");
        }
    }

    fn auth(provider: &str, label: &str, attributes: &[(&str, &str)]) -> Auth {
        Auth {
            provider: provider.into(),
            label: label.into(),
            attributes: attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            ..Auth::default()
        }
    }

    #[test]
    fn compat_info() {
        let info = |auth: Auth| auth.openai_compat_info();
        let pair = |key: &str, name: &str| Some((key.to_owned(), name.to_owned()));
        assert_eq!(
            info(auth(
                "openai-compatible-kimi",
                "Kimi",
                &[("compat_name", " Kimi ")]
            )),
            pair("openai-compatible-kimi", "Kimi")
        );
        assert_eq!(
            info(auth(
                "x",
                "",
                &[("compat_name", "Kimi"), ("provider_key", "moonshot")]
            )),
            pair("openai-compatible-moonshot", "Kimi")
        );
        assert_eq!(
            info(auth(" OpenAI-Compatibility ", " Local ", &[])),
            pair("openai-compatible-local", "Local")
        );
        assert_eq!(
            info(auth("openai-compatibility", "", &[("compat_name", " ")])),
            pair("openai-compatibility", "")
        );
        assert_eq!(info(auth("codex", "Kimi", &[("provider_key", "k")])), None);
    }
}
