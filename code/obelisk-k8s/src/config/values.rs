//! The merged values for a unit on a target: its config files, deep-merged in merge order, with the
//! `obelisk:` block on top. This is what a generator or chart gets.
//!
//! Maps merge key by key, recursively; anything else (lists, scalars, `null`) replaces what was there.
//! A file that's empty or `null` adds nothing. `obelisk:` is the tool's: a file may only set
//! `obelisk.release.{name, namespace}`, as strings (parsed into [`ValuesCase::releases`], and left out
//! of the values), and anything else under it is an error. `ref+awsssm` refs aren't resolved yet.
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
use crate::config::paths::{CASE_HEADER, Selection};
use crate::config::repo::{RepoConfig, Target, Unit};
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
    /// What each config file (relative to the repo root) set under `obelisk.release`, in merge order;
    /// files that set nothing are left out. Parsed, not used yet (it's not in `values`).
    pub releases: Releases,
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
    ) -> Result<ValuesReport<'a>, ObeliskK8sError> {
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| self.values_for(repo_root, shown_root, target, unit))
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
    ) -> Result<ValuesCase<'a>, ObeliskK8sError> {
        let (values, releases) = self.config_values(repo_root, shown_root, target, unit)?;
        ValuesCase::new(target, unit, values, releases)
    }

    /// The `obelisk:` block, then `unit`'s config files on `target` merged; and what the files set
    /// under `obelisk.release`.
    pub(crate) fn config_values(
        &self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &Target,
        unit: &Unit,
    ) -> Result<(Map<String, Value>, Releases), ObeliskK8sError> {
        let mut values = Map::new();
        values.insert(OBELISK_KEY.to_owned(), self.obelisk_block(target, unit));
        let mut releases = Vec::new();
        for path in self
            .resolve(repo_root, target, unit)?
            .iter()
            .filter(|p| p.found)
        {
            let shown = shown_under(shown_root, &path.path);
            let file = yaml::read_values(&repo_root.join(&path.path), &shown)?;
            absorb(
                &mut values,
                &mut releases,
                file,
                &path.path,
                &shown,
                "a config file",
            )?;
        }
        Ok((values, releases))
    }

    /// What the tool knows about the case.
    fn obelisk_block(&self, target: &Target, unit: &Unit) -> Value {
        let mut block = Map::new();
        block.insert("repo".to_owned(), Value::String(self.name.clone()));
        block.insert("target".to_owned(), Value::String(target.name.clone()));
        block.insert("unit".to_owned(), Value::String(unit.name.clone()));
        Value::Object(block)
    }
}

/// What each file set under `obelisk.release` (relative to the repo root), in merge order.
pub type Releases = Vec<(PathBuf, FileRelease)>;

impl<'a> ValuesCase<'a> {
    pub(crate) fn new(
        target: &'a Target,
        unit: &'a Unit,
        values: Map<String, Value>,
        releases: Releases,
    ) -> Result<Self, ObeliskK8sError> {
        let yaml = values_yaml(&values, target, unit)?;
        Ok(Self {
            target: &target.name,
            unit: &unit.name,
            values,
            yaml,
            releases,
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
/// mapping or nothing, and may only set `obelisk.release.{name, namespace}` under `obelisk:`, which is
/// recorded in `releases` (as `path`) and kept out of `values`. `what` names the kind of file.
pub(crate) fn absorb(
    values: &mut Map<String, Value>,
    releases: &mut Releases,
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
                let release = release_in_file(&obelisk, what).map_err(error)?;
                if release != FileRelease::default() {
                    releases.push((path.to_owned(), release));
                }
            }
            merge(values, file);
            Ok(())
        }
        _ => Err(error(format!("{what} must be a mapping (or empty)"))),
    }
}

/// What a config file says about the Helm release, under `obelisk.release`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileRelease {
    pub name: Option<String>,
    pub namespace: Option<String>,
}

/// The `obelisk:` a config file set: `null`, or `{ release: null | { name?, namespace? } }`, where both
/// are strings (or `null`). Values files are typed, so a plain `123` is a number: quote it.
fn release_in_file(obelisk: &Value, what: &str) -> Result<FileRelease, String> {
    let only = format!(
        "`{OBELISK_KEY}:` is reserved; {what} may only set `{OBELISK_KEY}.release.name` and `{OBELISK_KEY}.release.namespace`"
    );
    let obelisk = match obelisk {
        Value::Null => return Ok(FileRelease::default()),
        Value::Object(map) => map,
        _ => return Err(format!("{only}, so `{OBELISK_KEY}:` must be a mapping")),
    };
    if let Some(other) = obelisk.keys().find(|k| *k != "release") {
        return Err(format!("{only}, not `{OBELISK_KEY}.{other}`"));
    }
    let release = match obelisk.get("release") {
        None | Some(Value::Null) => return Ok(FileRelease::default()),
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
    let field = |key: &str| match release.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "`{OBELISK_KEY}.release.{key}` must be a string, not `{other}` (quote it)"
        )),
    };
    Ok(FileRelease {
        name: field("name")?,
        namespace: field("namespace")?,
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
        for (i, case) in self.cases.iter().enumerate() {
            if !self.single {
                if i > 0 {
                    writeln!(f, "---")?;
                }
                writeln!(
                    f,
                    "{CASE_HEADER}# [ target={}, unit={} ]{CASE_HEADER:#}",
                    case.target, case.unit
                )?;
            }
            f.write_str(&case.yaml)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::REPO_CONFIG_DIR;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(REPO_CONFIG_DIR).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
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
            .values_report(dir.path(), &selection, None)?
            .to_string();
        Ok(anstream::adapter::strip_str(&text).to_string())
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
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n\
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
        let body = text.split_once("unit: app\n").unwrap().1;
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
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n"
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

    fn case_releases(dir: &tempfile::TempDir) -> Vec<(PathBuf, FileRelease)> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".into()], &["app".into()]).unwrap();
        let report = config.values_report(dir.path(), &selection, None).unwrap();
        report.cases.into_iter().next().unwrap().releases
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
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\nenv: prod\n"
        );
        let release = |name: Option<&str>, namespace: &str| FileRelease {
            name: name.map(str::to_owned),
            namespace: Some(namespace.to_owned()),
        };
        assert_eq!(
            case_releases(&dir),
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
            assert_eq!(case_releases(&dir), [], "{empty:?}");
        }
    }

    #[test]
    fn reserves_the_rest_of_the_obelisk_key() {
        let only = "`obelisk:` is reserved; a config file may only set `obelisk.release.name` and `obelisk.release.namespace`";
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
        assert_eq!(case_releases(&dir)[0].1.name.as_deref(), Some("123"));
    }

    #[test]
    fn shows_several_cases_under_headers() {
        let dir = base(&[("dev.yaml", "env: dev\n"), ("prod.yaml", "env: prod\n")]);
        assert_eq!(
            report(&dir, &[], &[]).unwrap(),
            "# [ target=dev, unit=app ]\nobelisk:\n  repo: r\n  target: dev\n  unit: app\nenv: dev\n---\n\
             # [ target=prod, unit=app ]\nobelisk:\n  repo: r\n  target: prod\n  unit: app\nenv: prod\n"
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
            .values_report(dir.path(), &selection, Some(Path::new("~/dev/r")))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "~/dev/r/.obelisk-k8s/prod.yaml: a config file must be a mapping (or empty)"
        );
    }
}
