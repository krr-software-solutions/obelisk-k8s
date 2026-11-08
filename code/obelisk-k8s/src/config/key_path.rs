//! A dot-separated path of keys into the values (`a.b.c`), as a source's `unnestInput` and
//! `nestOutput` are written: keys into mappings only (no key may hold a `.`; no list indices).

use std::fmt;

use serde_json::{Map, Value};

use crate::config::values::OBELISK_KEY;

/// Keys into nested mappings, outermost first; never empty, nor is any key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPath(Vec<String>);

impl KeyPath {
    /// `written` (`a.b.c`), split into its keys.
    pub(crate) fn parse(written: &str) -> Result<Self, String> {
        let keys: Vec<String> = written.split('.').map(str::to_owned).collect();
        if keys.iter().any(String::is_empty) {
            return Err(format!("`{written}` has an empty key: write `a.b.c`"));
        }
        Ok(Self(keys))
    }

    /// Whether it's under `obelisk:` (or is it), which is the tool's.
    pub(crate) fn under_obelisk(&self) -> bool {
        self.0[0] == OBELISK_KEY
    }

    /// What's at it in `values`.
    pub(crate) fn get<'v>(&self, values: &'v Map<String, Value>) -> Result<&'v Value, String> {
        self.find(values)?.ok_or_else(|| {
            let (i, key) = self.missing(values);
            let here = self.0[..i].join(".");
            if here.is_empty() {
                format!("no `{key}` in the values")
            } else {
                format!("no `{key}` under `{here}`")
            }
        })
    }

    /// What's at it in `values`: `None` if a key on the way is missing; an error if something on
    /// the way isn't a mapping.
    pub(crate) fn find<'v>(
        &self,
        values: &'v Map<String, Value>,
    ) -> Result<Option<&'v Value>, String> {
        let mut map = values;
        for (i, key) in self.0.iter().enumerate() {
            let Some(at) = map.get(key) else {
                return Ok(None);
            };
            if i + 1 == self.0.len() {
                return Ok(Some(at));
            }
            let Value::Object(inner) = at else {
                return Err(format!("`{}` isn't a mapping", self.0[..=i].join(".")));
            };
            map = inner;
        }
        unreachable!("a key path has a key")
    }

    /// The first of its keys missing from `values` (which [`KeyPath::find`] found `None` in), and
    /// its index.
    fn missing(&self, values: &Map<String, Value>) -> (usize, &str) {
        let mut map = values;
        for (i, key) in self.0.iter().enumerate() {
            match map.get(key) {
                Some(Value::Object(inner)) => map = inner,
                _ => return (i, key),
            }
        }
        unreachable!("a key path that was found")
    }

    /// `value`, nested under it (`a: { b: { c: value } }`).
    pub(crate) fn wrap(&self, value: Value) -> Value {
        self.0.iter().rev().fold(value, |inner, key| {
            Value::Object(Map::from_iter([(key.clone(), inner)]))
        })
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("."))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn path(written: &str) -> KeyPath {
        KeyPath::parse(written).unwrap()
    }

    #[test]
    fn parses_dot_separated_keys() {
        assert_eq!(path("a.b.c").to_string(), "a.b.c");
        assert_eq!(path("a"), KeyPath(vec!["a".to_owned()]));
        for bad in ["", "a..b", ".a", "a."] {
            assert_eq!(
                KeyPath::parse(bad).unwrap_err(),
                format!("`{bad}` has an empty key: write `a.b.c`")
            );
        }
        assert!(path("obelisk").under_obelisk());
        assert!(path("obelisk.release").under_obelisk());
        assert!(!path("app.obelisk").under_obelisk());
    }

    #[test]
    fn gets_what_is_at_it() {
        let Value::Object(values) = json!({ "a": { "b": { "c": [1] }, "n": null }, "s": "x" })
        else {
            unreachable!()
        };
        assert_eq!(path("a.b").get(&values).unwrap(), &json!({ "c": [1] }));
        assert_eq!(path("a.b.c").get(&values).unwrap(), &json!([1]));
        assert_eq!(path("a.n").get(&values).unwrap(), &Value::Null);
        assert_eq!(
            path("nope").get(&values).unwrap_err(),
            "no `nope` in the values"
        );
        assert_eq!(
            path("a.nope").get(&values).unwrap_err(),
            "no `nope` under `a`"
        );
        assert_eq!(path("s.x").get(&values).unwrap_err(), "`s` isn't a mapping");
        assert_eq!(
            path("a.b.c.d").get(&values).unwrap_err(),
            "`a.b.c` isn't a mapping"
        );
    }

    #[test]
    fn wraps_a_value() {
        assert_eq!(path("a.b").wrap(json!([1])), json!({ "a": { "b": [1] } }));
        assert_eq!(
            path("a").wrap(json!({ "x": 1 })),
            json!({ "a": { "x": 1 } })
        );
    }
}
