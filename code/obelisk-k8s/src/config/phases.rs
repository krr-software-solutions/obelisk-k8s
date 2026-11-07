//! Running a case's build plan (see `config::build`): its phases' values, then the release it agrees
//! on (`template values`). Its manifests are `config::manifests`.
//!
//! The values go phase by phase. V₀ is the `obelisk:` block (and, on an ephemeral target, the
//! ephemeral values merged over it). Everything in phase k (its values sources, and its manifests)
//! renders against V₍ₖ₋₁₎, the values after the phases before it: its sources don't see each other.
//! Phase k's values outputs are then deep-merged over V₍ₖ₋₁₎, in source order, to make Vₖ (and the
//! ephemeral values merged over that again, so they always win). So values made in a phase are only
//! seen by the phases after it.
//!
//! A values source renders on its own (see `config::render` for the types): a Go template
//! (`gotmpl`) through a throwaway Helm chart, with the values as `.Values` and `.Release` as
//! `obelisk-build` / `default`, as for the build file (the real release isn't agreed on yet: use
//! `.Values.obelisk`; `.Template.Name` is the throwaway chart's, not the file's). Each is its own `helm` run. Its output must be one mapping, or
//! nothing; like the ephemeral values, it may only set `obelisk.release.{name, namespace, version}`
//! under `obelisk:` (kept on [`ValuesCase::sets`], not in the values). `${{obelisk:aws-ssm:<key>}}`
//! refs in each output (an `inline` one's too) are resolved before it merges (`apply`, `template
//! --resolve`), so everything after it sees the values; else they're only checked, and left as
//! written.
//!
//! After every phase's values, the case must agree on one release name, one release namespace and
//! one version: every value given (its target's and unit's `release.{name, namespace, version}`, and
//! `obelisk.release.{name, namespace, version}` from its values sources and ephemeral values) must be
//! the same, and there must be one of each. They're added to the values as
//! `obelisk.release.{name, namespace, version}`: a case's values (`template values`) are Vₙ with
//! them. A release name is a valid name of at most 53 characters (Helm's limit), a namespace one of
//! at most 63.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ObeliskK8sError;
use crate::config::build::{BuildPlan, RENDER_NAMESPACE, RENDER_RELEASE, Source, Warnings};
use crate::config::ephemeral::Ephemeral;
use crate::config::global::GlobalConfig;
use crate::config::paths::Selection;
use crate::config::render;
use crate::config::repo::{Release, RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::config::values::{
    Agreed, FileObelisk, FileObelisks, OBELISK_KEY, ValuesCase, ValuesReport, absorb, merge,
    values_yaml, with_release,
};
use crate::config::{NAME_RULE, is_valid_name};
use crate::interp::{InterpString, InterpolationParams};
use crate::ssm::{self, Ssm};
use crate::{helm, yaml};

/// A case's values phases, run. The values are without the release.
#[derive(Debug)]
pub(crate) struct Evaluated {
    /// Per phase, what its manifests render against (V₍ₖ₋₁₎ for phase k): kept only for a phase with
    /// manifests (`None` for the others: nothing reads it).
    pub(crate) before: Vec<Option<Map<String, Value>>>,
    /// Vₙ: the values after every phase.
    pub(crate) values: Map<String, Value>,
    pub(crate) sets: FileObelisks,
    pub(crate) agreed: Agreed,
}

impl RepoConfig {
    /// The values of every case in `selection`, after their phases, with the release they agreed on.
    /// `shown_root` is where the repo is, as the user should see it, when that isn't the current
    /// directory (paths in errors are shown under it). `warn` hears each build plan's warnings as
    /// it's built, once each.
    ///
    /// # Errors
    ///
    /// As [`RunInputs::new`] and rendering the build file; [`ObeliskK8sError::Helm`] if a values
    /// source doesn't render; [`ObeliskK8sError::Config`] for an output that isn't one mapping, or
    /// sets something under `obelisk:` other than `release.{name, namespace, version}`;
    /// [`ObeliskK8sError::Usage`] if a case has no version, release name or release namespace, more
    /// than one of any, or a release that isn't a valid name (or too long a release name).
    #[allow(clippy::too_many_arguments)]
    pub fn values_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        ssm: Option<&Ssm>,
        warn: impl FnMut(&str),
    ) -> Result<ValuesReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global, ssm)?;
        let mut cases = Vec::new();
        let mut warnings = Warnings::new(warn);
        for (target, unit) in &selection.cases {
            let obelisk = self.obelisk_values(target, unit, &inputs)?;
            let plan = self.build_plan(repo_root, shown_root, target, unit, &obelisk)?;
            warnings.add(&plan.warnings);
            cases.push(Self::values_case(
                repo_root, shown_root, target, unit, &inputs, &plan, obelisk,
            )?);
        }
        Ok(ValuesReport {
            cases,
            single: selection.single,
        })
    }

    /// One case's values after `plan`'s phases, with its agreed release. `obelisk` is as for
    /// [`RepoConfig::run_values`].
    pub(crate) fn values_case<'a>(
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
        plan: &BuildPlan,
        obelisk: Map<String, Value>,
    ) -> Result<ValuesCase<'a>, ObeliskK8sError> {
        let Evaluated {
            values,
            sets,
            agreed,
            ..
        } = Self::run_values(repo_root, shown_root, target, unit, inputs, plan, obelisk)?;
        ValuesCase::new(target, unit, with_release(values, &agreed), sets, agreed)
    }

    /// Runs `plan`'s values phases for `unit` on `target`, starting from `obelisk` (the case's
    /// [`RepoConfig::obelisk_values`]), and agrees on the release.
    pub(crate) fn run_values(
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &Target,
        unit: &Unit,
        inputs: &RunInputs<'_>,
        plan: &BuildPlan,
        obelisk: Map<String, Value>,
    ) -> Result<Evaluated, ObeliskK8sError> {
        let mut sets = Vec::new();
        let mut values = obelisk;
        // Merged over V₀ (where what it sets is recorded), and over every phase's values after it.
        let mut ephemeral_layer = None;
        if let Some(ephemeral) = inputs.ephemeral_for(target) {
            let mut layer = ephemeral.values().clone();
            resolve_refs(inputs.ssm, &mut layer, ephemeral.shown())?;
            absorb(
                &mut values,
                &mut sets,
                layer.clone(),
                ephemeral.shown(),
                ephemeral.shown(),
                "the ephemeral values",
            )?;
            if let Value::Object(mut layer) = layer {
                layer.remove(OBELISK_KEY);
                ephemeral_layer = Some(layer);
            }
        }

        let mut before = Vec::with_capacity(plan.phases.len());
        for phase in &plan.phases {
            // Its sources render against the values before it, so they don't see each other.
            let input = values_yaml(&values, target, unit)?;
            before.push(phase.has_manifests().then(|| values.clone()));
            for source in phase.values.iter().filter(|s| s.present()) {
                let shown = source.shown(shown_root);
                let Some(output) = values_output(source, &shown, repo_root, &input, inputs)? else {
                    continue;
                };
                absorb(
                    &mut values,
                    &mut sets,
                    output,
                    &source.describe(),
                    &shown,
                    "a values source's output",
                )?;
            }
            if let Some(layer) = &ephemeral_layer {
                merge(&mut values, layer.clone());
            }
        }

        let agreed = Agreed {
            version: agree(Field::Version, target, unit, &sets)?,
            release_name: agree(Field::ReleaseName, target, unit, &sets)?,
            namespace: agree(Field::ReleaseNamespace, target, unit, &sets)?,
        };
        check_release(&agreed, target, unit)?;
        Ok(Evaluated {
            before,
            values,
            sets,
            agreed,
        })
    }
}

