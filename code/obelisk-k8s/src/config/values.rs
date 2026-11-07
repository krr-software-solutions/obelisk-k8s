//! Values: the `obelisk:` block, how values merge, and what `template values` prints (the values a
//! case ends up with after its phases: see `config::phases`).
//!
//! Maps merge key by key, recursively; anything else (lists, scalars, `null`) replaces what was there.
//! An output that's empty or `null` adds nothing. `obelisk:` is the tool's: a values source may only
//! set `obelisk.release.{name, namespace, version}`, as strings (parsed into [`ValuesCase::sets`] and
//! left out of the values: the release goes back in once it's agreed, after the phases), and anything
//! else under it is an error.
//!
//! Values go through a generic tree, so quoted strings stay strings (`"0755"`, `"yes"`, `"1e3"` come out
//! quoted) and block scalars keep their text, but plain scalars come out in their parsed form:
//! - plain octal is read the YAML 1.1 way, as its readers (Helm) do: `0755` comes out as `493`;
//! - numbers are re-spelled: `0x1F` → `31`, `1.50` → `1.5`;
//! - YAML 1.1 booleans become `true` / `false` (`yes`, `On`, `N`, …);
//! - **not what a YAML 1.1 reader would make of them:** plain number-like scalars without a `.`
//!   (`1e3`, `08`, and integers past 64 bits) become floats (`1000.0`, `8.0`, `1.8e19`);
//! - `.inf` / `.nan` are an error, and tags are dropped.
//!
//! Quote a value to keep it exactly as written.

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ObeliskK8sError;
use crate::config::global::{Cluster, normalize};
use crate::config::paths::write_yaml_cases;
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::run::{RunInputs, utf8};
use crate::yaml;

/// The key the tool's own block goes under, at the top of the merged values.
pub const OBELISK_KEY: &str = "obelisk";

/// One (target, unit) case's values, after its phases.
#[derive(Debug)]
pub struct ValuesCase<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// The `obelisk:` block first (with the agreed `release`), then the merged outputs' keys in the
    /// order they first appear.
    pub values: Map<String, Value>,
    /// `values` as YAML.
    pub yaml: String,
    /// What each values source (its path relative to the repo root, URL, program, or where it's
    /// written inline), and the ephemeral values (as shown), set under `obelisk:`, in merge order;
    /// those that set nothing are left out.
    pub sets: FileObelisks,
    /// What the case agreed on, after its values phases.
    pub agreed: Agreed,
}

/// What a case agrees on after its values phases: one value each, from wherever it's given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreed {
    /// `obelisk.release.name`: `.Release.Name`, and the Helm release.
    pub release_name: String,
    /// `obelisk.release.namespace`: `.Release.Namespace`, and where the release goes.
    pub namespace: String,
    /// `obelisk.release.version`: the chart's `version` and `appVersion`.
    pub version: String,
}

/// What `template values` prints: a single case's values as plain YAML, or several as a YAML stream,
/// each document under a magenta `# [ target=…, unit=… ]` comment.
#[derive(Debug)]
pub struct ValuesReport<'a> {
    pub cases: Vec<ValuesCase<'a>>,
    pub single: bool,
}

