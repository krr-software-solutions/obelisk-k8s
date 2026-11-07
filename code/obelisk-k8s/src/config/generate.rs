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
//! `obelisk.release.{name, namespace, version}` under `obelisk:` (kept on [`ValuesCase::sets`], not in
//! the values).
//!
//! After the generators, the case must agree on one release name, one release namespace and one
//! version: every value given (its target's and unit's `release.{name, namespace, version}`, and
//! `obelisk.release.{name, namespace, version}` in its config files and generators) must be the
//! same, and there must be one of each. They're added to the values as
//! `obelisk.release.{name, namespace, version}`; templates get the release as
//! `.Release.{Name, Namespace}`. A release name is a valid name of at most 53 characters (Helm's
//! limit), a namespace one of at most 63.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ObeliskK8sError;
use crate::config::ephemeral::Ephemeral;
use crate::config::global::GlobalConfig;
use crate::config::paths::Selection;
use crate::config::paths::Source;
use crate::config::render;
use crate::config::repo::{Release, RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::config::values::{
    Agreed, FileObelisk, FileObelisks, OBELISK_KEY, ValuesCase, ValuesReport, absorb, values_yaml,
};
use crate::config::{NAME_RULE, is_valid_name};
use crate::interp::{InterpString, InterpolationParams};
use crate::ssm::{self, Ssm};
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
    /// something under `obelisk:` other than `release.{name, namespace, version}`;
    /// [`ObeliskK8sError::Usage`] if a case has no version, release name or release namespace, more
    /// than one of any, or a release that isn't a valid name (or too long a release name).
    pub fn generated_values_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        ssm: Option<&Ssm>,
    ) -> Result<ValuesReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global, ssm)?;
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| {
                self.generated_for(repo_root, shown_root, target, unit, &inputs)
                    .map(|(case, _)| case)
            })
            .collect::<Result<_, _>>()?;
        Ok(ValuesReport {
            cases,
            single: selection.single,
        })
    }

    /// One case's generated values, and what it agreed on.
    pub(crate) fn generated_for<'a>(
        &'a self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<(ValuesCase<'a>, Agreed), ObeliskK8sError> {
        let (mut values, mut sets) =
            self.config_values(repo_root, shown_root, target, unit, inputs)?;
        // Every generator sees the config values (`input`), not the others' outputs.
        let input = values_yaml(&values, target, unit)?;
        let generators = self.resolve_generators(repo_root, target, unit)?;
        if generators
            .iter()
            .any(|g| g.present() && matches!(g.source, Source::Gotmpl { .. }))
        {
            helm::check_generator_release(&unit.name)?;
        }
        // Only a chart reads the values as a tree, and a chart can't be a generator.
        let no_tree = Map::new();
        let render_inputs = render::Inputs {
            repo_root,
            values: &no_tree,
            values_yaml: &input,
            release: &unit.name,
            namespace: GENERATOR_NAMESPACE,
            chart_version: None,
            capabilities: None,
        };
        for generator in generators.iter().filter(|g| g.present()) {
            let shown = generator.shown(shown_root);
            let rendered = render::text(generator, &shown, &render_inputs)?;
            let mut documents = yaml::parse_values_stream(&rendered, &shown)?;
            // A `file` / `url` generator's values are written in the repo (or pinned), like a config
            // file's: their refs resolve. The others' output is never looked at.
            if let Source::Url { sha256, .. } = &generator.source {
                ssm::check_pinned(&rendered, *sha256, &shown)?;
            }
            if matches!(generator.source, Source::File { .. } | Source::Url { .. }) {
                let what = shown.display().to_string();
                ssm::resolve_or_check(inputs.ssm, documents.iter_mut().map(|d| (what.clone(), d)))?;
            }
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
                    &generator.describe(),
                    &shown,
                    "a generator's output",
                )?;
            }
        }
        let agreed = Agreed {
            version: agree(Field::Version, target, unit, &sets)?,
            release_name: agree(Field::ReleaseName, target, unit, &sets)?,
            namespace: agree(Field::ReleaseNamespace, target, unit, &sets)?,
        };
        check_release(&agreed, target, unit)?;
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
        let case = ValuesCase::new(target, unit, values, sets, Some(agreed.clone()))?;
        Ok((case, agreed))
    }
}

