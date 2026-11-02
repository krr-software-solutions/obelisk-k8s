//! The merged values for a unit on a target: its config files, deep-merged in merge order, with the
//! `obelisk:` block on top. This is what a generator or chart gets.
//!
//! Maps merge key by key, recursively; anything else (lists, scalars, `null`) replaces what was there.
//! A file that's empty or `null` adds nothing. `obelisk:` is the tool's: a file may only set
//! `obelisk.release.{name, namespace}` and `obelisk.version`, as strings (parsed into
//! [`ValuesCase::sets`] and left out of the values: the version and the release go back in once
//! they're agreed, after the generators), and anything else under it is an error. `ref+awsssm` refs aren't resolved yet.
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
use crate::config::ephemeral::Ephemeral;
use crate::config::global::{Cluster, GlobalConfig};
use crate::config::paths::{Selection, write_yaml_cases};
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::yaml;

/// The key the tool's own block goes under, at the top of the merged values.
pub const OBELISK_KEY: &str = "obelisk";

/// One (target, unit) case's merged values, rendered.
#[derive(Debug)]
pub struct ValuesCase<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// The `obelisk:` block first, then the merged files' keys in the order they first appear.
    pub values: Map<String, Value>,
    /// `values` as YAML.
    pub yaml: String,
    /// What each file (relative to the repo root, or the ephemeral values as shown) set under
    /// `obelisk:`, in merge order; files that set nothing are left out. The release and the version
    /// are agreed on after the generators.
    pub sets: FileObelisks,
    /// What the case agreed on, after the generators (`None` before them, in `template values`).
    pub agreed: Option<Agreed>,
}