impl RepoConfig {
    /// What the tool knows about the case, as values: just `obelisk: { repo, target, unit, cluster,
    /// ephemeral, git, paths }`, `paths` with the case's `unit` and `target` directories (the release
    /// goes in once it's agreed). What the build file renders with, and what the phases start from (with the ephemeral values over it). Made once per case.
    pub(crate) fn obelisk_values(
        &self,
        target: &Target,
        unit: &Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<Map<String, Value>, ObeliskK8sError> {
        let ephemeral = inputs.ephemeral_for(target);
        let mut block = Map::new();
        block.insert("repo".to_owned(), Value::String(self.name.clone()));
        block.insert("target".to_owned(), Value::String(target.name.clone()));
        block.insert("unit".to_owned(), Value::String(unit.name.clone()));
        block.insert("cluster".to_owned(), cluster_block(inputs.cluster(target)?));
        block.insert(
            "ephemeral".to_owned(),
            ephemeral.map_or(Value::Null, |e| Value::String(e.name.clone())),
        );
        block.insert("git".to_owned(), inputs.git.clone());
        let paths = case_paths(&inputs.paths, &inputs.repo_root, target, unit)?;
        block.insert("paths".to_owned(), paths);
        let mut values = Map::new();
        values.insert(OBELISK_KEY.to_owned(), Value::Object(block));
        Ok(values)
    }
}

/// `obelisk.cluster`: what the global config says about a cluster: its name, the kubectl context
/// it's reached through (`null` without one), and the obeliskd supernet and subnet it runs on
/// (`null` without `obeliskd`).
fn cluster_block(cluster: &Cluster) -> Value {
    let obeliskd = cluster.obeliskd.as_ref();
    serde_json::json!({
        "name": cluster.name,
        "context": cluster.context(),
        "supernet": obeliskd.map(|o| &o.supernet),
        "subnet": obeliskd.map(|o| &o.subnet),
    })
}

/// `obelisk.paths`: the run's (`paths`: `repo` and `global`), plus the case's `unit` and `target`
/// directories (where their `unit.yaml` / `target.yaml` are, under the absolute, normalized
/// `repo_root`), absolute and normalized like them.
fn case_paths(
    paths: &Value,
    repo_root: &Path,
    target: &Target,
    unit: &Unit,
) -> Result<Value, ObeliskK8sError> {
    let mut paths = paths.as_object().cloned().unwrap_or_default();
    for (key, file) in [("unit", &unit.file), ("target", &target.file)] {
        let dir = normalize(&repo_root.join(file.parent().unwrap_or(Path::new(""))));
        paths.insert(key.to_owned(), Value::String(utf8(&dir)?.to_owned()));
    }
    Ok(Value::Object(paths))
}

/// What each values source set under `obelisk:`, in merge order: as [`ValuesCase::sets`] describes
/// them; the ephemeral values as `Ephemeral` shows them (the path as given, or
/// `--ephemeral-values`).
pub type FileObelisks = Vec<(PathBuf, FileObelisk)>;

impl<'a> ValuesCase<'a> {
    pub(crate) fn new(
        target: &'a Target,
        unit: &'a Unit,
        values: Map<String, Value>,
        sets: FileObelisks,
        agreed: Agreed,
    ) -> Result<Self, ObeliskK8sError> {
        let yaml = values_yaml(&values, target, unit)?;
        Ok(Self {
            target: &target.name,
            unit: &unit.name,
            values,
            yaml,
            sets,
            agreed,
        })
    }
}

/// `values` (a case's) as YAML.
pub(crate) fn values_yaml(
    values: &Map<String, Value>,
    target: &Target,
    unit: &Unit,
) -> Result<String, ObeliskK8sError> {
    let name = format!(
        "values for unit `{}` on target `{}`",
        unit.name, target.name
    );
    yaml::to_string(values, Path::new(&name))
}

/// `path` (relative to the repo root) as the user should see it.
pub(crate) fn shown_under(shown_root: Option<&Path>, path: &Path) -> PathBuf {
    shown_root.map_or_else(|| path.to_owned(), |root| root.join(path))
}

/// Merges one output (a values source's, or the ephemeral values) into `values`: it must be a mapping
/// or nothing, and may only set `obelisk.release.{name, namespace, version}` under `obelisk:`, which
/// is recorded in `sets` (as `path`) and kept out of `values`. `what` names the kind of output.
pub(crate) fn absorb(
    values: &mut Map<String, Value>,
    sets: &mut FileObelisks,
    file: Value,
    path: &Path,
    shown: &Path,
    what: &str,
) -> Result<(), ObeliskK8sError> {
    let error = |message: String| ObeliskK8sError::Config {
        file: shown.to_owned(),
        at: None,
        message,
    };
    match file {
        Value::Null => Ok(()),
        Value::Object(mut file) => {
            if let Some(obelisk) = file.remove(OBELISK_KEY) {
                let set = obelisk_in_file(&obelisk, what).map_err(error)?;
                if set != FileObelisk::default() {
                    sets.push((path.to_owned(), set));
                }
            }
            merge(values, file);
            Ok(())
        }
        _ => Err(error(format!("{what} must be a mapping (or empty)"))),
    }
}

/// What a values source set under `obelisk:`: the Helm release's name, namespace and version
/// (`obelisk.release.{name, namespace, version}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileObelisk {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub version: Option<String>,
}