/// One of the things a case agrees on ([`Agreed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// `obelisk.release.version`: the chart's `version` and `appVersion`.
    Version,
    /// `obelisk.release.name`: `.Release.Name`, and the Helm release.
    ReleaseName,
    /// `obelisk.release.namespace`: `.Release.Namespace`, and where the release goes.
    ReleaseNamespace,
}

impl Field {
    /// e.g. "release name".
    fn what(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::ReleaseName => "release name",
            Self::ReleaseNamespace => "release namespace",
        }
    }

    /// Its key in a target or unit, e.g. `release.name`.
    fn key(self) -> &'static str {
        match self {
            Self::Version => "release.version",
            Self::ReleaseName => "release.name",
            Self::ReleaseNamespace => "release.namespace",
        }
    }

    /// What a target or unit says.
    fn in_target_or_unit(self, release: &Release) -> Option<&InterpString> {
        match self {
            Self::Version => release.version.as_ref(),
            Self::ReleaseName => release.name.as_ref(),
            Self::ReleaseNamespace => release.namespace.as_ref(),
        }
    }

    /// What a config file or generator set.
    fn in_file(self, set: &FileObelisk) -> Option<&String> {
        match self {
            Self::Version => set.version.as_ref(),
            Self::ReleaseName => set.name.as_ref(),
            Self::ReleaseNamespace => set.namespace.as_ref(),
        }
    }

    /// How to give it, for the error when nothing does.
    fn hint(self) -> &'static str {
        match self {
            Self::Version => " (e.g. from `.Values.obelisk.git`)",
            Self::ReleaseName => "",
            Self::ReleaseNamespace => {
                " (e.g. from `.Values.obelisk.ephemeral`, on an ephemeral target)"
            }
        }
    }
}