/// What a values source (shown as `shown`) outputs, given the values (`input`, as YAML): one
/// mapping (with its SSM refs resolved, or checked), or nothing.
fn values_output(
    source: &Source,
    shown: &Path,
    repo_root: &Path,
    input: &str,
    inputs: &RunInputs<'_>,
) -> Result<Option<Value>, ObeliskK8sError> {
    let mut documents = if let Source::Inline { values, .. } = source {
        vec![values.clone()]
    } else {
        // Only a chart reads the values as a tree, and a chart can't be a values source.
        let no_tree = Map::new();
        let render_inputs = render::Inputs {
            repo_root,
            values: &no_tree,
            values_yaml: input,
            release: RENDER_RELEASE,
            namespace: RENDER_NAMESPACE,
            chart_version: None,
            capabilities: None,
        };
        let text = render::text(source, shown, &render_inputs)?;
        if let Source::Url { sha256, .. } = source {
            ssm::check_pinned(&text, sha256.as_deref(), shown)?;
        }
        yaml::parse_values_stream(&text, shown)?
    };
    if documents.len() > 1 {
        return Err(ObeliskK8sError::Config {
            file: shown.to_owned(),
            at: None,
            message: format!(
                "a values source must output one mapping, not {} documents",
                documents.len()
            ),
        });
    }
    let Some(mut output) = documents.pop() else {
        return Ok(None);
    };
    resolve_refs(inputs.ssm, &mut output, shown)?;
    Ok(Some(output))
}

