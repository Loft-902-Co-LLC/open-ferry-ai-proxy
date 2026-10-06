// Ported from CLIProxyAPI internal/config/disable_image_generation_mode.go
// (DisableImageGenerationMode, String, UnmarshalYAML,
// parseDisableImageGenerationNode, parseDisableImageGenerationString)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `disable-image-generation` setting: whether the built-in
//! `image_generation` tool is taken out of the requests sent upstream.
//!
//! The providers' payload module does the taking out. Nothing in this port
//! adds the tool or generates images, so `false` and `passthrough` both
//! leave a request as the client sent it.
//!
//! Deviations from upstream:
//! - A sequence or mapping fails the load with yaml.v3's type error
//!   (`cannot unmarshal !!map into string`) rather than upstream's
//!   `invalid disable-image-generation value`.

use std::fmt;

use open_ferry_translate::go::{quote, to_lower};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// What `disable-image-generation` does with the built-in `image_generation`
/// tool (upstream's `DisableImageGenerationMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DisableImageGeneration {
    /// `false`: the tool is left alone.
    #[default]
    Off,
    /// `true`: the tool is taken out of every request, and the images
    /// endpoints answer 404.
    All,
    /// `chat`: the tool is taken out except on the images endpoints.
    Chat,
    /// `passthrough`: the tool is left as the client sent it.
    Passthrough,
}

impl DisableImageGeneration {
    /// The setting as upstream prints it (`String`): `false`, `true`, `chat`
    /// or `passthrough`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "false",
            Self::All => "true",
            Self::Chat => "chat",
            Self::Passthrough => "passthrough",
        }
    }

    /// Reads the setting's text, in any case and with spaces around it
    /// (`parseDisableImageGenerationString`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = to_lower(text);
        let text = text.trim();
        match text {
            "" | "false" | "0" | "off" | "no" => Ok(Self::Off),
            "true" | "1" | "on" | "yes" => Ok(Self::All),
            "chat" => Ok(Self::Chat),
            "passthrough" => Ok(Self::Passthrough),
            _ => Err(format!(
                "invalid disable-image-generation value {} (allowed: true, false, chat, passthrough)",
                quote(text)
            )),
        }
    }
}

impl fmt::Display for DisableImageGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DisableImageGeneration {
    /// Reads a YAML scalar as its text, which is how upstream reads
    /// anything but a boolean, and a boolean's text reads as that boolean.
    /// So `0x1` or `1.0` fails, as upstream's string decode does.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Mode;

        impl Visitor<'_> for Mode {
            type Value = DisableImageGeneration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("true, false, chat or passthrough")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(if value {
                    DisableImageGeneration::All
                } else {
                    DisableImageGeneration::Off
                })
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                DisableImageGeneration::parse(value).map_err(E::custom)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(DisableImageGeneration::Off)
            }
        }

        deserializer.deserialize_string(Mode)
    }
}

#[cfg(test)]
mod tests {
    use super::DisableImageGeneration;
    use crate::config::Config;

    fn load(yaml: &str) -> Result<DisableImageGeneration, String> {
        Config::parse(yaml)
            .map(|config| config.disable_image_generation)
            .map_err(|error| error.to_string())
    }

    // Not upstream's: the values parseDisableImageGenerationNode and
    // parseDisableImageGenerationString take, in both layouts.
    #[test]
    fn reads_every_spelling() {
        use DisableImageGeneration::{All, Chat, Off, Passthrough};
        let cases = [
            ("disable-image-generation: true\n", All),
            ("disable-image-generation: True\n", All),
            ("disable-image-generation: false\n", Off),
            ("disable-image-generation: 'true'\n", All),
            ("disable-image-generation: ' Chat '\n", Chat),
            ("disable-image-generation: passthrough\n", Passthrough),
            ("disable-image-generation: yes\n", All),
            ("disable-image-generation: off\n", Off),
            ("disable-image-generation: 1\n", All),
            ("disable-image-generation: 0\n", Off),
            ("disable-image-generation: ''\n", Off),
            ("disable-image-generation:\n", Off),
            ("port: 1\n", Off),
            ("multimedia:\n  disable-image-generation: chat\n", Chat),
        ];
        for (yaml, want) in cases {
            assert_eq!(load(yaml), Ok(want), "{yaml}");
        }
    }

    // Not upstream's: what parseDisableImageGenerationString refuses, and
    // number spellings, which upstream reads as their text.
    #[test]
    fn refuses_other_values() {
        for yaml in [
            "disable-image-generation: images\n",
            "disable-image-generation: 0x1\n",
            "disable-image-generation: 1.0\n",
            "disable-image-generation: [true]\n",
        ] {
            assert!(load(yaml).is_err(), "{yaml}");
        }
        let error = load("disable-image-generation: Images\n").unwrap_err();
        assert!(
            error.contains(
                "invalid disable-image-generation value \"images\" (allowed: true, false, chat, passthrough)"
            ),
            "{error}"
        );
    }

    // Not upstream's: String, which the config diff prints.
    #[test]
    fn prints_as_upstream() {
        assert_eq!(DisableImageGeneration::Off.to_string(), "false");
        assert_eq!(DisableImageGeneration::All.to_string(), "true");
        assert_eq!(DisableImageGeneration::Chat.as_str(), "chat");
        assert_eq!(DisableImageGeneration::Passthrough.as_str(), "passthrough");
    }
}