/// The agreed `what` for `unit` on `target`: every value given (by the target, the unit, a config
/// file or a generator) must be the same one, and there must be one.
fn agree(
    what: Field,
    target: &Target,
    unit: &Unit,
    sets: &FileObelisks,
) -> Result<String, ObeliskK8sError> {
    let params = InterpolationParams {
        target: Some(&target.name),
        unit: Some(&unit.name),
    };
    let mut given: Vec<(String, PathBuf)> = Vec::new();
    for (file, release) in [(&target.file, &target.release), (&unit.file, &unit.release)] {
        if let Some(raw) = what.in_target_or_unit(release) {
            let value = raw
                .interpolate(&params)
                .map_err(|var| ObeliskK8sError::Config {
                    file: file.clone(),
                    at: None,
                    message: format!(
                        "`{}: {raw}`: `${{{{obelisk:{}}}}}` has no value here",
                        what.key(),
                        var.name()
                    ),
                })?;
            given.push((value, file.clone()));
        }
    }
    given.extend(
        sets.iter()
            .filter_map(|(file, set)| Some((what.in_file(set)?.clone(), file.clone()))),
    );

    let case = format!("unit `{}` on target `{}`", unit.name, target.name);
    let mut distinct: Vec<&str> = given.iter().map(|(v, _)| v.as_str()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    match distinct.as_slice() {
        [one] => Ok((*one).to_owned()),
        [] => Err(ObeliskK8sError::Usage(format!(
            "no {} for {case}: set `{}` in its target or unit, or `{OBELISK_KEY}.{}` in a config file or generator{}",
            what.what(),
            what.key(),
            what.key(),
            what.hint()
        ))),
        _ => {
            let from: Vec<_> = given
                .iter()
                .map(|(v, f)| format!("`{v}` ({})", f.display()))
                .collect();
            Err(ObeliskK8sError::Usage(format!(
                "{case} has {} different {}s, from {} places: {}; they must agree",
                distinct.len(),
                what.what(),
                given.len(),
                from.join(", ")
            )))
        }
    }
}

/// Helm's and Kubernetes' rules for the agreed release.
fn check_release(agreed: &Agreed, target: &Target, unit: &Unit) -> Result<(), ObeliskK8sError> {
    let case = format!("unit `{}` on target `{}`", unit.name, target.name);
    for (what, value) in [
        ("release name", &agreed.release_name),
        ("release namespace", &agreed.namespace),
    ] {
        if !is_valid_name(value) {
            return Err(ObeliskK8sError::Usage(format!(
                "{what} `{value}` for {case} isn't a valid name ({NAME_RULE})"
            )));
        }
    }
    if !helm::fits_release_name(&agreed.release_name) {
        return Err(ObeliskK8sError::Usage(format!(
            "release name `{}` for {case} is too long: Helm release names are at most {} characters",
            agreed.release_name,
            helm::RELEASE_NAME_MAX
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::REPO_CONFIG_DIR;
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo};

    fn repo(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let mut files = vec![
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./values.yaml]\n  generators:\n    - { type: gotmpl, path: ./gen.yaml.gotmpl }\n    - { type: gotmpl, path: \"./${{obelisk:target}}.gen.yaml.gotmpl\" }\n",
            ),
            ("targets/prod/target.yaml", TARGET),
            ("targets/dev/target.yaml", TARGET),
            (
                "units/app/unit.yaml",
                "unit:\n  release: { version: 1.0.0 }\n  generators: [{ type: gotmpl, path: ./app.gen.yaml.gotmpl }]\n",
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
        let mut report =
            config.generated_values_report(dir.path(), &selection, None, None, &global(), None)?;
        let case = report.cases.remove(0);
        let yaml = case
            .yaml
            .replace(&dir.path().display().to_string(), "<repo>");
        Ok((yaml, case.sets))
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
            .generated_values_report(dir.path(), &selection, None, None, &global(), None)
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
            // Targets with no release: only the generator gives one.
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
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
            "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    globalRoot: /home/me/.obelisk-k8s\n    repoRoot: <repo>\n\
             \x20 release:\n    name: app\n    namespace: mc-prod\n    version: \"1.0.0\"\n\
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
    fn file_and_command_generators_merge_like_any_other() {
        // Neither runs `helm`.
        let dir = plain(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./values.yaml]\n  generators:\n    - { type: file, path: ./more.yaml }\n    - { type: command, command: [sh, -c, \"printf 'seen: %s\\\\n' $(grep -c 'unit: app')\"] }\n",
            ),
            ("values.yaml", "a: 1\n"),
            ("more.yaml", "b: 2\nobelisk:\n  release: { version: v9 }\n"),
        ]);
        let (yaml, _) = generated(&dir, "prod").unwrap();
        assert!(
            yaml.contains("    version: v9\na: 1\nb: 2\nseen: 1\n"),
            "{yaml}"
        );
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
                "{file}: `obelisk:` is reserved; a generator's output may only set `obelisk.release.name`, `obelisk.release.namespace` and `obelisk.release.version`, not `obelisk.target`"
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
            ("targets/prod/target.yaml", TARGET),
            ("targets/dev/target.yaml", TARGET),
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
        let mut report =
            config.generated_values_report(dir.path(), &selection, None, None, &global(), None)?;
        let case = report.cases.remove(0);
        Ok(case.values[OBELISK_KEY]["release"]["version"]
            .as_str()
            .unwrap()
            .to_owned())
    }

    #[test]
    fn agrees_on_one_version() {
        // From the unit, interpolated.
        let dir = plain(&[(
            "units/app/unit.yaml",
            "unit:\n  release: { version: \"0.0.0-${{obelisk:unit}}\" }\n",
        )]);
        assert_eq!(version(&dir, "prod").unwrap(), "0.0.0-app");
        // The same value from the target and a config file is one version.
        let dir = plain(&[
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\", version: v2.0.0 }\n",
            ),
            ("prod.yaml", "obelisk:\n  release: { version: v2.0.0 }\n"),
        ]);
        assert_eq!(version(&dir, "prod").unwrap(), "v2.0.0");
        // A config file's version isn't merged in as a value; only the agreed one is.
        let dir = plain(&[(
            "values.yaml",
            "obelisk:\n  release: { version: \"1.0\" }\nx: 1\n",
        )]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["dev".to_owned()], &["app".to_owned()])
            .unwrap();
        let report = config
            .generated_values_report(dir.path(), &selection, None, None, &global(), None)
            .unwrap();
        assert!(
            report.cases[0]
                .yaml
                .contains("    version: \"1.0\"\nx: 1\n"),
            "{}",
            report.cases[0].yaml
        );
    }

    #[test]
    fn needs_exactly_one_version() {
        let error = version(&plain(&[]), "prod").unwrap_err().to_string();
        assert_eq!(
            error,
            "no version for unit `app` on target `prod`: set `release.version` in its target or unit, or `obelisk.release.version` in a config file or generator (e.g. from `.Values.obelisk.git`)"
        );
        let dir = plain(&[
            ("units/app/unit.yaml", "unit:\n  release: { version: v1 }\n"),
            ("values.yaml", "obelisk:\n  release: { version: v1 }\n"),
            ("prod.yaml", "obelisk:\n  release: { version: v2 }\n"),
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
                "target:\n  cluster: c\n  ephemeral: true\n  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\" }\n",
            ),
            (
                "gen.gotmpl",
                "obelisk:\n  release: { version: {{ printf \"0.0.0-%s\" .Values.obelisk.ephemeral | quote }} }\nseen: {{ .Values.from | quote }}\n",
            ),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".to_owned()], &[]).unwrap();
        let ephemeral =
            Ephemeral::new("pr-7".into(), EphemeralValues::Inline("from: cli".into())).unwrap();
        let report = config
            .generated_values_report(
                dir.path(),
                &selection,
                None,
                Some(&ephemeral),
                &global(),
                None,
            )
            .unwrap();
        assert_eq!(
            report.cases[0].values[OBELISK_KEY]["release"]["version"],
            "0.0.0-pr-7"
        );
        assert_eq!(report.cases[0].values["seen"], "cli");
    }

    #[test]
    fn a_version_conflict_names_the_ephemeral_values() {
        use crate::config::ephemeral::{Ephemeral, EphemeralValues};
        let dir = plain(&[(
            "targets/prod/target.yaml",
            "target:\n  cluster: c\n  ephemeral: true\n  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\", version: v1 }\n",
        )]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".to_owned()], &[]).unwrap();
        let ephemeral = Ephemeral::new(
            "pr-7".into(),
            EphemeralValues::Inline("obelisk: { release: { version: v2 } }".into()),
        )
        .unwrap();
        assert_eq!(
            config
                .generated_values_report(
                    dir.path(),
                    &selection,
                    None,
                    Some(&ephemeral),
                    &global(),
                    None
                )
                .unwrap_err()
                .to_string(),
            "unit `app` on target `prod` has 2 different versions, from 2 places: `v1` (.obelisk-k8s/targets/prod/target.yaml), `v2` (--ephemeral-values); they must agree"
        );
    }

    fn release(dir: &tempfile::TempDir, target: &str) -> Result<Value, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&[target.to_owned()], &["app".to_owned()])
            .unwrap();
        let mut report =
            config.generated_values_report(dir.path(), &selection, None, None, &global(), None)?;
        Ok(report.cases.remove(0).values[OBELISK_KEY]["release"].clone())
    }

    #[test]
    fn agrees_on_one_release() {
        let version = ("units/app/unit.yaml", "unit:\n  release: { version: v1 }\n");
        // From the target (interpolated), and a config file saying the same.
        let dir = plain(&[
            version,
            ("prod.yaml", "obelisk:\n  release: { namespace: prod }\n"),
        ]);
        assert_eq!(
            release(&dir, "prod").unwrap(),
            serde_json::json!({ "name": "app", "namespace": "prod", "version": "v1" })
        );
        // Each is required.
        let dir = plain(&[
            version,
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release: { name: app }\n",
            ),
        ]);
        assert_eq!(
            release(&dir, "prod").unwrap_err().to_string(),
            "no release namespace for unit `app` on target `prod`: set `release.namespace` in its target or unit, or `obelisk.release.namespace` in a config file or generator (e.g. from `.Values.obelisk.ephemeral`, on an ephemeral target)"
        );
        let dir = plain(&[
            version,
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release: { namespace: n }\n",
            ),
        ]);
        assert!(
            release(&dir, "prod")
                .unwrap_err()
                .to_string()
                .starts_with("no release name for unit `app` on target `prod`"),
        );
        // And they must agree.
        let dir = plain(&[
            version,
            ("prod.yaml", "obelisk:\n  release: { name: other }\n"),
        ]);
        assert_eq!(
            release(&dir, "prod").unwrap_err().to_string(),
            "unit `app` on target `prod` has 2 different release names, from 2 places: `app` (.obelisk-k8s/targets/prod/target.yaml), `other` (.obelisk-k8s/prod.yaml); they must agree"
        );
    }

    #[test]
    fn a_unit_can_give_its_release_interpolated() {
        let dir = plain(&[
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release: { namespace: n }\n",
            ),
            (
                "units/app/unit.yaml",
                "unit:\n  release: { name: \"${{obelisk:unit}}-${{obelisk:target}}\", version: v1 }\n",
            ),
        ]);
        assert_eq!(
            release(&dir, "prod").unwrap(),
            serde_json::json!({ "name": "app-prod", "namespace": "n", "version": "v1" })
        );
    }

    #[test]
    fn a_namespace_conflict_names_every_place() {
        let dir = plain(&[
            (
                "units/app/unit.yaml",
                "unit:\n  release: { namespace: a, version: v1 }\n",
            ),
            ("values.yaml", "obelisk:\n  release: { namespace: b }\n"),
        ]);
        assert_eq!(
            release(&dir, "prod").unwrap_err().to_string(),
            "unit `app` on target `prod` has 3 different release namespaces, from 3 places: `prod` (.obelisk-k8s/targets/prod/target.yaml), `a` (.obelisk-k8s/units/app/unit.yaml), `b` (.obelisk-k8s/values.yaml); they must agree"
        );
    }

    #[test]
    fn the_release_must_be_valid_for_helm_and_kubernetes() {
        let version = ("units/app/unit.yaml", "unit:\n  release: { version: v1 }\n");
        let target = |release: &str| format!("target:\n  cluster: c\n  release: {release}\n");
        let bad_namespace = target("{ name: app, namespace: Not_Valid }");
        let dir = plain(&[version, ("targets/prod/target.yaml", &bad_namespace)]);
        let error = release(&dir, "prod").unwrap_err().to_string();
        assert!(
            error.starts_with(
                "release namespace `Not_Valid` for unit `app` on target `prod` isn't a valid name"
            ),
            "{error}"
        );
        let bad_name = target("{ name: -app, namespace: n }");
        let dir = plain(&[version, ("targets/prod/target.yaml", &bad_name)]);
        let error = release(&dir, "prod").unwrap_err().to_string();
        assert!(
            error.starts_with(
                "release name `-app` for unit `app` on target `prod` isn't a valid name"
            ),
            "{error}"
        );
        let long = target(&format!("{{ name: {}, namespace: n }}", "a".repeat(54)));
        let dir = plain(&[version, ("targets/prod/target.yaml", &long)]);
        let error = release(&dir, "prod").unwrap_err().to_string();
        assert!(
            error.contains("is too long: Helm release names are at most 53"),
            "{error}"
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
                "obelisk:\n  release:\n    version: {{ printf \"0.0.0-%s\" (.Values.obelisk.git | default \"nogit\") | quote }}\n",
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
                "obelisk:\n  release:\n    version: {{ printf \"0.0.0-%s\" .Values.obelisk.git.sha.short | quote }}\n",
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
            (
                "units/app/unit.yaml",
                "unit:\n  release: { version: 1.2.3 }\n",
            ),
            (
                "gen.gotmpl",
                "obelisk:\n  release: { version: \"0.0.0-dev\" }\n",
            ),
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
