//! A blueprint: a reusable batch of units, across repos, deployed together to one cluster.
//!
//! ```text
//! .obelisk-k8s/blueprints/<name>/blueprint.yaml
//! ```
//!
//! ```yaml
//! blueprint:
//!   ephemeral: true                 # every unit's target must agree (missing or null: false)
//!   defaults:
//!     ref: feature/abc|origin/feature/abc|origin/HEAD   # for units without their own
//!   units:
//!     - { repo: repo-1, target: local-ephemeral, unit: my-app-1 }
//!     - { repo: repo-1, target: local-ephemeral, unit: my-app-2, ref: a1b2cde }
//!     - repo: repo-1
//!       target: local-ephemeral
//!       unit: my-app-3
//!       dependsOn: [repo-1/local-ephemeral/my-app-1]
//! ```
//!
//! A unit is named `repo/target/unit`, which must be distinct within the blueprint; `dependsOn`
//! names others in it that way, and must not loop. Every unit needs a ref, its own or the default's
//! (see `checkout` for how one resolves). What the names point at (the repo, the target in it, the
//! unit, and whether the target is ephemeral) is only checked once an env resolves them to commits.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

use crate::ObeliskK8sError;
use crate::checkout::RefChain;
use crate::config::{NAME_RULE, check_name, config_error, dir_name, is_valid_name};
use crate::error::LineCol;
use crate::yaml::Spanned;

/// A unit in a blueprint: `repo/target/unit`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnitId {
    pub repo: String,
    pub target: String,
    pub unit: String,
}

impl UnitId {
    /// Parses `repo/target/unit`.
    ///
    /// # Errors
    ///
    /// A message if it isn't three valid names.
    pub fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split('/').collect();
        match parts[..] {
            [repo, target, unit] if parts.iter().all(|p| is_valid_name(p)) => Ok(Self {
                repo: repo.to_owned(),
                target: target.to_owned(),
                unit: unit.to_owned(),
            }),
            _ => Err(format!(
                "`{text}` isn't `repo/target/unit` (each {NAME_RULE})"
            )),
        }
    }
}

impl fmt::Display for UnitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.repo, self.target, self.unit)
    }
}

/// A blueprint, loaded and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blueprint {
    pub name: String,
    /// Relative to the repo root, e.g. `.obelisk-k8s/blueprints/krrss/blueprint.yaml`.
    pub file: PathBuf,
    pub ephemeral: bool,
    pub default_ref: Option<RefChain>,
    /// As written.
    pub units: Vec<BlueprintUnit>,
    /// Indices into `units`, dependencies first (ties in written order).
    pub order: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueprintUnit {
    pub id: UnitId,
    /// Its own, else the default.
    pub git_ref: RefChain,
    /// Whether `git_ref` is its own (not the default's).
    pub own_ref: bool,
    pub depends_on: Vec<UnitId>,
}

impl Blueprint {
    /// The units, dependencies first.
    pub fn ordered(&self) -> impl Iterator<Item = &BlueprintUnit> {
        self.order.iter().map(|&i| &self.units[i])
    }