/// What a case agrees on after its generators: one value each, from wherever it's given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreed {
    /// `obelisk.release.name`: `.Release.Name`, and the Helm release.
    pub release_name: String,
    /// `obelisk.release.namespace`: `.Release.Namespace`, and where the release goes.
    pub namespace: String,
    /// `obelisk.version`: the chart's `version` and `appVersion`.
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
    /// The merged values of every case in `selection`. `shown_root` is where the repo is, as the user
    /// should see it, when that isn't the current directory (paths in errors are shown under it).
    ///
    /// # Errors
    ///
    /// Whatever reading a config file fails with; [`ObeliskK8sError::Config`] for a config file that
    /// isn't a mapping, or sets `obelisk:`.
    pub fn values_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
    ) -> Result<ValuesReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global)?;
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| self.values_for(repo_root, shown_root, target, unit, &inputs))
            .collect::<Result<_, _>>()?;
        Ok(ValuesReport {
            cases,
            single: selection.single,
        })
    }

    fn values_for<'a>(
        &'a self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<ValuesCase<'a>, ObeliskK8sError> {
        let (values, sets) = self.config_values(repo_root, shown_root, target, unit, inputs)?;
        ValuesCase::new(target, unit, values, sets, None)
    }

    /// The `obelisk:` block, then `unit`'s config files on `target` merged, then the ephemeral values
    /// (on an ephemeral target); and what the files set under `obelisk:`.
    pub(crate) fn config_values(
        &self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &Target,
        unit: &Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<(Map<String, Value>, FileObelisks), ObeliskK8sError> {
        let ephemeral = inputs.ephemeral_for(target);
        let mut values = Map::new();
        values.insert(
            OBELISK_KEY.to_owned(),
            self.obelisk_block(target, unit, inputs)?,
        );
        let mut sets = Vec::new();
        for path in self
            .resolve(repo_root, target, unit)?
            .iter()
            .filter(|p| p.found)
        {
            let shown = shown_under(shown_root, &path.path);
            let file = yaml::read_values(&repo_root.join(&path.path), &shown)?;
            absorb(
                &mut values,
                &mut sets,
                file,
                &path.path,
                &shown,
                "a config file",
            )?;
        }
        if let Some(ephemeral) = ephemeral {
            absorb(
                &mut values,
                &mut sets,
                ephemeral.values().clone(),
                ephemeral.shown(),
                ephemeral.shown(),
                "the ephemeral values",
            )?;
        }
        Ok((values, sets))
    }

    /// What the tool knows about the case.
    fn obelisk_block(
        &self,
        target: &Target,
        unit: &Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<Value, ObeliskK8sError> {
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
        block.insert("paths".to_owned(), inputs.paths.clone());
        Ok(Value::Object(block))
    }
}

/// `obelisk.cluster`: what the global config says about a cluster: its name, and the obeliskd
/// supernet and subnet it runs on (`null` without `obeliskd`).
fn cluster_block(cluster: &Cluster) -> Value {
    let obeliskd = cluster.obeliskd.as_ref();
    serde_json::json!({
        "name": cluster.name,
        "supernet": obeliskd.map(|o| &o.supernet),
        "subnet": obeliskd.map(|o| &o.subnet),
    })
}

/// What each file set under `obelisk:`, in merge order: by its path relative to the repo root, or for
/// the ephemeral values, as [`Ephemeral`] shows them (the path as given, or `--ephemeral-values`).
pub type FileObelisks = Vec<(PathBuf, FileObelisk)>;

impl<'a> ValuesCase<'a> {
    pub(crate) fn new(
        target: &'a Target,
        unit: &'a Unit,
        values: Map<String, Value>,
        sets: FileObelisks,
        agreed: Option<Agreed>,
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

/// Merges one file's values (a config file, or a generator's output) into `values`: it must be a
/// mapping or nothing, and may only set `obelisk.release.{name, namespace}` and `obelisk.version` under
/// `obelisk:`, which is recorded in `sets` (as `path`) and kept out of `values`. `what` names the kind
/// of file.
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

/// What a config file (or generator) set under `obelisk:`: the Helm release's name and namespace
/// (`obelisk.release.{name, namespace}`), and the version (`obelisk.version`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileObelisk {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub version: Option<String>,
}

/// The `obelisk:` a config file set: `null`, or `{ release?: null | { name?, namespace? }, version? }`,
/// where each is a string (or `null`). Values files are typed, so a plain `123` is a number: quote it.
fn obelisk_in_file(obelisk: &Value, what: &str) -> Result<FileObelisk, String> {
    let only = format!(
        "`{OBELISK_KEY}:` is reserved; {what} may only set `{OBELISK_KEY}.release.name`, `{OBELISK_KEY}.release.namespace` and `{OBELISK_KEY}.version`"
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
    if let Some(other) = obelisk.keys().find(|k| *k != "release" && *k != "version") {
        return Err(format!("{only}, not `{OBELISK_KEY}.{other}`"));
    }
    let version = string(obelisk, "version", "version")?;
    let release = match obelisk.get("release") {
        None | Some(Value::Null) => {
            return Ok(FileObelisk {
                version,
                ..FileObelisk::default()
            });
        }
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(format!(
                "{only}, so `{OBELISK_KEY}.release` must be a mapping"
            ));
        }
    };
    if let Some(other) = release.keys().find(|k| *k != "name" && *k != "namespace") {
        return Err(format!("{only}, not `{OBELISK_KEY}.release.{other}`"));
    }
    Ok(FileObelisk {
        name: string(release, "name", "release.name")?,
        namespace: string(release, "namespace", "release.namespace")?,
        version,
    })
}

/// Deep-merges `from` into `into`: maps key by key, anything else replaces.
fn merge(into: &mut Map<String, Value>, from: Map<String, Value>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{global, write_repo};

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        write_repo(files)
    }

    const REPO: &str = "repo:\n  name: r\n  configPaths:\n    - ./global.yaml\n    - \"./${{obelisk:target}}.yaml\"\n    - \"./${{obelisk:unit}}.yaml\"\n";

    fn base(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let mut files = vec![
            ("repo.yaml", REPO),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit: {}\n"),
        ];
        files.extend_from_slice(extra);
        repo(&files)
    }

    fn report(
        dir: &tempfile::TempDir,
        targets: &[&str],
        units: &[&str],
    ) -> Result<String, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let owned = |n: &[&str]| n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let selection = config.select(&owned(targets), &owned(units)).unwrap();
        let text = config
            .values_report(dir.path(), &selection, None, None, &global())?
            .to_string();
        Ok(anstream::adapter::strip_str(&text)
            .to_string()
            .replace(&dir.path().display().to_string(), "<repo>"))
    }

    fn ephemeral_report(
        dir: &tempfile::TempDir,
        targets: &[&str],
        ephemeral: Option<&str>,
    ) -> Result<String, ObeliskK8sError> {
        use crate::config::ephemeral::EphemeralValues;
        let config = RepoConfig::load(dir.path()).unwrap();
        let owned = |n: &[&str]| n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let selection = config.select(&owned(targets), &[]).unwrap();
        let ephemeral = ephemeral
            .map(|v| Ephemeral::new("pr-7".into(), EphemeralValues::Inline(v.into())))
            .transpose()?;
        let text = config
            .values_report(dir.path(), &selection, None, ephemeral.as_ref(), &global())?
            .to_string();
        Ok(anstream::adapter::strip_str(&text)
            .to_string()
            .replace(&dir.path().display().to_string(), "<repo>"))
    }

    fn ephemeral_base() -> tempfile::TempDir {
        base(&[
            (
                "targets/preview/target.yaml",
                "target:\n  cluster: c\n  ephemeral: true\n",
            ),
            (
                "targets/qa/target.yaml",
                "target:\n  cluster: c\n  ephemeral: null\n",
            ),
            ("global.yaml", "a: global\nb: global\n"),
            ("preview.yaml", "a: preview\nc: preview\n"),
            ("app.yaml", "c: app\n"),
        ])
    }

    #[test]
    fn ephemeral_values_are_the_last_layer() {
        let dir = ephemeral_base();
        assert_eq!(
            ephemeral_report(
                &dir,
                &["preview"],
                Some("c: ephemeral\nobelisk:\n  version: \"1.0.0\"\n")
            )
            .unwrap(),
            "obelisk:\n  repo: r\n  target: preview\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: pr-7\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\n\
             a: preview\nb: global\nc: ephemeral\n"
        );
        // A plain target gets `ephemeral: null`, and none of the ephemeral values.
        let both = ephemeral_report(&dir, &["preview", "prod"], Some("c: ephemeral\n")).unwrap();
        let prod = both.split("target=prod").nth(1).unwrap();
        assert!(prod.contains("  ephemeral: null\n"), "{prod}");
        assert!(!prod.contains("c: ephemeral"), "{prod}");
    }

    #[test]
    fn ephemeral_values_set_obelisk_like_a_config_file() {
        let dir = ephemeral_base();
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["preview".to_owned()], &[]).unwrap();
        let ephemeral = Ephemeral::new(
            "pr-7".into(),
            crate::config::ephemeral::EphemeralValues::Inline(
                "obelisk:\n  version: \"1.0.0\"\n".into(),
            ),
        )
        .unwrap();
        let report = config
            .values_report(dir.path(), &selection, None, Some(&ephemeral), &global())
            .unwrap();
        assert_eq!(
            report.cases[0].sets,
            [(
                PathBuf::from("--ephemeral-values"),
                FileObelisk {
                    version: Some("1.0.0".to_owned()),
                    ..FileObelisk::default()
                }
            )]
        );
        let error = ephemeral_report(&dir, &["preview"], Some("obelisk:\n  target: x\n"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("--ephemeral-values: "), "{error}");
        let error = ephemeral_report(&dir, &["preview"], Some("[1]"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the ephemeral values must be a mapping"),
            "{error}"
        );
    }

    #[test]
    fn an_ephemeral_target_needs_an_ephemeral_and_only_it_does() {
        let dir = ephemeral_base();
        // Selected by default (no `-t`) too.
        for targets in [&["preview"][..], &[]] {
            assert_eq!(
                ephemeral_report(&dir, targets, None)
                    .unwrap_err()
                    .to_string(),
                "target `preview` is ephemeral: pass `--ephemeral-name`, and `--ephemeral-values` or `--ephemeral-values-path`; or pick other targets with `-t`"
            );
        }
        assert_eq!(
            ephemeral_report(&dir, &["prod", "qa"], Some("{}"))
                .unwrap_err()
                .to_string(),
            "ephemeral `pr-7` given, but none of the targets being rendered is ephemeral (`ephemeral: true`)"
        );
        assert!(ephemeral_report(&dir, &["prod", "qa"], None).is_ok());
        assert!(ephemeral_report(&dir, &[], Some("")).is_ok());
    }

    #[test]
    fn the_obelisk_block_has_the_targets_cluster() {
        let dir = base(&[
            ("targets/prod/target.yaml", "target:\n  cluster: obd\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: nope\n"),
        ]);
        let text = report(&dir, &["prod"], &["app"]).unwrap();
        assert!(
            text.contains("  cluster:\n    name: obd\n    supernet: main\n    subnet: sub\n"),
            "{text}"
        );
        assert_eq!(
            report(&dir, &["dev"], &["app"]).unwrap_err().to_string(),
            "target `dev` deploys to cluster `nope`, which isn't in the global config at ~/.obelisk-k8s (it has: c, obd): add it with `obelisk-k8s config global cluster add`"
        );
    }

    #[test]
    fn merges_in_order_with_the_obelisk_block_first() {
        let dir = base(&[
            (
                "global.yaml",
                "db:\n  host: global\n  port: 5432\nlist: [1, 2]\nkeep: g\n",
            ),
            (
                "prod.yaml",
                "db:\n  host: prod\n  pool: { max: 10 }\nlist: [3]\n",
            ),
            ("app.yaml", "db:\n  pool: { min: 1 }\nextra: null\n"),
        ]);
        assert_eq!(
            report(&dir, &["prod"], &["app"]).unwrap(),
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\n\
             db:\n  host: prod\n  port: 5432\n  pool:\n    max: 10\n    min: 1\n\
             list:\n- 3\nkeep: g\nextra: null\n"
        );
    }

    #[test]
    fn keeps_quoted_strings_and_reads_plain_scalars_the_yaml_1_1_way() {
        let dir = base(&[(
            "global.yaml",
            "quoted: [\"0755\", \"yes\", \"1e3\", \"null\", \"\"]\n\
             mode: 0755\nperm: 0644\nhex: 0x1F\nflag: Yes\nn: 3\nf: 1.50\n\
             tpl: |\n  {{ .Values.x }}\n  line two\nref: \"ref+awsssm:///app/x\"\n",
        )]);
        let text = report(&dir, &["prod"], &["app"]).unwrap();
        let body = text.split_once("repoRoot: <repo>\n").unwrap().1;
        // (The key `n` comes out quoted: plain, it's a YAML 1.1 boolean.)
        assert_eq!(
            body,
            "quoted:\n- \"0755\"\n- \"yes\"\n- \"1e3\"\n- \"null\"\n- \"\"\n\
             mode: 493\nperm: 420\nhex: 31\nflag: true\n\"n\": 3\nf: 1.5\n\
             tpl: |\n  {{ .Values.x }}\n  line two\nref: ref+awsssm:///app/x\n"
        );
    }

    #[test]
    fn known_limits_of_plain_scalars() {
        // Documented in the module: without a `.`, these aren't what a YAML 1.1 reader makes of them.
        let dir = base(&[("global.yaml", "e: 1e3\nz: 08\n")]);
        let text = report(&dir, &["prod"], &["app"]).unwrap();
        assert!(text.ends_with("e: 1000.0\nz: 8.0\n"), "{text}");
    }

    #[test]
    fn skips_empty_and_missing_files() {
        let dir = base(&[("global.yaml", ""), ("prod.yaml", "# nothing\n")]);
        assert_eq!(
            report(&dir, &["prod"], &["app"]).unwrap(),
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\n"
        );
    }

    #[test]
    fn rejects_a_file_that_isnt_a_mapping() {
        let dir = base(&[("prod.yaml", "- a\n- b\n")]);
        let error = report(&dir, &["prod"], &["app"]).unwrap_err().to_string();
        assert_eq!(
            error,
            ".obelisk-k8s/prod.yaml: a config file must be a mapping (or empty)"
        );
        let dir = base(&[("prod.yaml", "a: [\n")]);
        let error = report(&dir, &["prod"], &["app"]).unwrap_err().to_string();
        assert!(error.contains(".obelisk-k8s/prod.yaml:"), "{error}");
    }

    fn case_sets(dir: &tempfile::TempDir) -> Vec<(PathBuf, FileObelisk)> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".into()], &["app".into()]).unwrap();
        let report = config
            .values_report(dir.path(), &selection, None, None, &global())
            .unwrap();
        report.cases.into_iter().next().unwrap().sets
    }

    #[test]
    fn keeps_a_config_files_release_out_of_the_values() {
        let dir = base(&[
            ("global.yaml", "obelisk:\n  release:\n    namespace: ns\n"),
            (
                "prod.yaml",
                "obelisk:\n  release:\n    name: app\n    namespace: ns\nenv: prod\n",
            ),
        ]);
        assert_eq!(
            report(&dir, &["prod"], &["app"]).unwrap(),
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\nenv: prod\n"
        );
        let release = |name: Option<&str>, namespace: &str| FileObelisk {
            name: name.map(str::to_owned),
            namespace: Some(namespace.to_owned()),
            version: None,
        };
        assert_eq!(
            case_sets(&dir),
            [
                (
                    PathBuf::from(".obelisk-k8s/global.yaml"),
                    release(None, "ns")
                ),
                (
                    PathBuf::from(".obelisk-k8s/prod.yaml"),
                    release(Some("app"), "ns")
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
            let dir = base(&[("prod.yaml", empty)]);
            assert_eq!(case_sets(&dir), [], "{empty:?}");
        }
    }

    #[test]
    fn reserves_the_rest_of_the_obelisk_key() {
        let only = "`obelisk:` is reserved; a config file may only set `obelisk.release.name`, `obelisk.release.namespace` and `obelisk.version`";
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
                "obelisk: [a]\n",
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
            // Values files are typed: plain numbers are numbers (`0755` is 493).
            (
                "obelisk:\n  release: { name: 123 }\n",
                "`obelisk.release.name` must be a string, not `123` (quote it)".to_owned(),
            ),
            (
                "obelisk:\n  release: { namespace: true }\n",
                "`obelisk.release.namespace` must be a string, not `true` (quote it)".to_owned(),
            ),
        ] {
            let dir = base(&[("prod.yaml", bad)]);
            let error = report(&dir, &["prod"], &["app"]).unwrap_err().to_string();
            assert_eq!(error, format!(".obelisk-k8s/prod.yaml: {why}"), "{bad:?}");
        }
        let dir = base(&[("prod.yaml", "obelisk:\n  release: { name: \"123\" }\n")]);
        assert_eq!(case_sets(&dir)[0].1.name.as_deref(), Some("123"));
    }

    #[test]
    fn shows_several_cases_under_headers() {
        let dir = base(&[("dev.yaml", "env: dev\n"), ("prod.yaml", "env: prod\n")]);
        assert_eq!(
            report(&dir, &[], &[]).unwrap(),
            "# [ target=dev, unit=app ]\nobelisk:\n  repo: r\n  target: dev\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\nenv: dev\n---\n\
             # [ target=prod, unit=app ]\nobelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\nenv: prod\n"
        );
    }

    #[test]
    fn several_cases_are_a_yaml_stream() {
        let dir = base(&[("dev.yaml", "env: dev\n"), ("prod.yaml", "env: prod\n")]);
        let text = report(&dir, &[], &[]).unwrap();
        let docs: Vec<Value> = serde_saphyr::from_str_multiple(&text).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1]["env"], "prod");
    }

    #[test]
    fn errors_show_paths_under_the_repo_when_asked() {
        let dir = base(&[("prod.yaml", "- a\n")]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".into()], &["app".into()]).unwrap();
        let error = config
            .values_report(
                dir.path(),
                &selection,
                Some(Path::new("~/dev/r")),
                None,
                &global(),
            )
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "~/dev/r/.obelisk-k8s/prod.yaml: a config file must be a mapping (or empty)"
        );
    }

    #[test]
    fn the_obelisk_block_has_the_git_facts() {
        // A temp dir isn't a git repo; `git::facts` itself is tested in `git.rs`.
        let dir = base(&[]);
        let text = report(&dir, &["prod"], &["app"]).unwrap();
        assert!(
            text.starts_with(
                "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn keeps_a_config_files_version_out_of_the_values() {
        let dir = base(&[("prod.yaml", "obelisk:\n  version: v1.2.3\n")]);
        assert!(
            !report(&dir, &["prod"], &["app"])
                .unwrap()
                .contains("version")
        );
        assert_eq!(case_sets(&dir)[0].1.version.as_deref(), Some("v1.2.3"));
        let dir = base(&[("prod.yaml", "obelisk:\n  version: 1.5\n")]);
        let e = report(&dir, &["prod"], &["app"]).unwrap_err().to_string();
        assert!(
            e.ends_with("`obelisk.version` must be a string, not `1.5` (quote it)"),
            "{e}"
        );
    }
}