/// The `obelisk:` a values source set: `null`, or `{ release?: null | { name?, namespace?, version? } }`,
/// where each is a string (or `null`). Values files are typed, so a plain `123` is a number: quote it.
fn obelisk_in_file(obelisk: &Value, what: &str) -> Result<FileObelisk, String> {
    let only = format!(
        "`{OBELISK_KEY}:` is reserved; {what} may only set `{OBELISK_KEY}.release.name`, `{OBELISK_KEY}.release.namespace` and `{OBELISK_KEY}.release.version`"
    );
    let string = |map: &Map<String, Value>, key: &str, shown: &str| match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "`{OBELISK_KEY}.{shown}` must be a string, not `{other}` (quote it)"
        )),
    };
    let obelisk = match obelisk {
        Value::Null => return Ok(FileObelisk::default()),
        Value::Object(map) => map,
        _ => return Err(format!("{only}, so `{OBELISK_KEY}:` must be a mapping")),
    };
    // Where the version used to be: say where it went.
    if obelisk.contains_key("version") {
        return Err(format!(
            "`{OBELISK_KEY}.version` is now `{OBELISK_KEY}.release.version`: move it under `{OBELISK_KEY}.release`"
        ));
    }
    if let Some(other) = obelisk.keys().find(|k| *k != "release") {
        return Err(format!("{only}, not `{OBELISK_KEY}.{other}`"));
    }
    let release = match obelisk.get("release") {
        None | Some(Value::Null) => return Ok(FileObelisk::default()),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(format!(
                "{only}, so `{OBELISK_KEY}.release` must be a mapping"
            ));
        }
    };
    if let Some(other) = release
        .keys()
        .find(|k| !matches!(k.as_str(), "name" | "namespace" | "version"))
    {
        return Err(format!("{only}, not `{OBELISK_KEY}.release.{other}`"));
    }
    Ok(FileObelisk {
        name: string(release, "name", "release.name")?,
        namespace: string(release, "namespace", "release.namespace")?,
        version: string(release, "version", "release.version")?,
    })
}

/// Deep-merges `from` into `into`: maps key by key, anything else replaces.
pub(crate) fn merge(into: &mut Map<String, Value>, from: Map<String, Value>) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => merge(existing, incoming),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

impl fmt::Display for ValuesReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cases = self
            .cases
            .iter()
            .map(|c| (c.target, c.unit, c.yaml.as_str()));
        write_yaml_cases(f, self.single, "---\n", cases)
    }
}

