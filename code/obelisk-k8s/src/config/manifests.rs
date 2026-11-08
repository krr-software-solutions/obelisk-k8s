//! The k8s resources for a unit on a target (`template manifests`): its build plan's manifests
//! sources, each rendered against the values of the phases before its own (see `config::phases`),
//! joined in phase order, then source order.
//!
//! Each source renders on its own (see `config::render` for the types). A Go template (`type:
//! gotmpl`) renders through a throwaway chart, with the values as `.Values`, so it can't use another
//! one's `define`s; an upstream chart (`type: helm`) renders as the case's release, its values from
//! those values. The values are V₍ₖ₋₁₎ (for a source in phase k) with the agreed release added as
//! `obelisk.release.{name, namespace, version}`: every phase's values run, and the release is agreed
//! on, before any manifests render. `.Release.Name` and `.Release.Namespace` are the agreed release,
//! and the chart's `version` and `appVersion` its agreed version.
//!
//! The images declared in phases before k (see `config::images`) are added too, as
//! `obelisk.images.<name>.{ref, repository, tag}`: the ref the image is delivered as to the target's
//! cluster (`<push.prefix>/<name>:<version>` with a `docker.yaml`, `<name>:<version>` on a local k3d
//! cluster without one; any other cluster is an error; a repository over docker's 255 characters is
//! one too). Only the images phases before the last manifests phase render (their specs checked).
//! An image's name may hold `-`s, `.`s and `/`s (a repository path): it's still one key, which a
//! template reaches with `index`, e.g. `{{ (index .Values.obelisk.images "krrss-mc/web").ref }}`.
//!
//! What Helm prints is kept as is: documents separated by `---`, each under a `# Source:` comment
//! naming the template file; a `file`, `url` or `command` source's output goes under one `# Source:`
//! of its own (its later documents keep theirs, if any). A source may print any number of documents,
//! or none (one that prints only a comment gives an empty document, which `kubectl` ignores). A `file`
//! or `url` source's SSM refs are its own, resolved (or checked) as it renders; the others' come from
//! the values. A unit with no manifests sources renders nothing, and its values phases don't run.

use std::fmt;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value};

use crate::ObeliskK8sError;
use crate::config::build::{BuildPlan, Phase, Source, Warnings};
use crate::config::ephemeral::Ephemeral;
use crate::config::global::GlobalConfig;
use crate::config::images::{images_values, render_images};
use crate::config::paths::{Selection, write_yaml_cases};
use crate::config::push::Destination;
use crate::config::render;
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::config::values::{Agreed, OBELISK_KEY, values_yaml, with_release};
use crate::helm;
use crate::ssm::{self, Ssm};

/// One (target, unit) case's rendered resources.
#[derive(Debug)]
pub struct ManifestsCase<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// Every manifests source's output, in phase order then source order, as Helm printed it (empty
    /// if there are none).
    pub manifests: String,
    /// The release they render as: `None` if the unit has no manifests sources (nothing was
    /// rendered, and nothing agreed on).
    pub release: Option<Agreed>,
}

/// What `template manifests` prints: a single case's resources, or several, each under a magenta
/// `# [ target=…, unit=… ]` comment.
#[derive(Debug)]
pub struct ManifestsReport<'a> {
    pub cases: Vec<ManifestsCase<'a>>,
    pub single: bool,
}