/// Resolves (with `ssm`; else checks) the refs in `output`'s values (not under `obelisk:`, which
/// only takes the release), named by `shown` and the key.
fn resolve_refs(
    ssm: Option<&Ssm>,
    output: &mut Value,
    shown: &Path,
) -> Result<(), ObeliskK8sError> {
    let Value::Object(map) = output else {
        // Not a mapping: refused when it's merged.
        return Ok(());
    };
    let named = map
        .iter_mut()
        .filter(|(k, _)| *k != OBELISK_KEY)
        .map(|(key, value)| (format!("{}, at `{key}`", shown.display()), value));
    ssm::resolve_or_check(ssm, named)?;
    Ok(())
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

    /// What a values source set.
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

/// The agreed `what` for `unit` on `target`: every value given (by the target, the unit, a values
/// source or the ephemeral values) must be the same one, and there must be one.
fn agree(
    what: Field,
    target: &Target,
    unit: &Unit,
    sets: &FileObelisks,
) -> Result<String, ObeliskK8sError> {
    let params = InterpolationParams {
        target: Some(&target.name),
        unit: Some(&unit.name),
        path_unit: None,
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
            "no {} for {case}: set `{}` in its target or unit, or `{OBELISK_KEY}.{}` in a values source{}",
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
    use crate::config::build::parse_plan;
    use crate::config::ephemeral::EphemeralValues;
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo};
    use std::fmt::Write as _;

    /// Targets `prod` and `dev` (on cluster `c`, releasing each unit as itself into a namespace named
    /// after the target), unit `app` (version 1.0.0), and the repo's build file `./build.gotmpl`
    /// (only rendered by the tests that run `helm`: the others give its output); `files` on top,
    /// replacing any at the same path.
    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let mut all = vec![
            ("repo.yaml", "repo:\n  name: r\n  build: ./build.gotmpl\n"),
            ("targets/prod/target.yaml", TARGET),
            ("targets/dev/target.yaml", TARGET),
            (
                "units/app/unit.yaml",
                "unit:\n  release: { version: 1.0.0 }\n",
            ),
        ];
        all.retain(|(p, _)| !files.iter().any(|(q, _)| q == p));
        all.extend_from_slice(files);
        write_repo(&all)
    }

    /// One case's values, as YAML (the repo's path as `<repo>`), the tree, and what was set.
    #[derive(Debug)]
    struct Out {
        yaml: String,
        values: Map<String, Value>,
        sets: FileObelisks,
    }

    /// Unit `app` on `target`, its build file having output `output`.
    fn run_with(
        dir: &tempfile::TempDir,
        target: &str,
        output: &str,
        ephemeral: Option<&Ephemeral>,
        ssm: Option<&Ssm>,
    ) -> Result<Out, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&[target.to_owned()], &["app".to_owned()])
            .unwrap();
        let global = global();
        let inputs = RunInputs::new(dir.path(), &selection, ephemeral, &global, ssm)?;
        let (target, unit) = selection.cases[0];
        let shown = Path::new(".obelisk-k8s/build.gotmpl");
        let plan = parse_plan(output, shown, shown, &config.helm_repos, dir.path())?;
        let obelisk = config.obelisk_values(target, unit, &inputs)?;
        let case =
            RepoConfig::values_case(dir.path(), None, target, unit, &inputs, &plan, obelisk)?;
        Ok(Out {
            yaml: case
                .yaml
                .replace(&dir.path().display().to_string(), "<repo>"),
            values: case.values,
            sets: case.sets,
        })
    }

    fn run(dir: &tempfile::TempDir, target: &str, output: &str) -> Result<Out, ObeliskK8sError> {
        run_with(dir, target, output, None, None)
    }

    /// A plan of one phase per item, `p1`, `p2`, …, each with these values sources.
    fn phases(values: &[&str]) -> String {
        let mut text = "build:\n  phases:\n".to_owned();
        for (i, sources) in values.iter().enumerate() {
            let _ = writeln!(text, "    - name: p{}\n      values: [{sources}]", i + 1);
        }
        text
    }

    /// The `obelisk:` block for unit `app` on `prod` (no git, no ephemeral, cluster `c`), with the
    /// release.
    const OBELISK_PROD: &str = "obelisk:\n  repo: r\n  target: prod\n  unit: app\n  cluster:\n    name: c\n    supernet: null\n    subnet: null\n  ephemeral: null\n  git: null\n  paths:\n    repo:\n      root: <repo>\n      obelisk: <repo>/.obelisk-k8s\n    global:\n      obelisk: /home/me/.obelisk-k8s\n  release:\n    name: app\n    namespace: prod\n    version: \"1.0.0\"\n";

    #[test]
    fn the_obelisk_block_alone_without_phases() {
        let dir = repo(&[]);
        assert_eq!(run(&dir, "prod", "build: {}\n").unwrap().yaml, OBELISK_PROD);
    }

    #[test]
    fn a_phase_sees_the_phases_before_it_but_not_itself() {
        let dir = repo(&[]);
        // A command prints how many lines of its input start with `a:`.
        let saw = |key: &str| {
            format!(
                "{{ type: command, command: [sh, -c, \"printf '{key}: %s\\\\n' $(grep -c '^a:' || true)\"] }}"
            )
        };
        let out = run(
            &dir,
            "prod",
            &phases(&[
                &format!(
                    "{{ type: inline, values: {{ a: 1 }} }}, {}",
                    saw("p1-saw-a")
                ),
                &saw("p2-saw-a"),
            ]),
        )
        .unwrap();
        assert_eq!(out.values["p1-saw-a"], 0);
        assert_eq!(out.values["p2-saw-a"], 1);
    }

    #[test]
    fn outputs_merge_in_phase_then_source_order() {
        let dir = repo(&[
            (
                "global.yaml",
                "db:\n  host: global\n  port: 5432\nlist: [1, 2]\nkeep: g\n",
            ),
            (
                "prod.yaml",
                "db:\n  host: prod\n  pool: { max: 10 }\nlist: [3]\n",
            ),
        ]);
        let out = run(
            &dir,
            "prod",
            &phases(&[
                "{ type: file, path: ./global.yaml }, { type: file, path: ./prod.yaml }",
                "{ type: inline, values: { db: { pool: { min: 1 } }, extra: null } }",
            ]),
        )
        .unwrap();
        assert_eq!(
            out.yaml,
            format!(
                "{OBELISK_PROD}db:\n  host: prod\n  port: 5432\n  pool:\n    max: 10\n    min: 1\nlist:\n- 3\nkeep: g\nextra: null\n"
            )
        );
    }

    #[test]
    fn missing_files_are_skipped() {
        let dir = repo(&[("here.yaml", "here: true\n")]);
        // Not even `helm` runs for a missing `gotmpl`.
        let out = run(
            &dir,
            "prod",
            &phases(&[
                "{ type: gotmpl, path: ./nope.gotmpl }, { type: file, path: ./nope.yaml }, { type: file, path: /nowhere/x.yaml }",
                "{ type: file, path: ./here.yaml }",
            ]),
        )
        .unwrap();
        assert_eq!(out.yaml, format!("{OBELISK_PROD}here: true\n"));
    }

    #[test]
    fn keeps_quoted_strings_and_reads_plain_scalars_the_yaml_1_1_way() {
        let dir = repo(&[(
            "v.yaml",
            "quoted: [\"0755\", \"yes\", \"1e3\", \"null\", \"\"]\n\
             mode: 0755\nperm: 0644\nhex: 0x1F\nflag: Yes\nn: 3\nf: 1.50\n\
             tpl: |\n  {{ .Values.x }}\n  line two\nref: \"before ref+awsssm:///app/x\"\n",
        )]);
        let out = run(&dir, "prod", &phases(&["{ type: file, path: ./v.yaml }"])).unwrap();
        // (The key `n` comes out quoted: plain, it's a YAML 1.1 boolean.)
        assert_eq!(
            out.yaml.strip_prefix(OBELISK_PROD).unwrap(),
            "quoted:\n- \"0755\"\n- \"yes\"\n- \"1e3\"\n- \"null\"\n- \"\"\n\
             mode: 493\nperm: 420\nhex: 31\nflag: true\n\"n\": 3\nf: 1.5\n\
             tpl: |\n  {{ .Values.x }}\n  line two\nref: before ref+awsssm:///app/x\n"
        );
        // Documented in `config::values`: without a `.`, these aren't what a YAML 1.1 reader makes
        // of them.
        let out = run(
            &dir,
            "prod",
            &phases(&["{ type: inline, values: { e: 1e3, z: 08 } }"]),
        )
        .unwrap();
        assert!(out.yaml.ends_with("e: 1000.0\nz: 8.0\n"), "{}", out.yaml);
    }

    #[test]
    fn an_output_must_be_one_mapping() {
        let dir = repo(&[("list.yaml", "- a\n- b\n"), ("bad.yaml", "a: [\n")]);
        let error = |sources: &str| {
            run(&dir, "prod", &phases(&[sources]))
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error("{ type: file, path: ./list.yaml }"),
            ".obelisk-k8s/list.yaml: a values source's output must be a mapping (or empty)"
        );
        assert!(
            error("{ type: file, path: ./bad.yaml }").contains(".obelisk-k8s/bad.yaml:"),
            "{}",
            error("{ type: file, path: ./bad.yaml }")
        );
        assert_eq!(
            error("{ type: command, command: [printf, \"a: 1\\n---\\nb: 2\\n\"] }"),
            "printf: a values source must output one mapping, not 2 documents"
        );
        assert_eq!(
            error("{ type: inline, values: { obelisk: { target: x } } }"),
            ".obelisk-k8s/build.gotmpl (build.phases[0].values[0]): `obelisk:` is reserved; a values source's output may only set `obelisk.release.name`, `obelisk.release.namespace` and `obelisk.release.version`, not `obelisk.target`"
        );
    }

    #[test]
    fn errors_show_paths_under_the_repo_when_asked() {
        let dir = repo(&[("list.yaml", "- a\n")]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["prod".into()], &["app".into()]).unwrap();
        let global = global();
        let inputs = RunInputs::new(dir.path(), &selection, None, &global, None).unwrap();
        let (target, unit) = selection.cases[0];
        let error = |source: &str| {
            let plan = parse_plan(
                &phases(&[source]),
                Path::new(".obelisk-k8s/build.gotmpl"),
                Path::new("~/dev/r/.obelisk-k8s/build.gotmpl"),
                &config.helm_repos,
                dir.path(),
            )
            .unwrap();
            let obelisk = config.obelisk_values(target, unit, &inputs).unwrap();
            RepoConfig::values_case(
                dir.path(),
                Some(Path::new("~/dev/r")),
                target,
                unit,
                &inputs,
                &plan,
                obelisk,
            )
            .unwrap_err()
            .to_string()
        };
        assert_eq!(
            error("{ type: file, path: ./list.yaml }"),
            "~/dev/r/.obelisk-k8s/list.yaml: a values source's output must be a mapping (or empty)"
        );
        // An inline source too: its label is the build file's path in the repo, shown the same way.
        assert_eq!(
            error("{ type: inline, values: { obelisk: { x: 1 } } }"),
            "~/dev/r/.obelisk-k8s/build.gotmpl (build.phases[0].values[0]): `obelisk:` is reserved; a values source's output may only set `obelisk.release.name`, `obelisk.release.namespace` and `obelisk.release.version`, not `obelisk.x`"
        );
    }

    #[test]
    fn the_obelisk_block_has_the_targets_cluster() {
        let dir = repo(&[
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: obd\n  release: { name: app, namespace: n }\n",
            ),
            ("targets/dev/target.yaml", "target:\n  cluster: nope\n"),
        ]);
        let out = run(&dir, "prod", "build: {}\n").unwrap();
        assert!(
            out.yaml
                .contains("  cluster:\n    name: obd\n    supernet: main\n    subnet: sub\n"),
            "{}",
            out.yaml
        );
        assert_eq!(
            run(&dir, "dev", "build: {}\n").unwrap_err().to_string(),
            "target `dev` deploys to cluster `nope`, which isn't in the global config at ~/.obelisk-k8s (it has: c, obd): add it with `obelisk-k8s config global cluster add`"
        );
    }

    fn ephemeral_repo() -> tempfile::TempDir {
        repo(&[
            (
                "targets/preview/target.yaml",
                "target:\n  cluster: c\n  ephemeral: true\n  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\" }\n",
            ),
            (
                "targets/qa/target.yaml",
                "target:\n  cluster: c\n  ephemeral: null\n",
            ),
        ])
    }

    fn ephemeral(values: &str) -> Ephemeral {
        Ephemeral::new("pr-7".into(), EphemeralValues::Inline(values.into())).unwrap()
    }

    #[test]
    fn ephemeral_values_are_seen_by_every_phase_and_always_win() {
        let dir = ephemeral_repo();
        let e = ephemeral("c: ephemeral\nobelisk:\n  release: { version: \"1.0.0\" }\n");
        let out = run_with(
            &dir,
            "preview",
            &phases(&[
                "{ type: inline, values: { a: p1, c: p1 } }, { type: command, command: [sh, -c, \"grep -q '^c: ephemeral' && echo 'saw: ephemeral'\"] }",
                "{ type: inline, values: { c: p2 } }",
            ]),
            Some(&e),
            None,
        )
        .unwrap();
        assert_eq!(out.values["c"], "ephemeral");
        assert_eq!(out.values["saw"], "ephemeral");
        assert_eq!(out.values["a"], "p1");
        assert_eq!(out.values[OBELISK_KEY]["ephemeral"], "pr-7");
        // Its version is set once, as `--ephemeral-values`.
        assert_eq!(
            out.sets,
            [(
                PathBuf::from("--ephemeral-values"),
                FileObelisk {
                    version: Some("1.0.0".to_owned()),
                    ..FileObelisk::default()
                }
            )]
        );
        let error = run_with(
            &dir,
            "preview",
            "build: {}\n",
            Some(&ephemeral("obelisk:\n  target: x\n")),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("--ephemeral-values: "), "{error}");
        let error = run_with(
            &dir,
            "preview",
            "build: {}\n",
            Some(&ephemeral("[1]")),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("the ephemeral values must be a mapping"),
            "{error}"
        );
    }

    #[test]
    fn an_ephemeral_target_needs_an_ephemeral_and_only_it_does() {
        let dir = ephemeral_repo();
        assert_eq!(
            run(&dir, "preview", "build: {}\n").unwrap_err().to_string(),
            "target `preview` is ephemeral: pass `--ephemeral-name`, and `--ephemeral-values` or `--ephemeral-values-path`; or pick other targets with `-t`"
        );
        assert_eq!(
            run_with(&dir, "qa", "build: {}\n", Some(&ephemeral("{}")), None)
                .unwrap_err()
                .to_string(),
            "ephemeral `pr-7` given, but none of the targets being rendered is ephemeral (`ephemeral: true`)"
        );
    }

    #[test]
    fn a_version_conflict_names_the_ephemeral_values() {
        let dir = ephemeral_repo();
        let e = ephemeral("obelisk: { release: { version: v2 } }");
        assert_eq!(
            run_with(&dir, "preview", "build: {}\n", Some(&e), None)
                .unwrap_err()
                .to_string(),
            "unit `app` on target `preview` has 2 different versions, from 2 places: `1.0.0` (.obelisk-k8s/units/app/unit.yaml), `v2` (--ephemeral-values); they must agree"
        );
    }

    fn release(dir: &tempfile::TempDir, output: &str) -> Result<Value, ObeliskK8sError> {
        Ok(run(dir, "prod", output)?.values[OBELISK_KEY]["release"].clone())
    }

    #[test]
    fn agrees_on_one_release_across_phases() {
        let dir = repo(&[
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit: {}\n"),
        ]);
        // Each from a different phase; the same value twice is one.
        let plan = phases(&[
            "{ type: inline, values: { obelisk: { release: { name: app } } } }",
            "{ type: inline, values: { obelisk: { release: { namespace: ns, name: app } } } }",
            "{ type: inline, values: { obelisk: { release: { version: v1 } } } }",
        ]);
        assert_eq!(
            release(&dir, &plan).unwrap(),
            serde_json::json!({ "name": "app", "namespace": "ns", "version": "v1" })
        );
        // A conflict between phases names both.
        let plan = phases(&[
            "{ type: inline, values: { obelisk: { release: { name: app, namespace: ns, version: v1 } } } }",
            "{ type: file, path: ./other.yaml }",
        ]);
        let dir = repo(&[
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit: {}\n"),
            ("other.yaml", "obelisk:\n  release: { name: other }\n"),
        ]);
        assert_eq!(
            release(&dir, &plan).unwrap_err().to_string(),
            "unit `app` on target `prod` has 2 different release names, from 2 places: `app` (.obelisk-k8s/build.gotmpl (build.phases[0].values[0])), `other` (.obelisk-k8s/other.yaml); they must agree"
        );
    }

    #[test]
    fn needs_exactly_one_of_each() {
        let dir = repo(&[("units/app/unit.yaml", "unit: {}\n")]);
        assert_eq!(
            release(&dir, "build: {}\n").unwrap_err().to_string(),
            "no version for unit `app` on target `prod`: set `release.version` in its target or unit, or `obelisk.release.version` in a values source (e.g. from `.Values.obelisk.git`)"
        );
        let dir = repo(&[(
            "targets/prod/target.yaml",
            "target:\n  cluster: c\n  release: { name: app }\n",
        )]);
        assert_eq!(
            release(&dir, "build: {}\n").unwrap_err().to_string(),
            "no release namespace for unit `app` on target `prod`: set `release.namespace` in its target or unit, or `obelisk.release.namespace` in a values source (e.g. from `.Values.obelisk.ephemeral`, on an ephemeral target)"
        );
        let dir = repo(&[]);
        assert_eq!(
            release(
                &dir,
                &phases(&["{ type: inline, values: { obelisk: { release: { version: v1, namespace: x } } } }"])
            )
            .unwrap_err()
            .to_string(),
            "unit `app` on target `prod` has 2 different versions, from 2 places: `1.0.0` (.obelisk-k8s/units/app/unit.yaml), `v1` (.obelisk-k8s/build.gotmpl (build.phases[0].values[0])); they must agree"
        );
    }

    #[test]
    fn a_unit_can_give_its_release_interpolated() {
        let dir = repo(&[
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
            release(&dir, "build: {}\n").unwrap(),
            serde_json::json!({ "name": "app-prod", "namespace": "n", "version": "v1" })
        );
        // `${{obelisk:path:unit}}` is for build paths only.
        let dir = repo(&[(
            "units/app/unit.yaml",
            "unit:\n  release: { version: \"${{obelisk:path:unit}}\" }\n",
        )]);
        assert_eq!(
            release(&dir, "build: {}\n").unwrap_err().to_string(),
            ".obelisk-k8s/units/app/unit.yaml: `release.version: ${{obelisk:path:unit}}`: `${{obelisk:path:unit}}` has no value here"
        );
    }

    #[test]
    fn the_release_must_be_valid_for_helm_and_kubernetes() {
        let error = |release: &str| {
            let text = format!("target:\n  cluster: c\n  release: {release}\n");
            let dir = repo(&[("targets/prod/target.yaml", &text)]);
            run(&dir, "prod", "build: {}\n").unwrap_err().to_string()
        };
        assert!(error("{ name: app, namespace: Not_Valid }").starts_with(
            "release namespace `Not_Valid` for unit `app` on target `prod` isn't a valid name"
        ));
        assert!(
            error("{ name: -app, namespace: n }").starts_with(
                "release name `-app` for unit `app` on target `prod` isn't a valid name"
            )
        );
        assert!(
            error(&format!("{{ name: {}, namespace: n }}", "a".repeat(54)))
                .contains("is too long: Helm release names are at most 53")
        );
    }

    #[test]
    fn ssm_refs_resolve_in_each_output_before_it_merges() {
        let dir = repo(&[(
            "db.yaml",
            "db:\n  url: \"pg://u:${{obelisk:aws-ssm:/p}}@h\"\n",
        )]);
        // The second phase's command sees the first's values resolved.
        let plan = phases(&[
            "{ type: inline, values: { db: { password: \"${{obelisk:aws-ssm:/p}}\" } } }, { type: file, path: ./db.yaml }",
            "{ type: command, command: [sh, -c, \"grep -q 'password: s3cret' && echo 'saw: plaintext' || echo 'saw: ref'\"] }",
        ]);
        let out = run(&dir, "prod", &plan).unwrap();
        assert_eq!(
            Value::Object(out.values.clone())["db"],
            serde_json::json!({ "password": "${{obelisk:aws-ssm:/p}}", "url": "pg://u:${{obelisk:aws-ssm:/p}}@h" })
        );
        assert_eq!(out.values["saw"], "ref");
        let (ssm, asked) = crate::ssm::tests::fake(&[("/p", "s3cret")]);
        let out = run_with(&dir, "prod", &plan, None, Some(&ssm)).unwrap();
        assert_eq!(
            Value::Object(out.values.clone())["db"],
            serde_json::json!({ "password": "s3cret", "url": "pg://u:s3cret@h" })
        );
        assert_eq!(out.values["saw"], "plaintext");
        // Fetched once, and kept for the run.
        assert_eq!(asked.borrow().len(), 1);
    }

    #[test]
    fn a_bad_ssm_ref_is_an_error_even_unresolved() {
        let dir = repo(&[]);
        assert_eq!(
            run(
                &dir,
                "prod",
                &phases(&["{ type: inline, values: { x: { y: \"${{obelisk:unit}}\" } } }"])
            )
            .unwrap_err()
            .to_string(),
            ".obelisk-k8s/build.gotmpl (build.phases[0].values[0]), at `x`: `${{obelisk:unit}}` isn't available here: only `${{obelisk:aws-ssm:<key>}}`"
        );
    }

    fn report(dir: &tempfile::TempDir, targets: &[&str]) -> Result<String, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        let selection = config.select(&targets, &[]).unwrap();
        let text = config
            .values_report(dir.path(), &selection, None, None, &global(), None, |_| {})?
            .to_string();
        Ok(anstream::adapter::strip_str(&text)
            .to_string()
            .replace(&dir.path().display().to_string(), "<repo>"))
    }

    #[test]
    fn go_template_sources_see_the_values_before_their_phase() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                "build:\n  phases:\n    - name: base\n      values:\n        - { type: inline, values: { db: { host: h, port: 5432 } } }\n        - { type: gotmpl, path: \"./{{ .Values.obelisk.target }}.gotmpl\" }\n    - name: derived\n      values: [{ type: gotmpl, path: ./derive.gotmpl }]\n",
            ),
            ("prod.gotmpl", "replicas: 3\n"),
            (
                "derive.gotmpl",
                "db:\n  url: \"postgres://{{ .Values.db.host }}:{{ .Values.db.port }}\"\nrelease: {{ .Release.Name }}/{{ .Release.Namespace }}\n{{- if .Values.replicas }}\nsaw: replicas\n{{- end }}\n",
            ),
        ]);
        let text = report(&dir, &["dev", "prod"]).unwrap();
        let docs: Vec<Value> = serde_saphyr::from_str_multiple(&text).unwrap();
        assert_eq!(docs.len(), 2);
        assert!(
            text.starts_with("# [ target=dev, unit=app ]\nobelisk:\n"),
            "{text}"
        );
        assert!(
            text.contains("\n---\n# [ target=prod, unit=app ]\n"),
            "{text}"
        );
        assert_eq!(docs[1]["db"]["url"], "postgres://h:5432");
        // Not the unit's release, which isn't agreed on yet: a fixed one.
        assert_eq!(docs[1]["release"], "obelisk-build/default");
        assert_eq!(docs[1]["saw"], "replicas");
        // `dev` has no `dev.gotmpl`: skipped.
        assert_eq!(docs[0].get("saw"), None);
    }

    #[test]
    fn a_unit_longer_than_a_helm_release_name_renders_its_go_templates() {
        if !helm_installed() {
            return;
        }
        // Its release is named otherwise; the build file and Go templates render as a fixed one.
        let long = "a".repeat(63);
        let dir = repo(&[
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  release: { namespace: prod }\n",
            ),
            (
                "units/app/unit.yaml",
                "unit:\n  release: { name: app, version: 1.0.0 }\n",
            ),
            (
                &format!("units/{long}/unit.yaml"),
                "unit:\n  release: { name: short, version: 1.0.0 }\n",
            ),
            (
                "build.gotmpl",
                "build:\n  phases: [{ name: p, values: [{ type: gotmpl, path: ./v.gotmpl }] }]\n",
            ),
            ("v.gotmpl", "release: {{ .Release.Name }}\n"),
        ]);
        let text = report(&dir, &["prod"]).unwrap();
        assert!(
            text.contains(&format!("# [ target=prod, unit={long} ]\n")),
            "{text}"
        );
        assert!(text.contains("\nrelease: obelisk-build\n"), "{text}");
    }

    #[test]
    fn a_gotmpl_source_can_set_the_version_from_a_real_sha() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            ("units/app/unit.yaml", "unit: {}\n"),
            (
                "build.gotmpl",
                "build:\n  phases: [{ name: version, values: [{ type: gotmpl, path: ./version.gotmpl }] }]\n",
            ),
            (
                "version.gotmpl",
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
        let text = report(&dir, &["prod"]).unwrap();
        assert!(
            text.contains(&format!("    version: 0.0.0-{short}\n")),
            "{text}"
        );
    }

    #[test]
    fn a_bad_go_template_source_names_its_file() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                "build:\n  phases: [{ name: p, values: [{ type: gotmpl, path: ./gen.gotmpl }] }]\n",
            ),
            ("gen.gotmpl", "x: {{ .Values.nope.deeper }}\n"),
        ]);
        let e = report(&dir, &["dev"]).unwrap_err().to_string();
        let file = ".obelisk-k8s/gen.gotmpl";
        assert!(
            e.starts_with(&format!("{file}: helm couldn't render it: ")),
            "{e}"
        );
        assert!(!e.contains("obelisk-render/templates"), "{e}");
    }
}
