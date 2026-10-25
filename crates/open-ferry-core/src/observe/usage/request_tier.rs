//! Final outbound tiers without retaining an unrelated JSON value tree.
//! Duplicate keys take their last value, as the usage reader did before;
//! all containers are visited to keep Serde's depth and string checks.

use std::fmt;

use serde::Deserialize;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

/// Read only the three tier paths, validating the whole payload.
pub(super) fn read(payload: &[u8]) -> String {
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let Ok(tiers) = Read(Path::Root).deserialize(&mut deserializer) else {
        return String::new();
    };
    if deserializer.end().is_err() {
        return String::new();
    }
    [tiers.direct, tiers.request, tiers.interaction]
        .into_iter()
        .find(|tier| !tier.is_empty())
        .unwrap_or_default()
}

#[derive(Default)]
struct Tiers {
    direct: String,
    request: String,
    interaction: String,
}

#[derive(Clone, Copy)]
enum Path {
    Root,
    Envelope,
    Tier,
    Other,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum Field {
    ServiceTier,
    Request,
    Interaction,
    #[serde(other)]
    Other,
}

struct Read(Path);

impl<'de> DeserializeSeed<'de> for Read {
    type Value = Tiers;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Tiers, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Read {
    type Value = Tiers;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Tiers, M::Error> {
        let mut tiers = Tiers::default();
        while let Some(field) = map.next_key::<Field>()? {
            match (self.0, field) {
                (Path::Root | Path::Envelope, Field::ServiceTier) => {
                    tiers.direct = map.next_value_seed(Read(Path::Tier))?.direct;
                }
                (Path::Root, Field::Request) => {
                    tiers.request = map.next_value_seed(Read(Path::Envelope))?.direct;
                }
                (Path::Root, Field::Interaction) => {
                    tiers.interaction = map.next_value_seed(Read(Path::Envelope))?.direct;
                }
                _ => {
                    map.next_value_seed(Read(Path::Other))?;
                }
            }
        }
        Ok(tiers)
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut sequence: S) -> Result<Tiers, S::Error> {
        while sequence.next_element_seed(Read(Path::Other))?.is_some() {}
        Ok(Tiers::default())
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Tiers, E> {
        Ok(Tiers {
            direct: if matches!(self.0, Path::Tier) {
                value.trim().to_owned()
            } else {
                String::new()
            },
            ..Tiers::default()
        })
    }

    fn visit_bool<E: serde::de::Error>(self, _value: bool) -> Result<Tiers, E> {
        Ok(Tiers::default())
    }

    fn visit_i64<E: serde::de::Error>(self, _value: i64) -> Result<Tiers, E> {
        Ok(Tiers::default())
    }

    fn visit_u64<E: serde::de::Error>(self, _value: u64) -> Result<Tiers, E> {
        Ok(Tiers::default())
    }

    fn visit_f64<E: serde::de::Error>(self, _value: f64) -> Result<Tiers, E> {
        Ok(Tiers::default())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Tiers, E> {
        Ok(Tiers::default())
    }
}
