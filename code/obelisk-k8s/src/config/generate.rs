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
//! `obelisk.release.{name, namespace}` under `obelisk:` (kept on [`ValuesCase::releases`], not in the
//! values).

use std::path::Path;

use crate::ObeliskK8sError;
use crate::config::paths::Selection;
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::values::{ValuesCase, ValuesReport, absorb, shown_under, values_yaml};
use crate::{helm, yaml};

/// Helm's limit on release names.
const HELM_RELEASE_NAME_MAX: usize = 53;

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
    ) -> Result<ValuesReport<'a>, ObeliskK8sError> {
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| self.generated_for(repo_root, shown_root, target, unit))
            .collect::<Result<_, _>>()?;
        Ok(ValuesReport {
            cases,
            single: selection.single,
        })
    }

    fn generated_for<'a>(
        &'a self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
    ) -> Result<ValuesCase<'a>, ObeliskK8sError> {
        let (mut values, mut releases) = self.config_values(repo_root, shown_root, target, unit)?;
        let input = values_yaml(&values, target, unit)?;
        let generators = self.resolve_generators(repo_root, target, unit)?;
        if generators.iter().any(|p| p.found) && unit.name.len() > HELM_RELEASE_NAME_MAX {
            return Err(ObeliskK8sError::Usage(format!(
                "unit `{}` has generators, which render as a Helm release named after it, but Helm release names are at most {HELM_RELEASE_NAME_MAX} characters",
                unit.name
            )));
        }
        for path in generators.iter().filter(|p| p.found) {
            let shown = shown_under(shown_root, &path.path);
            let rendered = helm::render(
                &repo_root.join(&path.path),
                &shown,
                &input,
                &unit.name,
                GENERATOR_NAMESPACE,
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
                    &mut releases,
                    output,
                    &path.path,
                    &shown,
                    "a generator's output",
                )?;
            }
        }
        ValuesCase::new(target, unit, values, releases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::REPO_CONFIG_DIR;

    /// These run `helm`. Without it they fail, unless `OBELISK_TEST_SKIP_HELM=1` says to skip them.
    fn helm_installed() -> bool {
        if std::process::Command::new("helm")
            .arg("version")
            .output()
            .is_ok()
        {
            return true;
        }
        assert!(
            std::env::var_os("OBELISK_TEST_SKIP_HELM").is_some_and(|v| v == "1"),
            "these tests need `helm` on the PATH (or OBELISK_TEST_SKIP_HELM=1 to skip them)"
        );
        false
    }

    fn repo(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut files = vec![
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./values.yaml]\n  generators:\n    - { type: gotmpl, path: ./gen.yaml.gotmpl }\n    - { type: gotmpl, path: \"./${{obelisk:target}}.gen.yaml.gotmpl\" }\n",
            ),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
            (
                "units/app/unit.yaml",
                "unit:\n  generators: [{ type: gotmpl, path: ./app.gen.yaml.gotmpl }]\n",
            ),
            (
                "values.yaml",
                "db: { host: h, port: 5432 }\nmode: \"0755\"\n",
            ),
        ];
        files.extend_from_slice(extra);
        for (path, text) in files {
            let path = dir.path().join(REPO_CONFIG_DIR).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// The one case for unit `app` on `target`: (its YAML, what its files set under `obelisk.release`).
    fn generated(
        dir: &tempfile::TempDir,
        target: &str,
    ) -> Result<(String, crate::config::values::Releases), ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&[target.to_owned()], &["app".to_owned()])
            .unwrap();
        let mut report = config.generated_values_report(dir.path(), &selection, None)?;
        let case = report.cases.remove(0);
        Ok((case.yaml, case.releases))
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
            .generated_values_report(dir.path(), &selection, None)
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
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n\
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
                "{file}: `obelisk:` is reserved; a generator's output may only set `obelisk.release.name` and `obelisk.release.namespace`, not `obelisk.target`"
            )
        );
    }
}