    /// Loads `blueprints/<name>/blueprint.yaml` from `entry`.
    pub(crate) fn load(entry: &crate::config::Entry) -> Result<Self, ObeliskK8sError> {
        let file = &entry.file;
        let BlueprintFile { blueprint } = crate::yaml::read_file(&entry.path, file)?;
        let name = dir_name(entry, "rename it")?;
        let chain = |text: &Spanned<String>| {
            RefChain::parse(&text.value).map_err(|m| config_error(file, Some(text.at), m))
        };
        let default_ref = blueprint
            .defaults
            .and_then(|d| d.git_ref)
            .map(|r| chain(&r))
            .transpose()?;

        let mut units = Vec::new();
        let mut at: HashMap<UnitId, LineCol> = HashMap::new();
        for entry in blueprint.units.unwrap_or_default() {
            for name in [&entry.repo, &entry.target, &entry.unit] {
                check_name(file, name)?;
            }
            let id = UnitId {
                repo: entry.repo.value,
                target: entry.target.value,
                unit: entry.unit.value,
            };
            if at.insert(id.clone(), entry.repo.at).is_some() {
                return Err(config_error(
                    file,
                    Some(entry.repo.at),
                    format!("unit `{id}` is already in this blueprint"),
                ));
            }
            let (git_ref, own_ref) = match (&entry.git_ref, &default_ref) {
                (Some(own), _) => (chain(own)?, true),
                (None, Some(default)) => (default.clone(), false),
                (None, None) => {
                    return Err(config_error(
                        file,
                        Some(entry.repo.at),
                        format!("unit `{id}` has no ref: set `ref` on it, or `defaults.ref`"),
                    ));
                }
            };
            let mut depends_on = Vec::new();
            for dep in entry.depends_on.unwrap_or_default() {
                let dep_id =
                    UnitId::parse(&dep.value).map_err(|m| config_error(file, Some(dep.at), m))?;
                if dep_id == id {
                    return Err(config_error(
                        file,
                        Some(dep.at),
                        format!("unit `{id}` depends on itself"),
                    ));
                }
                if !depends_on.contains(&dep_id) {
                    depends_on.push(dep_id);
                }
            }
            units.push((
                BlueprintUnit {
                    id,
                    git_ref,
                    own_ref,
                    depends_on,
                },
                entry.repo.at,
            ));
        }
        for (unit, unit_at) in &units {
            if let Some(missing) = unit.depends_on.iter().find(|d| !at.contains_key(*d)) {
                return Err(config_error(
                    file,
                    Some(*unit_at),
                    format!(
                        "unit `{}` depends on `{missing}`, which isn't in this blueprint",
                        unit.id
                    ),
                ));
            }
        }
        let units: Vec<BlueprintUnit> = units.into_iter().map(|(u, _)| u).collect();
        let order = order(&units).map_err(|stuck| {
            config_error(
                file,
                None,
                format!("`dependsOn` loops between: {}", stuck.join(", ")),
            )
        })?;
        Ok(Self {
            name,
            file: file.clone(),
            ephemeral: blueprint.ephemeral.unwrap_or(false),
            default_ref,
            units,
            order,
        })
    }
}

/// Dependencies first, ties in written order; or the units caught in a loop.
fn order(units: &[BlueprintUnit]) -> Result<Vec<usize>, Vec<String>> {
    let index: HashMap<&UnitId, usize> =
        units.iter().enumerate().map(|(i, u)| (&u.id, i)).collect();
    let mut done = vec![false; units.len()];
    let mut order = Vec::with_capacity(units.len());
    while order.len() < units.len() {
        let next = (0..units.len())
            .find(|&i| !done[i] && units[i].depends_on.iter().all(|d| done[index[d]]));
        let Some(next) = next else {
            return Err(units
                .iter()
                .zip(&done)
                .filter(|(_, d)| !**d)
                .map(|(u, _)| format!("`{}`", u.id))
                .collect());
        };
        done[next] = true;
        order.push(next);
    }
    Ok(order)
}

/// What `config repo show` prints for one.
impl fmt::Display for Blueprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  {}:  ({})", self.name, self.file.display())?;
        if self.ephemeral {
            writeln!(f, "    ephemeral: yes")?;
        }
        if let Some(default) = &self.default_ref {
            writeln!(f, "    default ref: {default}")?;
        }
        writeln!(
            f,
            "    units:{}",
            if self.units.is_empty() { " (none)" } else { "" }
        )?;
        for unit in self.ordered() {
            let git_ref = if unit.own_ref {
                format!("  (ref: {})", unit.git_ref)
            } else {
                String::new()
            };
            writeln!(f, "      {}{git_ref}", unit.id)?;
            for dep in &unit.depends_on {
                writeln!(f, "        after {dep}")?;
            }
        }
        Ok(())
    }
}

