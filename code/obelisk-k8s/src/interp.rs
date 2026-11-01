//! `${{obelisk:...}}` interpolation in obelisk-k8s's own config files (never in values files, and never
//! in `.gotmpl`, which has its own mechanism).
//!
//! ```text
//! "./config/${{obelisk:unit}}/values.yaml" → [Text("./config/"), Var(Unit), Text("/values.yaml")]
//! ```
//!
//! `$${{` writes a literal `${{`.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer};

/// A value obelisk-k8s fills in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Var {
    Target,
    Unit,
}

impl Var {
    const ALL: [Self; 2] = [Self::Target, Self::Unit];

    /// The name after `obelisk:`, e.g. `target`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Unit => "unit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StringPart {
    Text(String),
    Var(Var),
}

/// A string that may hold `${{obelisk:...}}` vars, with the string as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpString {
    raw: String,
    parts: Vec<StringPart>,
}

/// The values the vars take. A var with no value here is an error, not an empty string.
#[derive(Debug, Clone, Copy, Default)]
pub struct InterpolationParams<'a> {
    pub target: Option<&'a str>,
    pub unit: Option<&'a str>,
}

impl InterpolationParams<'_> {
    const fn get(&self, var: Var) -> Option<&str> {
        match var {
            Var::Target => self.target,
            Var::Unit => self.unit,
        }
    }
}

const OPEN: &str = "${{";
const CLOSE: &str = "}}";
const NAMESPACE: &str = "obelisk:";

impl InterpString {
    /// Parses `raw`.
    ///
    /// # Errors
    ///
    /// A message (without the string itself) for an unclosed `${{`, or an unknown namespace or var.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = raw;
        while let Some(i) = rest.find(OPEN) {
            // `$${{` is an escaped, literal `${{`.
            if rest[..i].ends_with('$') {
                text.push_str(&rest[..i - 1]);
                text.push_str(OPEN);
                rest = &rest[i + OPEN.len()..];
                continue;
            }
            text.push_str(&rest[..i]);
            let after = &rest[i + OPEN.len()..];
            let end = after
                .find(CLOSE)
                .ok_or_else(|| format!("unclosed `{OPEN}` (expected `{CLOSE}`)"))?;
            let inner = &after[..end];
            let name = inner.strip_prefix(NAMESPACE).ok_or_else(|| {
                format!("`{OPEN}{inner}{CLOSE}`: only `{OPEN}{NAMESPACE}...{CLOSE}` is supported")
            })?;
            let var = Var::ALL
                .into_iter()
                .find(|v| v.name() == name)
                .ok_or_else(|| {
                    let known = Var::ALL.map(Var::name).join(", ");
                    format!("unknown var `{NAMESPACE}{name}` (known: {known})")
                })?;
            if !text.is_empty() {
                parts.push(StringPart::Text(std::mem::take(&mut text)));
            }
            parts.push(StringPart::Var(var));
            rest = &after[end + CLOSE.len()..];
        }
        text.push_str(rest);
        if !text.is_empty() {
            parts.push(StringPart::Text(text));
        }
        Ok(Self {
            raw: raw.to_owned(),
            parts,
        })
    }

    /// The string as written.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    #[must_use]
    pub fn parts(&self) -> &[StringPart] {
        &self.parts
    }

    /// The vars it uses.
    #[must_use]
    pub fn vars(&self) -> BTreeSet<Var> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                StringPart::Var(v) => Some(*v),
                StringPart::Text(_) => None,
            })
            .collect()
    }

    /// Fills in the vars.
    ///
    /// # Errors
    ///
    /// The first var used that has no value in `params`.
    pub fn interpolate(&self, params: &InterpolationParams<'_>) -> Result<String, Var> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                StringPart::Text(text) => out.push_str(text),
                StringPart::Var(var) => out.push_str(params.get(*var).ok_or(*var)?),
            }
        }
        Ok(out)
    }
}

impl fmt::Display for InterpString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Bad syntax fails when the file is read, with the YAML crate's file, line and column.
impl<'de> Deserialize<'de> for InterpString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw)
            .map_err(|message| serde::de::Error::custom(format!("in `{raw}`: {message}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(raw: &str) -> Vec<StringPart> {
        InterpString::parse(raw).unwrap().parts
    }

    fn text(s: &str) -> StringPart {
        StringPart::Text(s.to_owned())
    }

    #[test]
    fn parses_vars_and_text() {
        assert_eq!(
            parts("./config/${{obelisk:unit}}/${{obelisk:target}}.values.yaml"),
            [
                text("./config/"),
                StringPart::Var(Var::Unit),
                text("/"),
                StringPart::Var(Var::Target),
                text(".values.yaml")
            ]
        );
        assert_eq!(parts("${{obelisk:unit}}"), [StringPart::Var(Var::Unit)]);
        assert_eq!(parts("plain"), [text("plain")]);
        assert_eq!(parts(""), []);
    }

    #[test]
    fn keeps_the_raw_string() {
        let s = InterpString::parse("./a/${{obelisk:unit}}").unwrap();
        assert_eq!(s.raw(), "./a/${{obelisk:unit}}");
        assert_eq!(s.to_string(), "./a/${{obelisk:unit}}");
    }

    #[test]
    fn escapes() {
        assert_eq!(parts("a$${{obelisk:unit}}b"), [text("a${{obelisk:unit}}b")]);
        assert_eq!(
            parts("$${{${{obelisk:unit}}"),
            [text("${{"), StringPart::Var(Var::Unit)]
        );
        assert_eq!(parts("$ {{ }}"), [text("$ {{ }}")]);
    }

    #[test]
    fn rejects_bad_syntax() {
        assert_eq!(
            InterpString::parse("./${{obelisk:unit").unwrap_err(),
            "unclosed `${{` (expected `}}`)"
        );
        assert_eq!(
            InterpString::parse("${{obelisk:untis}}").unwrap_err(),
            "unknown var `obelisk:untis` (known: target, unit)"
        );
        assert_eq!(
            InterpString::parse("${{env:HOME}}").unwrap_err(),
            "`${{env:HOME}}`: only `${{obelisk:...}}` is supported"
        );
    }

    #[test]
    fn lists_vars() {
        let s =
            InterpString::parse("${{obelisk:unit}}/${{obelisk:target}}/${{obelisk:unit}}").unwrap();
        assert_eq!(
            s.vars().into_iter().collect::<Vec<_>>(),
            [Var::Target, Var::Unit]
        );
        assert!(InterpString::parse("./x").unwrap().vars().is_empty());
    }

    #[test]
    fn interpolates() {
        let s = InterpString::parse("./config/${{obelisk:unit}}/${{obelisk:target}}.yaml").unwrap();
        let params = InterpolationParams {
            target: Some("prod"),
            unit: Some("app"),
        };
        assert_eq!(s.interpolate(&params).unwrap(), "./config/app/prod.yaml");
        let no_unit = InterpolationParams {
            target: Some("prod"),
            unit: None,
        };
        assert_eq!(s.interpolate(&no_unit), Err(Var::Unit));
    }
}
