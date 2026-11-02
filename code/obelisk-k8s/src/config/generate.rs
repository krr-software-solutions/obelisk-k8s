//! The generated values for a unit on a target (`template generated-values`): its merged config values
//! (see `config::values`), with each of its generators' outputs deep-merged on top, in merge order.
//!
//! Generators are listed like config paths (`generators:` in `repo.yaml`, `target.yaml` and
//! `unit.yaml`; interpolated; missing files skipped). Each is a Go template (`type: gotmpl`), rendered
//! by Helm through a throwaway chart with the merged config values as `.Values`. In a generator,
//! `.Release.Name` is the unit and `.Release.Namespace` is `default`: the real ones aren't known yet,
//! so use `.Values.obelisk` (and `.Template.Name` is the throwaway chart's, not the file's). Every
//! generator sees the same config values, not the others' outputs. Each one is its own `helm` run, so
//! a full `template generated-values` costs targets × units × generators of them.
//!
//! A generator's output must be one mapping, or nothing. Like a config file, it may only set
//! `obelisk.release.{name, namespace}` and `obelisk.version` under `obelisk:` (kept on
//! [`ValuesCase::sets`], not in the values).
//!
//! After the generators, every version given for the case (its target's and unit's `version`, and
//! `obelisk.version` in its config files and generators) must be one and the same; it's added to the
//! values as `obelisk.version`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ObeliskK8sError;
use crate::config::ephemeral::{Ephemeral, RunInputs};
use crate::config::paths::Selection;
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::values::{
    FileObelisks, OBELISK_KEY, ValuesCase, ValuesReport, absorb, shown_under, values_yaml,
};
use crate::interp::InterpolationParams;
use crate::{helm, yaml};

/// What `.Release.Namespace` is while a generator renders.
const GENERATOR_NAMESPACE: &str = "default";

impl RepoConfig {
    /// The generated values of every case in `selection`. `shown_root` is as for
    /// [`RepoConfig::values_report`].
    ///
    /// # Errors
    ///
    /// Whatever building the config values fails with; [`ObeliskK8sError::Helm`] if a generator
    /// doesn't render; [`ObeliskK8sError::Config`] for an output that isn't one mapping, or sets
    /// something under `obelisk:` other than `release`.
    pub fn generated_values_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
    ) -> Result<ValuesReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral)?;
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| self.generated_for(repo_root, shown_root, target, unit, &inputs))
            .collect::<Result<_, _>>()?;
        Ok(ValuesReport {
            cases,
            single: selection.single,
        })
    }

    pub(crate) fn generated_for<'a>(
        &'a self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<ValuesCase<'a>, ObeliskK8sError> {
        let (mut values, mut sets) =
            self.config_values(repo_root, shown_root, target, unit, inputs)?;
        let input = values_yaml(&values, target, unit)?;
        let generators = self.resolve_generators(repo_root, target, unit)?;
        if generators.iter().any(|p| p.found) {
            helm::check_release_name(&unit.name, "unit", "generators")?;
        }
        for path in generators.iter().filter(|p| p.found) {
            let shown = shown_under(shown_root, &path.path);
            let rendered = helm::render(
                &repo_root.join(&path.path),
                &shown,
                &input,
                &unit.name,
                GENERATOR_NAMESPACE,
                None,
            )?;
            let mut documents = yaml::parse_values_stream(&rendered, &shown)?;
            if documents.len() > 1 {
                return Err(ObeliskK8sError::Config {
                    file: shown,
                    at: None,
                    message: format!(
                        "a generator must output one mapping, not {} documents",
                        documents.len()
                    ),
                });
            }
            if let Some(output) = documents.pop() {
                absorb(
                    &mut values,
                    &mut sets,
                    output,
                    &path.path,
                    &shown,
                    "a generator's output",
                )?;
            }
        }
        let version = agree_on_version(target, unit, &sets)?;
        if let Some(Value::Object(obelisk)) = values.get_mut(OBELISK_KEY) {
            obelisk.insert("version".to_owned(), Value::String(version));
        }
        ValuesCase::new(target, unit, values, sets)
    }
}

