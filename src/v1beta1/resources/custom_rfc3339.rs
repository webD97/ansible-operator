use chrono::{DateTime, FixedOffset, SecondsFormat};
use serde::{Deserialize, Deserializer, Serializer};

pub fn serialize<S>(dt: &Option<DateTime<FixedOffset>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match dt {
        Some(d) => serializer.serialize_str(&d.to_rfc3339_opts(SecondsFormat::Secs, true)),
        None => serializer.serialize_none(),
    }
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<FixedOffset>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    opt.map(|s| DateTime::parse_from_rfc3339(&s).map_err(serde::de::Error::custom))
        .transpose()
}