impl RepoConfig {
    /// The rendered resources of every case in `selection`. `shown_root` and `warn` are as for
    /// [`RepoConfig::values_report`].
    ///
    /// # Errors
    ///
    /// Whatever running the values phases fails with; [`ObeliskK8sError::Helm`] if a manifests
    /// source doesn't render.
    #[allow(clippy::too_many_arguments)]
    pub fn manifests_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        ssm: Option<&Ssm>,
        warn: impl FnMut(&str),
    ) -> Result<ManifestsReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global, ssm)?;
        let mut cases = Vec::new();
        let mut warnings = Warnings::new(warn);
        for (target, unit) in &selection.cases {
            let obelisk = self.obelisk_values(target, unit, &inputs)?;
            let plan = self.build_plan(repo_root, shown_root, target, unit, &obelisk)?;
            warnings.add(&plan.warnings);
            // Offline: charts render with Helm's default capabilities.
            cases.push(Self::manifests_for(
                repo_root, shown_root, target, unit, &inputs, &plan, obelisk, None,
            )?);
        }
        Ok(ManifestsReport {
            cases,
            single: selection.single,
        })
    }

    /// One case's manifests, from `plan`. `obelisk` is as for [`RepoConfig::run_values`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn manifests_for<'a>(
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
        plan: &BuildPlan,
        obelisk: Map<String, Value>,
        capabilities: Option<&helm::Capabilities>,
    ) -> Result<ManifestsCase<'a>, ObeliskK8sError> {
        let mut manifests = String::new();
        let mut release = None;
        // Nothing to render: skip the values phases too (each source may be a `helm` run).
        if plan.has_manifests() {
            let evaluated =
                Self::run_values(repo_root, shown_root, target, unit, inputs, plan, obelisk)?;
            let agreed = evaluated.agreed;
            let version = &agreed.version;
            if !helm::is_chart_version(version) {
                return Err(ObeliskK8sError::Usage(format!(
                    "version `{version}` for unit `{}` on target `{}` isn't SemVer, which Helm needs for a chart version: e.g. `0.0.0-{version}`, or a tag like `v1.2.3`",
                    unit.name, target.name
                )));
            }
            // Each image is in `obelisk.images` for the phases after the one declaring it: only the
            // images phases before the last manifests phase are seen, so only they render.
            let last_manifests = plan.phases.iter().rposition(Phase::has_manifests);
            let images = render_images(
                repo_root,
                shown_root,
                target,
                unit,
                plan,
                last_manifests.unwrap_or(0),
                &evaluated.before,
                &agreed,
            )?;
            // A ref only means something where the image can be delivered: a cluster with nowhere
            // to push them is an error, as for `images build`.
            let to = if images.is_empty() {
                None
            } else {
                let to = Destination::of(inputs.cluster(target)?).map_err(|e| match e {
                    ObeliskK8sError::Usage(message) => ObeliskK8sError::Usage(format!(
                        "unit `{}` on target `{}` has images its manifests see (`obelisk.images`), but {message}",
                        unit.name, target.name
                    )),
                    other => other,
                })?;
                Some(to)
            };
            // Phase k renders against the values before it, V₍ₖ₋₁₎: kept for each phase with
            // manifests.
            for (k, (phase, before)) in plan.phases.iter().zip(evaluated.before).enumerate() {
                // Kept for a phase with images too: only ones with manifests render here.
                let Some(before) = before.filter(|_| phase.has_manifests()) else {
                    continue;
                };
                let mut values = with_release(before, &agreed);
                let earlier = images
                    .iter()
                    .filter(|declared| plan.phases[..k].iter().any(|p| p.name == declared.phase));
                let earlier: Vec<_> = earlier.map(|declared| &declared.image).collect();
                if let (false, Some(to), Some(Value::Object(obelisk))) =
                    (earlier.is_empty(), &to, values.get_mut(OBELISK_KEY))
                {
                    obelisk.insert(
                        "images".to_owned(),
                        Value::Object(images_values(earlier, to)?),
                    );
                }
                let values_yaml = values_yaml(&values, target, unit)?;
                let render_inputs = render::Inputs {
                    repo_root,
                    values: &values,
                    values_yaml: &values_yaml,
                    release: &agreed.release_name,
                    namespace: &agreed.namespace,
                    chart_version: Some(version),
                    capabilities,
                };
                for source in phase.manifests.iter().filter(|s| s.present()) {
                    render_source(source, shown_root, &render_inputs, inputs, &mut manifests)?;
                }
            }
            release = Some(agreed);
        }
        Ok(ManifestsCase {
            target: &target.name,
            unit: &unit.name,
            manifests,
            release,
        })
    }
}

