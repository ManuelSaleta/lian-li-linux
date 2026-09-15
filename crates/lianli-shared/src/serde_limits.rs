use serde::de::{Error, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::marker::PhantomData;

fn bounded_vec<'de, D, T, const LIMIT: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const LIMIT: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for Bounded<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "at most {LIMIT} entries")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(LIMIT));
            while values.len() < LIMIT {
                match sequence.next_element()? {
                    Some(value) => values.push(value),
                    None => return Ok(values),
                }
            }
            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return Err(A::Error::custom(format!(
                    "Collection exceeds {LIMIT} entries"
                )));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, LIMIT>(PhantomData))
}

pub fn lcds<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<crate::config::LcdConfig>, D::Error> {
    bounded_vec::<_, _, 256>(d)
}

pub fn templates<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<crate::template::LcdTemplate>, D::Error> {
    bounded_vec::<_, _, 1024>(d)
}

pub fn widgets<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<crate::template::Widget>, D::Error> {
    bounded_vec::<_, _, 1024>(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lcd_limit_preserves_defaults_and_legacy_alias() {
        let entries = vec![serde_json::json!({"type": "color"}); 256];
        let mut value = serde_json::json!({"devices": entries});
        let config: crate::config::AppConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.lcds.len(), 256);
        value["devices"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "color"}));
        assert!(serde_json::from_value::<crate::config::AppConfig>(value)
            .unwrap_err()
            .to_string()
            .contains("exceeds 256"));
        assert!(serde_json::from_str::<crate::config::AppConfig>("{}")
            .unwrap()
            .lcds
            .is_empty());
    }

    #[test]
    fn bounded_sequence_does_not_deserialize_excess_elements() {
        #[derive(Debug, Deserialize)]
        struct Entry {
            _value: u32,
        }
        let mut input = serde_json::Deserializer::from_str(r#"[{"_value":1},{"_value":2},false]"#);
        let error = bounded_vec::<_, Entry, 2>(&mut input).unwrap_err();
        assert!(error.to_string().contains("exceeds 2"));
    }

    #[test]
    fn template_and_widget_limits_reject_the_first_excess_entry() {
        let widget = serde_json::json!({"id":"w","x":0,"y":0,"width":1,"height":1,
            "kind":{"type":"image","path":"image.png"}});
        let mut template = serde_json::json!({"id":"t","name":"T","base_width":1,"base_height":1,
            "background":{"type":"color","rgb":[0,0,0]},"widgets":vec![widget.clone();1024]});
        let accepted: crate::template::LcdTemplate =
            serde_json::from_value(template.clone()).unwrap();
        assert_eq!(accepted.widgets.len(), 1024);
        template["widgets"].as_array_mut().unwrap().push(widget);
        assert!(
            serde_json::from_value::<crate::template::LcdTemplate>(template.clone())
                .unwrap_err()
                .to_string()
                .contains("exceeds 1024")
        );
        template["widgets"] = serde_json::json!([]);
        #[derive(Debug, Deserialize)]
        struct File {
            #[serde(deserialize_with = "templates")]
            templates: Vec<crate::template::LcdTemplate>,
        }
        let mut file = serde_json::json!({"templates":vec![template.clone();1024]});
        assert_eq!(
            serde_json::from_value::<File>(file.clone())
                .unwrap()
                .templates
                .len(),
            1024
        );
        file["templates"].as_array_mut().unwrap().push(template);
        assert!(serde_json::from_value::<File>(file)
            .unwrap_err()
            .to_string()
            .contains("exceeds 1024"));
    }
}