/// `values` with the agreed release as `obelisk.release.{name, namespace, version}`.
pub(crate) fn with_release(mut values: Map<String, Value>, agreed: &Agreed) -> Map<String, Value> {
    if let Some(Value::Object(obelisk)) = values.get_mut(OBELISK_KEY) {
        obelisk.insert(
            "release".to_owned(),
            serde_json::json!({
                "name": agreed.release_name,
                "namespace": agreed.namespace,
                "version": agreed.version,
            }),
        );
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "a values source's output";

    /// `outputs` absorbed in order (each shown as `<i>.yaml`) over `{ obelisk: { repo: r } }`.
    fn absorbed(outputs: &[&str]) -> Result<(Map<String, Value>, FileObelisks), ObeliskK8sError> {
        let mut values = Map::new();
        values.insert(OBELISK_KEY.to_owned(), serde_json::json!({ "repo": "r" }));
        let mut sets = Vec::new();
        for (i, text) in outputs.iter().enumerate() {
            let shown = PathBuf::from(format!("{i}.yaml"));
            let output = yaml::parse_values(text, &shown)?;
            absorb(&mut values, &mut sets, output, &shown, &shown, OUTPUT)?;
        }
        Ok((values, sets))
    }

    fn error(output: &str) -> String {
        absorbed(&[output]).unwrap_err().to_string()
    }

    #[test]
    fn merges_maps_key_by_key_and_replaces_the_rest() {
        let (values, _) = absorbed(&[
            "db:\n  host: global\n  port: 5432\nlist: [1, 2]\nkeep: g\n",
            "db:\n  host: prod\n  pool: { max: 10 }\nlist: [3]\n",
            "db:\n  pool: { min: 1 }\nextra: null\n",
            "",
            "# nothing\n",
        ])
        .unwrap();
        assert_eq!(
            Value::Object(values),
            serde_json::json!({
                "obelisk": { "repo": "r" },
                "db": { "host": "prod", "port": 5432, "pool": { "max": 10, "min": 1 } },
                "list": [3],
                "keep": "g",
                "extra": null,
            })
        );
    }

    #[test]
    fn an_output_must_be_a_mapping() {
        assert_eq!(
            error("- a\n- b\n"),
            "0.yaml: a values source's output must be a mapping (or empty)"
        );
    }

    #[test]
    fn keeps_what_an_output_sets_out_of_the_values() {
        let (values, sets) = absorbed(&[
            "obelisk:\n  release:\n    namespace: ns\n",
            "obelisk:\n  release:\n    name: app\n    version: v1.2.3\nenv: prod\n",
        ])
        .unwrap();
        assert_eq!(
            Value::Object(values),
            serde_json::json!({ "obelisk": { "repo": "r" }, "env": "prod" })
        );
        assert_eq!(
            sets,
            [
                (
                    PathBuf::from("0.yaml"),
                    FileObelisk {
                        namespace: Some("ns".to_owned()),
                        ..FileObelisk::default()
                    }
                ),
                (
                    PathBuf::from("1.yaml"),
                    FileObelisk {
                        name: Some("app".to_owned()),
                        version: Some("v1.2.3".to_owned()),
                        namespace: None,
                    }
                ),
            ]
        );
    }

    #[test]
    fn an_empty_obelisk_or_release_sets_nothing() {
        for empty in [
            "obelisk: null\n",
            "obelisk: {}\n",
            "obelisk:\n  release: null\n",
            "obelisk:\n  release: {}\n",
            "obelisk:\n  release: { name: null }\n",
        ] {
            assert_eq!(absorbed(&[empty]).unwrap().1, [], "{empty:?}");
        }
    }

    #[test]
    fn reserves_the_rest_of_the_obelisk_key() {
        let only = "`obelisk:` is reserved; a values source's output may only set `obelisk.release.name`, `obelisk.release.namespace` and `obelisk.release.version`";
        for (bad, why) in [
            (
                "obelisk:\n  target: other\n",
                format!("{only}, not `obelisk.target`"),
            ),
            (
                "obelisk:\n  release: { name: app, color: red }\n",
                format!("{only}, not `obelisk.release.color`"),
            ),
            (
                "obelisk: 1\n",
                format!("{only}, so `obelisk:` must be a mapping"),
            ),
            (
                "obelisk:\n  release: app\n",
                format!("{only}, so `obelisk.release` must be a mapping"),
            ),
            (
                "obelisk:\n  release: { name: [a] }\n",
                "`obelisk.release.name` must be a string, not `[\"a\"]` (quote it)".to_owned(),
            ),
            // Values are typed: plain numbers are numbers (`0755` is 493).
            (
                "obelisk:\n  release: { name: 123 }\n",
                "`obelisk.release.name` must be a string, not `123` (quote it)".to_owned(),
            ),
            (
                "obelisk:\n  release: { version: 1.5 }\n",
                "`obelisk.release.version` must be a string, not `1.5` (quote it)".to_owned(),
            ),
            (
                "obelisk:\n  version: v1.2.3\n",
                "`obelisk.version` is now `obelisk.release.version`: move it under `obelisk.release`"
                    .to_owned(),
            ),
        ] {
            assert_eq!(error(bad), format!("0.yaml: {why}"), "{bad:?}");
        }
        let (_, sets) = absorbed(&["obelisk:\n  release: { name: \"123\" }\n"]).unwrap();
        assert_eq!(sets[0].1.name.as_deref(), Some("123"));
    }

    #[test]
    fn adds_the_agreed_release_to_the_obelisk_block() {
        let (values, _) = absorbed(&["a: 1\n"]).unwrap();
        let agreed = Agreed {
            release_name: "app".to_owned(),
            namespace: "ns".to_owned(),
            version: "v1".to_owned(),
        };
        assert_eq!(
            Value::Object(with_release(values, &agreed)),
            serde_json::json!({
                "obelisk": { "repo": "r", "release": { "name": "app", "namespace": "ns", "version": "v1" } },
                "a": 1,
            })
        );
    }

    #[test]
    fn the_cluster_block_names_its_context() {
        use crate::config::global::{K8s, Location};
        let global = crate::config::test_support::global();
        let k3d = global.cluster("c").unwrap();
        assert_eq!(cluster_block(k3d)["context"], "k3d-c");
        let mut remote = k3d.clone();
        (remote.k8s, remote.location) = (K8s::Talos, Location::Remote);
        assert_eq!(cluster_block(&remote)["context"], Value::Null);
        remote.kubectl_context = Some("admin@r".to_owned());
        assert_eq!(cluster_block(&remote)["context"], "admin@r");
    }

    #[test]
    fn paths_add_the_cases_unit_and_target_directories() {
        let unit = Unit {
            name: "app".to_owned(),
            file: ".obelisk-k8s/units/app/unit.yaml".into(),
            build: None,
            targets: crate::config::repo::UnitTargets::All,
            release: crate::config::repo::Release::default(),
        };
        let target = Target {
            name: "prod".to_owned(),
            file: ".obelisk-k8s/targets/prod/target.yaml".into(),
            cluster: "c".to_owned(),
            release: crate::config::repo::Release::default(),
            ephemeral: false,
        };
        let run = serde_json::json!({
            "repo": { "root": "/r", "obelisk": "/r/.obelisk-k8s" },
            "global": { "obelisk": "/home/me/.obelisk-k8s" },
        });
        assert_eq!(
            case_paths(&run, Path::new("/r"), &target, &unit).unwrap(),
            serde_json::json!({
                "repo": { "root": "/r", "obelisk": "/r/.obelisk-k8s" },
                "global": { "obelisk": "/home/me/.obelisk-k8s" },
                "unit": "/r/.obelisk-k8s/units/app",
                "target": "/r/.obelisk-k8s/targets/prod",
            })
        );
    }
}
