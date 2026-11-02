//! Ephemeral targets: a target with `ephemeral: true` is a stack that runs many times side by side,
//! each copy named on the command line and given its own values there.
//!
//! Rendering a case on an ephemeral target needs an [`Ephemeral`]: its name is `obelisk.ephemeral`
//! (`null` on every other target), and its values are the last config layer, merged over the target's
//! and unit's config files under the same rules as a config file.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::paths::Selection;
use crate::config::repo::Target;
use crate::config::{NAME_RULE, is_valid_name};
use crate::{ObeliskK8sError, git, yaml};

/// One copy of an ephemeral target, as given on the command line.
#[derive(Debug)]
pub struct Ephemeral {
    /// A valid name, like a target's or unit's (a DNS label).
    pub name: String,
    /// A mapping or nothing, like a config file; checked when it's merged.
    values: Value,
    /// Where the values came from, as the user should see it: the path as given, or
    /// `--ephemeral-values` for inline YAML. It stands in for a file's path in errors and in
    /// [`FileObelisks`](crate::config::values::FileObelisks).
    shown: PathBuf,
}

/// Where an [`Ephemeral`]'s values come from.
#[derive(Debug)]
pub enum EphemeralValues {
    /// YAML text, e.g. `--ephemeral-values '{replicas: 1}'`.
    Inline(String),
    /// A YAML file, as given (relative to the current directory, or absolute).
    File(PathBuf),
}

impl Ephemeral {
    /// Checks `name` and reads `values`.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if `name` isn't a valid name; whatever reading the values fails with.
    pub fn new(name: String, values: EphemeralValues) -> Result<Self, ObeliskK8sError> {
        if !is_valid_name(&name) {
            return Err(ObeliskK8sError::Usage(format!(
                "ephemeral name `{name}` isn't a valid name ({NAME_RULE})"
            )));
        }
        let (values, shown) = match values {
            EphemeralValues::Inline(text) => {
                let shown = PathBuf::from("--ephemeral-values");
                (yaml::parse_values(&text, &shown)?, shown)
            }
            EphemeralValues::File(path) => (yaml::read_values(&path, &path)?, path),
        };
        Ok(Self {
            name,
            values,
            shown,
        })
    }

    pub(crate) fn values(&self) -> &Value {
        &self.values
    }

    pub(crate) fn shown(&self) -> &Path {
        &self.shown
    }
}

/// What every case in a run shares, beyond the repo's config: the git facts and the ephemeral copy.
#[derive(Debug)]
pub(crate) struct RunInputs<'e> {
    pub(crate) git: Value,
    pub(crate) ephemeral: Option<&'e Ephemeral>,
}

impl<'e> RunInputs<'e> {
    /// Gathers them for `selection`, after checking it with [`check_selection`].
    ///
    /// # Errors
    ///
    /// As [`check_selection`].
    pub(crate) fn new(
        repo_root: &Path,
        selection: &Selection<'_>,
        ephemeral: Option<&'e Ephemeral>,
    ) -> Result<Self, ObeliskK8sError> {
        check_selection(selection, ephemeral)?;
        Ok(Self {
            git: git::facts(repo_root),
            ephemeral,
        })
    }

    /// The copy `target` renders as: `None` unless it's ephemeral.
    pub(crate) fn ephemeral_for(&self, target: &Target) -> Option<&'e Ephemeral> {
        self.ephemeral.filter(|_| target.ephemeral)
    }
}

/// `ephemeral` is needed exactly when a target being rendered (one with a case in `selection`) is
/// ephemeral.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if one is, without `ephemeral`; or `ephemeral` is given and none is.
fn check_selection(
    selection: &Selection<'_>,
    ephemeral: Option<&Ephemeral>,
) -> Result<(), ObeliskK8sError> {
    let mut ephemeral_targets: Vec<&str> = Vec::new();
    for (target, _) in &selection.cases {
        if target.ephemeral && !ephemeral_targets.contains(&target.name.as_str()) {
            ephemeral_targets.push(&target.name);
        }
    }
    match (ephemeral, ephemeral_targets.as_slice()) {
        (None, [_, ..]) => {
            let names = ephemeral_targets
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let (s, is) = if ephemeral_targets.len() == 1 {
                ("", "is")
            } else {
                ("s", "are")
            };
            Err(ObeliskK8sError::Usage(format!(
                "target{s} {names} {is} ephemeral: pass `--ephemeral-name`, and `--ephemeral-values` or `--ephemeral-values-path`; or pick other targets with `-t`"
            )))
        }
        (Some(ephemeral), []) => Err(ObeliskK8sError::Usage(format!(
            "ephemeral `{}` given, but none of the targets being rendered is ephemeral (`ephemeral: true`)",
            ephemeral.name
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_must_be_valid() {
        let new = |name: &str| Ephemeral::new(name.into(), EphemeralValues::Inline(String::new()));
        assert!(new("pr-7").is_ok());
        assert_eq!(
            new("PR_7").unwrap_err().to_string(),
            format!("ephemeral name `PR_7` isn't a valid name ({NAME_RULE})")
        );
    }

    #[test]
    fn reads_inline_values() {
        let e = Ephemeral::new("x".into(), EphemeralValues::Inline("a: 0755".into())).unwrap();
        assert_eq!(e.values(), &serde_json::json!({ "a": 493 }));
        assert_eq!(e.shown(), Path::new("--ephemeral-values"));
        let error = Ephemeral::new("x".into(), EphemeralValues::Inline("a: [".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--ephemeral-values"), "{error}");
    }

    #[test]
    fn reads_values_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.yaml");
        std::fs::write(&path, "a: 1\n").unwrap();
        let e = Ephemeral::new("x".into(), EphemeralValues::File(path.clone())).unwrap();
        assert_eq!(e.values(), &serde_json::json!({ "a": 1 }));
        assert_eq!(e.shown(), path);
        assert!(matches!(
            Ephemeral::new("x".into(), EphemeralValues::File(dir.path().join("nope"))),
            Err(ObeliskK8sError::Io { .. })
        ));
    }
}