/// `obelisk.version` for `unit` on `target`: every value given (by the target, the unit, a config file
/// or a generator) must be the same one, and there must be one.
fn agree_on_version(
    target: &Target,
    unit: &Unit,
    sets: &FileObelisks,
) -> Result<String, ObeliskK8sError> {
    let params = InterpolationParams {
        target: Some(&target.name),
        unit: Some(&unit.name),
    };
    let mut given: Vec<(String, PathBuf)> = Vec::new();
    for (file, version) in [(&target.file, &target.version), (&unit.file, &unit.version)] {
        if let Some(version) = version {
            let value = version
                .interpolate(&params)
                .map_err(|var| ObeliskK8sError::Config {
                    file: file.clone(),
                    at: None,
                    message: format!(
                        "`version: {version}`: `${{{{obelisk:{}}}}}` has no value here",
                        var.name()
                    ),
                })?;
            given.push((value, file.clone()));
        }
    }
    given.extend(
        sets.iter()
            .filter_map(|(file, set)| Some((set.version.clone()?, file.clone()))),
    );

    let case = format!("unit `{}` on target `{}`", unit.name, target.name);
    let mut distinct: Vec<&str> = given.iter().map(|(v, _)| v.as_str()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    match distinct.as_slice() {
        [one] => Ok((*one).to_owned()),
        [] => Err(ObeliskK8sError::Usage(format!(
            "no version for {case}: set `version` in its target or unit, or `{OBELISK_KEY}.version` in a config file or generator (e.g. from `.Values.{OBELISK_KEY}.git`)"
        ))),
        _ => {
            let from: Vec<_> = given
                .iter()
                .map(|(v, f)| format!("`{v}` ({})", f.display()))
                .collect();
            Err(ObeliskK8sError::Usage(format!(
                "{case} has {} different versions, from {} places: {}; they must agree",
                distinct.len(),
                given.len(),
                from.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::REPO_CONFIG_DIR;
    use crate::config::test_support::{helm_installed, write_repo};

    fn repo(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let mut files = vec![
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./values.yaml]\n  generators:\n    - { type: gotmpl, path: ./gen.yaml.gotmpl }\n    - { type: gotmpl, path: \"./${{obelisk:target}}.gen.yaml.gotmpl\" }\n",
            ),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
            (
                "units/app/unit.yaml",
                "unit:\n  version: 1.0.0\n  generators: [{ type: gotmpl, path: ./app.gen.yaml.gotmpl }]\n",
            ),
            (
                "values.yaml",
                "db: { host: h, port: 5432 }\nmode: \"0755\"\n",
            ),
        ];
        files.extend_from_slice(extra);
        write_repo(&files)
    }

    /// The one case for unit `app` on `target`: (its YAML, what its files set under `obelisk:`).
    fn generated(
        dir: &tempfile::TempDir,
        target: &str,
    ) -> Result<(String, crate::config::values::FileObelisks), ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&[target.to_owned()], &["app".to_owned()])
            .unwrap();
        let mut report = config.generated_values_report(dir.path(), &selection, None, None)?;
        let case = report.cases.remove(0);
        Ok((case.yaml, case.sets))
    }

    #[test]
    fn a_unit_with_generators_needs_a_helm_sized_name() {
        // Fails before `helm` would run.
        let long = "a".repeat(54);
        let dir = repo(&[("gen.yaml.gotmpl", "x: 1\n")]);
        let unit = dir.path().join(REPO_CONFIG_DIR).join("units");
        std::fs::rename(unit.join("app"), unit.join(&long)).unwrap();
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["dev".to_owned()], std::slice::from_ref(&long))
            .unwrap();
        let error = config
            .generated_values_report(dir.path(), &selection, None, None)
            .unwrap_err();
        assert!(
            error.to_string().contains("at most 53 characters"),
            "{error}"
        );
    }

    #[test]
    fn merges_each_generators_output_over_the_config_values() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "gen.yaml.gotmpl",
                "obelisk:\n  release:\n    name: {{ .Values.obelisk.unit }}\n    namespace: mc-{{ .Values.obelisk.target }}\ndb:\n  url: \"postgres://{{ .Values.db.host }}:{{ .Values.db.port }}\"\nrelease: {{ .Release.Name }}/{{ .Release.Namespace }}\n",
            ),
            ("prod.gen.yaml.gotmpl", "replicas: 3\n"),
            (
                "app.gen.yaml.gotmpl",
                "{{- if .Values.replicas }}\nsaw: replicas\n{{- end }}\ndb: { port: 6543 }\n",
            ),
        ]);
        let (yaml, releases) = generated(&dir, "prod").unwrap();
        // Every generator sees the config values, not the others' output: `app.gen` didn't see `replicas`.
        assert_eq!(
            yaml,
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  ephemeral: null\n  git: null\n  version: \"1.0.0\"\n\
             db:\n  host: h\n  port: 6543\n  url: postgres://h:5432\n\
             mode: \"0755\"\nrelease: app/default\nreplicas: 3\n"
        );
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].0, Path::new(".obelisk-k8s/gen.yaml.gotmpl"));
        assert_eq!(releases[0].1.namespace.as_deref(), Some("mc-prod"));

        // `dev` has no `dev.gen.yaml.gotmpl`: skipped.
        assert!(!generated(&dir, "dev").unwrap().0.contains("replicas"));
    }

    #[test]
    fn a_generator_can_output_nothing() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[(
            "gen.yaml.gotmpl",
            "{{- if false }}\nx: 1\n{{- end }}\n# just a comment\n",
        )]);
        let (yaml, _) = generated(&dir, "dev").unwrap();
        assert!(yaml.ends_with("mode: \"0755\"\n"), "{yaml}");
    }

    #[test]
    fn reports_bad_generators() {
        if !helm_installed() {
            return;
        }
        let error = |text: &str| {
            generated(&repo(&[("gen.yaml.gotmpl", text)]), "dev")
                .unwrap_err()
                .to_string()
        };
        let file = ".obelisk-k8s/gen.yaml.gotmpl";

        // Only what obelisk-k8s controls: Helm's own wording varies by version.
        let e = error("x: {{ .Values.nope.deeper }}\n");
        assert!(
            e.starts_with(&format!("{file}: helm couldn't render it: ")),
            "{e}"
        );
        assert!(
            e.matches(file).count() >= 2,
            "Helm's message names the file too: {e}"
        );
        assert!(!e.contains("obelisk-render/templates"), "{e}");
        assert!(!e.contains("--debug"), "{e}");

        assert_eq!(
            error("a: 1\n---\nb: 2\n"),
            format!("{file}: a generator must output one mapping, not 2 documents")
        );
        // Helm itself only accepts mappings as documents, so a list fails there.
        let e = error("- a\n");
        assert!(
            e.starts_with(&format!("{file}: helm couldn't render it: ")),
            "{e}"
        );

        assert_eq!(
            error("obelisk:\n  target: x\n"),
            format!(
                "{file}: `obelisk:` is reserved; a generator's output may only set `obelisk.release.name`, `obelisk.release.namespace` and `obelisk.version`, not `obelisk.target`"
            )
        );
    }

    /// `units/app` on targets `prod` / `dev`, with no generators (so no `helm`), and `files` on top.
    fn plain(files: &[(&str, &str)]) -> tempfile::TempDir {
        let mut all = vec![
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./values.yaml, \"./${{obelisk:target}}.yaml\"]\n",
            ),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit: {}\n"),
        ];
        all.retain(|(p, _)| !files.iter().any(|(q, _)| q == p));
        all.extend_from_slice(files);
        write_repo(&all)
    }

    fn version(dir: &tempfile::TempDir, target: &str) -> Result<String, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&[target.to_owned()], &["app".to_owned()])
            .unwrap();
        let mut report = config.generated_values_report(dir.path(), &selection, None, None)?;
        let case = report.cases.remove(0);
        Ok(case.values[OBELISK_KEY]["version"]
            .as_str()
            .unwrap()
            .to_owned())
    }

    #[test]
    fn agrees_on_one_version() {
        // From the unit, interpolated.
        let dir = plain(&[(
            "units/app/unit.yaml",
            "unit:\n  version: \"0.0.0-${{obelisk:unit}}\"\n",
        )]);
        assert_eq!(version(&dir, "prod").unwrap(), "0.0.0-app");
        // The same value from the target and a config file is one version.
        let dir = plain(&[
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  version: v2.0.0\n",
            ),
            ("prod.yaml", "obelisk:\n  version: v2.0.0\n"),
        ]);
        assert_eq!(version(&dir, "prod").unwrap(), "v2.0.0");
        // A config file's version isn't merged in as a value; only the agreed one is.
        let dir = plain(&[("values.yaml", "obelisk:\n  version: \"1.0\"\nx: 1\n")]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["dev".to_owned()], &["app".to_owned()])
            .unwrap();
        let report = config
            .generated_values_report(dir.path(), &selection, None, None)
            .unwrap();
        assert!(
            report.cases[0].yaml.contains("  version: \"1.0\"\nx: 1\n"),
            "{}",
            report.cases[0].yaml
        );
    }

    #[test]
    fn needs_exactly_one_version() {
        let error = version(&plain(&[]), "prod").unwrap_err().to_string();
        assert_eq!(
            error,
            "no version for unit `app` on target `prod`: set `version` in its target or unit, or `obelisk.version` in a config file or generator (e.g. from `.Values.obelisk.git`)"
        );
        let dir = plain(&[
            ("units/app/unit.yaml", "unit:\n  version: v1\n"),
            ("values.yaml", "obelisk:\n  version: v1\n"),
            ("prod.yaml", "obelisk:\n  version: v2\n"),
        ]);
        assert_eq!(
            version(&dir, "prod").unwrap_err().to_string(),
            "unit `app` on target `prod` has 2 different versions, from 3 places: `v1` (.obelisk-k8s/units/app/unit.yaml), `v1` (.obelisk-k8s/values.yaml), `v2` (.obelisk-k8s/prod.yaml); they must agree"
        );
        assert_eq!(version(&dir, "dev").unwrap(), "v1", "dev has no prod.yaml");
    }

    #[test]
    fn a_generator_sees_the_ephemeral_and_its_values() {
        use crate::config::ephemeral::{Ephemeral, EphemeralValues};
        if !helm_installed() {
            return;
        }
        let dir = plain(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  generators: [{ type: gotmpl, path: ./gen.gotmpl }]\n",
            ),
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  ephemeral: true\n",
            ),
            (
                "gen.gotmpl",
                "obelisk:\n  version: {{ printf \"0.0.0-%s\" .Values.obelisk.ephemeral | quote }}\nseen: {{ .Values.from | quote }}\n",
            ),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".to_owned()], &[]).unwrap();
        let ephemeral =
            Ephemeral::new("pr-7".into(), EphemeralValues::Inline("from: cli".into())).unwrap();
        let report = config
            .generated_values_report(dir.path(), &selection, None, Some(&ephemeral))
            .unwrap();
        assert_eq!(report.cases[0].values[OBELISK_KEY]["version"], "0.0.0-pr-7");
        assert_eq!(report.cases[0].values["seen"], "cli");
    }

    #[test]
    fn a_version_conflict_names_the_ephemeral_values() {
        use crate::config::ephemeral::{Ephemeral, EphemeralValues};
        let dir = plain(&[(
            "targets/prod/target.yaml",
            "target:\n  cluster: c\n  ephemeral: true\n  version: v1\n",
        )]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".to_owned()], &[]).unwrap();
        let ephemeral = Ephemeral::new(
            "pr-7".into(),
            EphemeralValues::Inline("obelisk: { version: v2 }".into()),
        )
        .unwrap();
        assert_eq!(
            config
                .generated_values_report(dir.path(), &selection, None, Some(&ephemeral))
                .unwrap_err()
                .to_string(),
            "unit `app` on target `prod` has 2 different versions, from 2 places: `v1` (.obelisk-k8s/targets/prod/target.yaml), `v2` (--ephemeral-values); they must agree"
        );
    }

    #[test]
    fn a_generator_can_set_the_version_from_git() {
        if !helm_installed() {
            return;
        }
        let dir = plain(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  generators: [{ type: gotmpl, path: ./gen.gotmpl }]\n",
            ),
            (
                "gen.gotmpl",
                "obelisk:\n  version: {{ printf \"0.0.0-%s\" (.Values.obelisk.git | default \"nogit\") | quote }}\n",
            ),
        ]);
        // A temp dir isn't a git repo: `obelisk.git` is null.
        assert_eq!(version(&dir, "prod").unwrap(), "0.0.0-nogit");
    }

    #[test]
    fn a_generator_can_set_the_version_from_a_real_sha() {
        if !helm_installed() {
            return;
        }
        let dir = plain(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  generators: [{ type: gotmpl, path: ./gen.gotmpl }]\n",
            ),
            (
                "gen.gotmpl",
                "obelisk:\n  version: {{ printf \"0.0.0-%s\" .Values.obelisk.git.sha.short | quote }}\n",
            ),
        ]);
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "one"]);
        let short = git(&["rev-parse", "--short", "HEAD"]);
        assert_eq!(version(&dir, "prod").unwrap(), format!("0.0.0-{short}"));
    }

    #[test]
    fn a_unit_and_a_generator_must_agree_on_the_version() {
        if !helm_installed() {
            return;
        }
        let dir = plain(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  generators: [{ type: gotmpl, path: ./gen.gotmpl }]\n",
            ),
            ("units/app/unit.yaml", "unit:\n  version: 1.2.3\n"),
            ("gen.gotmpl", "obelisk:\n  version: \"0.0.0-dev\"\n"),
        ]);
        let e = version(&dir, "prod").unwrap_err().to_string();
        assert!(
            e.contains(
                "`1.2.3` (.obelisk-k8s/units/app/unit.yaml), `0.0.0-dev` (.obelisk-k8s/gen.gotmpl)"
            ),
            "{e}"
        );
    }
}