/// Renders one manifests source onto `manifests`.
fn render_source(
    source: &Source,
    shown_root: Option<&Path>,
    render_inputs: &render::Inputs<'_>,
    inputs: &RunInputs<'_>,
    manifests: &mut String,
) -> Result<(), ObeliskK8sError> {
    let shown = source.shown(render_inputs.repo_root, shown_root);
    let mut rendered = render::text(source, &shown, render_inputs)?;
    // A raw manifest's refs are its own; the others' come from the (resolved) values.
    if matches!(source, Source::File { .. } | Source::Url { .. }) {
        if let Source::Url { sha256, .. } = source {
            ssm::check_pinned(&rendered, sha256.as_deref(), &shown)?;
        }
        rendered = ssm::in_manifests(inputs.ssm, &rendered, &shown)?;
    }
    // A template that renders nothing still gets a newline from Helm.
    if rendered.trim().is_empty() {
        return Ok(());
    }
    // Helm names each document's template (`# Source:`); the others are named here.
    if matches!(source, Source::Gotmpl { .. } | Source::Helm(_)) {
        manifests.push_str(&rendered);
    } else {
        let _ = write!(manifests, "---\n# Source: {}\n", shown.display());
        manifests.push_str(rendered.strip_prefix("---\n").unwrap_or(&rendered));
        if !rendered.ends_with('\n') {
            manifests.push('\n');
        }
    }
    Ok(())
}

