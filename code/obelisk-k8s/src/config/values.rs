//! The merged values for a unit on a target: its config files, deep-merged in merge order, with the
//! `obelisk:` block on top. This is what a generator or chart gets.
//!
//! Maps merge key by key, recursively; anything else (lists, scalars, `null`) replaces what was there.
//! A file that's empty or `null` adds nothing. A file that sets `obelisk:` is an error: that key is
//! the tool's. `ref+awsssm` refs aren't resolved yet.
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
        let shown =
            |path: &Path| shown_root.map_or_else(|| path.to_owned(), |root| root.join(path));
        let error = |file: PathBuf, message: String| ObeliskK8sError::Config {
            file,
            at: None,
            message,
        };
        let mut merged = Map::new();
        for path in self
            .resolve(repo_root, target, unit)?
            .iter()
            .filter(|p| p.found)
        {
            let file = shown(&path.path);
            match yaml::read_values(&repo_root.join(&path.path), &file)? {
                Value::Null => {}
                Value::Object(values) if values.contains_key(OBELISK_KEY) => {
                    return Err(error(
                        file,
                        format!("`{OBELISK_KEY}:` is reserved: obelisk-k8s writes it"),
                    ));
                }
                Value::Object(values) => merge(&mut merged, values),
                _ => {
                    return Err(error(
                        file,
                        "a config file must be a mapping (or empty)".to_owned(),
                    ));
                }
            }
        }
        let mut values = Map::new();
        values.insert(OBELISK_KEY.to_owned(), self.obelisk_block(target, unit));
        values.extend(merged);
        let name = format!(
            "values for unit `{}` on target `{}`",
            unit.name, target.name
        );
        let yaml = yaml::to_string(&values, Path::new(&name))?;
        Ok(ValuesCase {
            target: &target.name,
            unit: &unit.name,
            values,
            yaml,
        })
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

    #[test]
    fn reserves_the_obelisk_key() {
        let dir = base(&[("prod.yaml", "obelisk:\n  target: other\n")]);
        let error = report(&dir, &["prod"], &["app"]).unwrap_err().to_string();
        assert_eq!(
            error,
            ".obelisk-k8s/prod.yaml: `obelisk:` is reserved: obelisk-k8s writes it"
        );
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