// The file, as written.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlueprintFile {
    blueprint: BlueprintSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlueprintSection {
    ephemeral: Option<bool>,
    defaults: Option<Defaults>,
    units: Option<Vec<UnitEntry>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    #[serde(rename = "ref")]
    git_ref: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UnitEntry {
    repo: Spanned<String>,
    target: Spanned<String>,
    unit: Spanned<String>,
    #[serde(rename = "ref")]
    git_ref: Option<Spanned<String>>,
    depends_on: Option<Vec<Spanned<String>>>,
}

#[cfg(test)]
mod tests {
    use crate::config::repo::RepoConfig;
    use crate::config::test_support::write_repo;

    const REPO: &str = "repo:\n  name: bp\n";

    fn load(blueprint: &str) -> Result<RepoConfig, crate::ObeliskK8sError> {
        let dir = write_repo(&[
            ("repo.yaml", REPO),
            ("blueprints/b/blueprint.yaml", blueprint),
        ]);
        RepoConfig::load(dir.path())
    }

    #[test]
    fn loads_one_in_dependency_order() {
        let config = load(
            "blueprint:\n  ephemeral: true\n  defaults:\n    ref: main|origin/HEAD\n  units:\n    - { repo: r, target: t, unit: c, dependsOn: [r/t/a, r/t/b] }\n    - { repo: r, target: t, unit: b, ref: abc1234, dependsOn: [r/t/a] }\n    - { repo: r, target: t, unit: a }\n",
        )
        .unwrap();
        let [bp] = &config.blueprints[..] else {
            panic!("{:?}", config.blueprints)
        };
        assert_eq!(bp.name, "b");
        assert!(bp.ephemeral);
        let ordered: Vec<String> = bp.ordered().map(|u| u.id.to_string()).collect();
        assert_eq!(ordered, ["r/t/a", "r/t/b", "r/t/c"]);
        assert_eq!(bp.units[1].git_ref.to_string(), "abc1234");
        assert!(bp.units[1].own_ref);
        assert_eq!(bp.units[2].git_ref.to_string(), "main|origin/HEAD");
        assert!(!bp.units[2].own_ref);
    }

    #[test]
    fn says_whats_wrong() {
        let cases = [
            (
                "blueprint:\n  units:\n    - { repo: r, target: t, unit: a }\n",
                "has no ref",
            ),
            (
                "blueprint:\n  defaults: { ref: main }\n  units:\n    - { repo: r, target: t, unit: a }\n    - { repo: r, target: t, unit: a }\n",
                "already in this blueprint",
            ),
            (
                "blueprint:\n  defaults: { ref: main }\n  units:\n    - { repo: r, target: t, unit: a, dependsOn: [r/t/x] }\n",
                "isn't in this blueprint",
            ),
            (
                "blueprint:\n  defaults: { ref: main }\n  units:\n    - { repo: r, target: t, unit: a, dependsOn: [r/t/a] }\n",
                "depends on itself",
            ),
            (
                "blueprint:\n  defaults: { ref: main }\n  units:\n    - { repo: r, target: t, unit: a, dependsOn: [r/t/b] }\n    - { repo: r, target: t, unit: b, dependsOn: [r/t/a] }\n",
                "loops between: `r/t/a`, `r/t/b`",
            ),
            (
                "blueprint:\n  defaults: { ref: main }\n  units:\n    - { repo: r, target: t, unit: a, dependsOn: [r/a] }\n",
                "isn't `repo/target/unit`",
            ),
            (
                "blueprint:\n  defaults: { ref: \"main||x\" }\n",
                "empty candidate",
            ),
            (
                "blueprint:\n  defaults: { ref: main }\n  units:\n    - { repo: R, target: t, unit: a }\n",
                "isn't a valid name",
            ),
        ];
        for (text, expected) in cases {
            let error = load(text).unwrap_err().to_string();
            assert!(error.contains(expected), "{text}\n→ {error}");
        }
    }
}