impl fmt::Display for ManifestsReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Every chunk Helm prints starts with `---`, so cases need nothing between them.
        let cases = self
            .cases
            .iter()
            .map(|c| (c.target, c.unit, c.manifests.as_str()));
        write_yaml_cases(f, self.single, "", cases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::build::parse_plan;
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo};

    /// Targets `prod` and `dev`, unit `app` (version 1.0.0), the repo's build file `./build.gotmpl`
    /// (only rendered by the tests that run `helm`); `files` on top, replacing any at the same path.
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

    /// Unit `app` on `prod`, its build file having output `output`: its manifests, and release.
    fn case(
        dir: &tempfile::TempDir,
        output: &str,
        ssm: Option<&Ssm>,
    ) -> Result<(String, Option<Agreed>), ObeliskK8sError> {
        case_on(dir, output, ssm, &global())
    }

    /// [`case`], with `global` as the global config.
    fn case_on(
        dir: &tempfile::TempDir,
        output: &str,
        ssm: Option<&Ssm>,
        global: &GlobalConfig,
    ) -> Result<(String, Option<Agreed>), ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["prod".to_owned()], &["app".to_owned()])
            .unwrap();
        let inputs = RunInputs::new(dir.path(), &selection, None, global, ssm)?;
        let (target, unit) = selection.cases[0];
        let shown = Path::new(".obelisk-k8s/build.gotmpl");
        let plan = parse_plan(output, shown, shown, &config.helm_repos, dir.path())?;
        let obelisk = config.obelisk_values(target, unit, &inputs)?;
        let case = RepoConfig::manifests_for(
            dir.path(),
            None,
            target,
            unit,
            &inputs,
            &plan,
            obelisk,
            None,
        )?;
        Ok((case.manifests, case.release))
    }

    fn manifests(dir: &tempfile::TempDir, output: &str) -> Result<String, ObeliskK8sError> {
        case(dir, output, None).map(|(manifests, _)| manifests)
    }

    /// A `command` manifests source printing `kind: <kind>` and how many lines of its values start
    /// with `pattern`.
    fn counts(kind: &str, pattern: &str) -> String {
        format!(
            "{{ type: command, command: [sh, -c, \"printf 'kind: {kind}\\\\nseen: %s\\\\n' $(grep -c '{pattern}' || true)\"] }}"
        )
    }

    /// An inline images source declaring `name` at version `v1`.
    fn image(name: &str) -> String {
        format!(
            "{{ type: inline, images: [{{ name: {name}, version: v1, dockerfile: {{ type: file, path: ./app/Dockerfile }} }}] }}"
        )
    }

    #[test]
    fn manifests_see_images_from_earlier_phases_not_their_own() {
        let dir = repo(&[("app/Dockerfile", "FROM scratch\n")]);
        let output = format!(
            "build:\n  phases:\n    - name: one\n      images: [{}]\n      manifests: [{}]\n    - name: two\n      images: [{}]\n      manifests: [{}, {}]\n    - name: three\n      manifests: [{}]\n",
            image("web"),
            counts("One", "images:"),
            image("worker"),
            counts("TwoWeb", "ref: web:v1$"),
            counts("TwoWorker", "worker"),
            counts("Three", "ref: [a-z]*:v1$"),
        );
        // Cluster `c` is a local k3d one: the image is imported as is.
        assert_eq!(
            manifests(&dir, &output).unwrap(),
            "---\n# Source: sh\nkind: One\nseen: 0\n---\n# Source: sh\nkind: TwoWeb\nseen: 1\n---\n# Source: sh\nkind: TwoWorker\nseen: 0\n---\n# Source: sh\nkind: Three\nseen: 2\n"
        );
    }

    #[test]
    fn images_no_manifests_phase_sees_dont_render() {
        // In or after the last manifests phase: a failing images source never runs.
        let dir = repo(&[]);
        let output = format!(
            "build:\n  phases:\n    - name: one\n      images: [{{ type: command, command: [\"false\"] }}]\n      manifests: [{}]\n    - name: two\n      images: [{{ type: command, command: [\"false\"] }}]\n",
            counts("One", "images:")
        );
        assert_eq!(
            manifests(&dir, &output).unwrap(),
            "---\n# Source: sh\nkind: One\nseen: 0\n"
        );
    }

    #[test]
    fn refs_follow_the_clusters_destination() {
        use crate::config::global::docker::{Docker, Push};
        use crate::config::global::{K8s, Location};
        let dir = repo(&[("app/Dockerfile", "FROM scratch\n")]);
        let output = format!(
            "build:\n  phases:\n    - name: one\n      images: [{}]\n    - name: two\n      manifests: [{}]\n",
            image("web"),
            counts("Two", "^      ref: reg.internal:5000/krrss/web:v1$")
        );
        let mut global = global();
        global.clusters[0].docker = Some(Docker {
            file: "clusters/c/docker.yaml".into(),
            push: Push {
                prefix: "reg.internal:5000/krrss".to_owned(),
                credentials: None,
            },
            pull_auths: Vec::new(),
        });
        let (text, _) = case_on(&dir, &output, None, &global).unwrap();
        assert_eq!(text, "---\n# Source: sh\nkind: Two\nseen: 1\n");

        // A repository path keeps its layout under the prefix.
        let slashed = format!(
            "build:\n  phases:\n    - name: one\n      images: [{}]\n    - name: two\n      manifests: [{}]\n",
            image("krrss-mc/auth/web-server"),
            counts(
                "Two",
                "^      ref: reg.internal:5000/krrss/krrss-mc/auth/web-server:v1$"
            )
        );
        let (text, _) = case_on(&dir, &slashed, None, &global).unwrap();
        assert_eq!(text, "---\n# Source: sh\nkind: Two\nseen: 1\n");

        // A cluster with nowhere to push: no ref would mean the right image.
        global.clusters[0].docker = None;
        (global.clusters[0].k8s, global.clusters[0].location) = (K8s::Talos, Location::Remote);
        assert_eq!(
            case_on(&dir, &output, None, &global)
                .unwrap_err()
                .to_string(),
            "unit `app` on target `prod` has images its manifests see (`obelisk.images`), but cluster `c` has nowhere to push images: add clusters/c/docker.yaml"
        );

        // A unit with no images there renders as before.
        let plain = format!(
            "build:\n  phases:\n    - name: one\n      manifests: [{}]\n",
            counts("One", "images:")
        );
        let (text, _) = case_on(&dir, &plain, None, &global).unwrap();
        assert_eq!(text, "---\n# Source: sh\nkind: One\nseen: 0\n");
    }

    #[test]
    fn a_template_indexes_a_slashed_image_name() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            ("app/Dockerfile", "FROM scratch\n"),
            (
                "cm.gotmpl",
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\ndata:\n  image: {{ (index .Values.obelisk.images \"krrss-mc/web\").ref | quote }}\n",
            ),
        ]);
        let output = format!(
            "build:\n  phases:\n    - name: one\n      images: [{}]\n    - name: two\n      manifests: [{{ type: gotmpl, path: ./cm.gotmpl }}]\n",
            image("krrss-mc/web"),
        );
        // Cluster `c` is a local k3d one: imported as is.
        let text = manifests(&dir, &output).unwrap();
        assert!(text.contains("  image: \"krrss-mc/web:v1\"\n"), "{text}");
    }

    #[test]
    fn manifests_see_the_values_before_their_phase_not_their_own() {
        let dir = repo(&[]);
        let output = format!(
            "build:\n  phases:\n    - name: one\n      values: [{{ type: inline, values: {{ a: 1 }} }}]\n      manifests: [{}]\n    - name: two\n      manifests: [{}]\n",
            counts("One", "^a:"),
            counts("Two", "^a:")
        );
        assert_eq!(
            manifests(&dir, &output).unwrap(),
            "---\n# Source: sh\nkind: One\nseen: 0\n---\n# Source: sh\nkind: Two\nseen: 1\n"
        );
    }

    #[test]
    fn every_phase_sees_the_release_agreed_after_all_of_them() {
        // The version comes from the last phase; the first phase's manifests still get it.
        let dir = repo(&[("units/app/unit.yaml", "unit: {}\n")]);
        let output = format!(
            "build:\n  phases:\n    - name: first\n      manifests: [{}]\n    - name: last\n      values: [{{ type: inline, values: {{ obelisk: {{ release: {{ version: v3.0.0 }} }} }} }}]\n",
            counts("First", "^    version: v3.0.0")
        );
        let (text, release) = case(&dir, &output, None).unwrap();
        assert_eq!(text, "---\n# Source: sh\nkind: First\nseen: 1\n");
        assert_eq!(
            release,
            Some(Agreed {
                release_name: "app".to_owned(),
                namespace: "prod".to_owned(),
                version: "v3.0.0".to_owned(),
            })
        );
    }

    #[test]
    fn mixed_phases_join_in_phase_then_source_order() {
        let dir = repo(&[
            ("a.yaml", "kind: A\n"),
            ("c.yaml", "---\nkind: C1\n---\nkind: C2\n"),
        ]);
        let output = "build:\n  phases:\n    - name: p1\n      manifests:\n        - { type: file, path: ./a.yaml }\n        - { type: file, path: ./missing.yaml }\n      values: [{ type: inline, values: { x: 1 } }]\n    - name: p2\n      values: [{ type: inline, values: { y: 2 } }]\n    - name: p3\n      manifests:\n        - { type: command, command: [printf, \"kind: B\"] }\n        - { type: file, path: ./c.yaml }\n";
        assert_eq!(
            manifests(&dir, output).unwrap(),
            "---\n# Source: .obelisk-k8s/a.yaml\nkind: A\n---\n# Source: printf\nkind: B\n---\n# Source: .obelisk-k8s/c.yaml\nkind: C1\n---\nkind: C2\n"
        );
    }

    #[test]
    fn a_unit_without_manifests_renders_nothing_and_agrees_on_nothing() {
        // Its values phases don't run: no release is needed.
        let dir = repo(&[("units/app/unit.yaml", "unit: {}\n")]);
        let output = "build:\n  phases:\n    - name: p\n      values: [{ type: inline, values: { a: 1 } }]\n      manifests: [{ type: gotmpl, path: ./missing.gotmpl }]\n";
        assert_eq!(case(&dir, output, None).unwrap(), (String::new(), None));
    }

    #[test]
    fn a_version_must_be_semver_to_render_manifests() {
        // Checked before anything renders.
        let dir = repo(&[(
            "units/app/unit.yaml",
            "unit:\n  release: { version: b84d8f6 }\n",
        )]);
        let output = "build:\n  phases: [{ name: p, manifests: [{ type: command, command: [\"false\"] }] }]\n";
        assert_eq!(
            manifests(&dir, output).unwrap_err().to_string(),
            "version `b84d8f6` for unit `app` on target `prod` isn't SemVer, which Helm needs for a chart version: e.g. `0.0.0-b84d8f6`, or a tag like `v1.2.3`"
        );
    }

    #[test]
    fn a_file_sources_ssm_refs_resolve_when_asked() {
        let raw = "# kept when there are no refs\napiVersion: v1\nkind: Secret\nmetadata:\n  name: s\nstringData:\n  password: \"${{obelisk:aws-ssm:/p}}\"\n";
        let dir = repo(&[("raw.yaml", raw)]);
        let output =
            "build:\n  phases: [{ name: p, manifests: [{ type: file, path: ./raw.yaml }] }]\n";
        let rendered = manifests(&dir, output).unwrap();
        assert!(rendered.ends_with(raw), "{rendered}");
        let (ssm, _) = crate::ssm::tests::fake(&[("/p", "s3cret")]);
        let (resolved, _) = case(&dir, output, Some(&ssm)).unwrap();
        assert!(resolved.contains("password: s3cret"), "{resolved}");
        assert!(!resolved.contains("aws-ssm"), "{resolved}");
    }

    fn report(dir: &tempfile::TempDir, targets: &[&str]) -> Result<String, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        let selection = config.select(&targets, &["app".to_owned()]).unwrap();
        let text = config
            .manifests_report(dir.path(), &selection, None, None, &global(), None, |_| {})?
            .to_string();
        Ok(anstream::adapter::strip_str(&text).to_string())
    }

    /// A build file: a values phase (`port`, and `url` from a Go template), then the manifests
    /// sources `manifests` (written as a YAML flow list's items).
    fn build_file(manifests: &str) -> String {
        format!(
            "build:\n  phases:\n    - name: values\n      values:\n        - {{ type: inline, values: {{ port: 8080 }} }}\n    - name: derived\n      values: [{{ type: gotmpl, path: ./url.gotmpl }}]\n    - name: out\n      manifests: [{manifests}]\n"
        )
    }

    const URL: &str = "url: \"http://{{ .Values.obelisk.unit }}:{{ .Values.port }}\"\n";
    const CONFIG_MAP: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Values.obelisk.unit }}\n  namespace: {{ .Release.Namespace }}\ndata:\n  url: {{ .Values.url | quote }}\n  release: {{ .Release.Name | quote }}\n  chart: {{ .Chart.Version | quote }}\n  version: {{ .Values.obelisk.release.version | quote }}\n";

    #[test]
    fn go_templates_render_with_the_values_as_the_agreed_release() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                &build_file(
                    "{ type: gotmpl, path: ./cm.gotmpl }, { type: gotmpl, path: \"./{{ .Values.obelisk.target }}.gotmpl\" }",
                ),
            ),
            ("url.gotmpl", URL),
            ("cm.gotmpl", CONFIG_MAP),
            (
                "prod.gotmpl",
                "apiVersion: v1\nkind: Secret\nmetadata:\n  name: a\n---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: b\n",
            ),
        ]);
        let text = report(&dir, &["prod"]).unwrap();
        let sources: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("# Source: "))
            .collect();
        assert_eq!(
            sources,
            [
                "# Source: .obelisk-k8s/cm.gotmpl",
                "# Source: .obelisk-k8s/prod.gotmpl",
                "# Source: .obelisk-k8s/prod.gotmpl",
            ]
        );
        assert!(
            text.contains("  namespace: prod\ndata:\n  url: \"http://app:8080\"\n  release: \"app\"\n  chart: \"1.0.0\"\n  version: \"1.0.0\"\n"),
            "{text}"
        );
        // `dev` has no `dev.gotmpl`: skipped.
        assert!(!report(&dir, &["dev"]).unwrap().contains("Secret"));
    }

    #[test]
    fn several_cases_are_a_stream_under_headers() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                &build_file("{ type: gotmpl, path: ./cm.gotmpl }"),
            ),
            ("url.gotmpl", URL),
            ("cm.gotmpl", CONFIG_MAP),
        ]);
        let text = report(&dir, &["dev", "prod"]).unwrap();
        assert!(
            text.starts_with("# [ target=dev, unit=app ]\n---\n# Source: "),
            "{text}"
        );
        assert!(
            text.contains("\n# [ target=prod, unit=app ]\n---\n"),
            "{text}"
        );
        let docs: Vec<serde_json::Value> = serde_saphyr::from_str_multiple(&text).unwrap();
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn a_template_can_render_nothing() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                &build_file("{ type: gotmpl, path: ./cm.gotmpl }"),
            ),
            ("url.gotmpl", URL),
            (
                "cm.gotmpl",
                "{{- if false }}\nkind: ConfigMap\n{{- end }}\n",
            ),
        ]);
        assert_eq!(report(&dir, &["dev"]).unwrap(), "");
    }

    #[test]
    fn a_bad_template_names_its_file() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                &build_file("{ type: gotmpl, path: ./cm.gotmpl }"),
            ),
            ("url.gotmpl", URL),
            ("cm.gotmpl", "x: {{ .Values.nope.deeper }}\n"),
        ]);
        let e = report(&dir, &["dev"]).unwrap_err().to_string();
        assert!(
            e.starts_with(".obelisk-k8s/cm.gotmpl: helm couldn't render it: "),
            "{e}"
        );
    }

    #[test]
    fn a_number_like_version_stays_a_string_in_the_chart() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "units/app/unit.yaml",
                "unit:\n  release: { version: \"1.0\" }\n",
            ),
            (
                "build.gotmpl",
                &build_file("{ type: gotmpl, path: ./cm.gotmpl }"),
            ),
            ("url.gotmpl", URL),
            ("cm.gotmpl", CONFIG_MAP),
        ]);
        let text = report(&dir, &["dev"]).unwrap();
        assert!(text.contains("  chart: \"1.0\"\n"), "{text}");
    }

    #[test]
    fn every_source_type_joins_the_stream_under_its_source() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            (
                "build.gotmpl",
                &build_file(
                    "{ type: gotmpl, path: ./cm.gotmpl }, { type: file, path: ./raw.yaml }, { type: command, command: [sh, -c, \"printf 'kind: FromCommand\\\\n'\"] }",
                ),
            ),
            ("url.gotmpl", URL),
            ("cm.gotmpl", CONFIG_MAP),
            ("raw.yaml", "kind: Raw\nkeep: '{{ as is }}'\n"),
        ]);
        let text = report(&dir, &["dev"]).unwrap();
        let sources: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# Source: "))
            .collect();
        assert_eq!(
            sources,
            [".obelisk-k8s/cm.gotmpl", ".obelisk-k8s/raw.yaml", "sh"],
            "{text}"
        );
        assert!(
            text.contains(
                "---\n# Source: .obelisk-k8s/raw.yaml\nkind: Raw\nkeep: '{{ as is }}'\n---\n"
            ),
            "{text}"
        );
        assert!(text.ends_with("kind: FromCommand\n"), "{text}");
    }
}
