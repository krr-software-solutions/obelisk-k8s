//! The k8s resources for a unit on a target (`template manifests`): each of its templates rendered with
//! its generated values (see `config::generate`), joined in merge order.
//!
//! Templates are listed like config paths (`templates:` in `repo.yaml`, `target.yaml` and
//! `unit.yaml`; interpolated; missing files skipped). Each is a Go template (`type: gotmpl`), rendered
//! on its own by Helm through a throwaway chart, with the generated values as `.Values`, so a template
//! can't use another one's `define`s. `.Release.Name` and `.Release.Namespace` are the case's agreed
//! release (`obelisk.release.{name, namespace}`, which also need agreeing on before anything renders),
//! and the chart's `version` and `appVersion` its agreed `obelisk.version`.
//!
//! What Helm prints is kept as is: documents separated by `---`, each under a `# Source:` comment naming
//! the template file. A template may print any number of documents, or none (one that prints only a
//! comment gives an empty document, which `kubectl` ignores). A unit with no templates renders
//! nothing, and its generators don't run.

use std::fmt;
use std::path::Path;

use crate::ObeliskK8sError;
use crate::config::ephemeral::Ephemeral;
use crate::config::global::GlobalConfig;
use crate::config::paths::{Selection, write_yaml_cases};
use crate::config::repo::{RepoConfig, Target, Unit};
use crate::config::run::RunInputs;
use crate::config::values::{Agreed, shown_under};
use crate::helm;

/// One (target, unit) case's rendered resources.
#[derive(Debug)]
pub struct ManifestsCase<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// Every template's output, in merge order, as Helm printed it (empty if there are none).
    pub manifests: String,
    /// The release they render as: `None` if the unit has no templates (nothing was rendered, and
    /// nothing agreed on).
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
    /// The rendered resources of every case in `selection`. `shown_root` is as for
    /// [`RepoConfig::values_report`].
    ///
    /// # Errors
    ///
    /// Whatever building the generated values fails with; [`ObeliskK8sError::Helm`] if a template
    /// doesn't render.
    pub fn manifests_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
    ) -> Result<ManifestsReport<'a>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global)?;
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| self.manifests_for(repo_root, shown_root, target, unit, &inputs))
            .collect::<Result<_, _>>()?;
        Ok(ManifestsReport {
            cases,
            single: selection.single,
        })
    }

    pub(crate) fn manifests_for<'a>(
        &'a self,
        repo_root: &Path,
        shown_root: Option<&Path>,
        target: &'a Target,
        unit: &'a Unit,
        inputs: &RunInputs<'_>,
    ) -> Result<ManifestsCase<'a>, ObeliskK8sError> {
        let templates = self.resolve_templates(repo_root, target, unit)?;
        let mut manifests = String::new();
        let mut release = None;
        // Nothing to render: skip the generators too (each is a `helm` run).
        if templates.iter().any(|p| p.found) {
            let (generated, agreed) =
                self.generated_for(repo_root, shown_root, target, unit, inputs)?;
            let version = &agreed.version;
            if !helm::is_chart_version(version) {
                return Err(ObeliskK8sError::Usage(format!(
                    "version `{version}` for unit `{}` on target `{}` isn't SemVer, which Helm needs for a chart version: e.g. `0.0.0-{version}`, or a tag like `v1.2.3`",
                    unit.name, target.name
                )));
            }
            let values = &generated.yaml;
            for path in templates.iter().filter(|p| p.found) {
                let shown = shown_under(shown_root, &path.path);
                let rendered = helm::render(
                    &repo_root.join(&path.path),
                    &shown,
                    values,
                    &agreed.release_name,
                    &agreed.namespace,
                    Some(version),
                )?;
                // A template that renders nothing still gets a newline from Helm.
                if !rendered.trim().is_empty() {
                    manifests.push_str(&rendered);
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
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo};

    fn repo(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let mut files = vec![
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [./values.yaml]\n  generators: [{ type: gotmpl, path: ./gen.gotmpl }]\n  templates:\n    - { type: gotmpl, path: ./all.gotmpl }\n    - { type: gotmpl, path: \"./${{obelisk:target}}.gotmpl\" }\n",
            ),
            ("targets/prod/target.yaml", TARGET),
            ("targets/dev/target.yaml", TARGET),
            (
                "units/app/unit.yaml",
                "unit:\n  version: 1.0.0\n  templates: [{ type: gotmpl, path: ./app.gotmpl }]\n",
            ),
            ("values.yaml", "port: 8080\n"),
            (
                "gen.gotmpl",
                "url: \"http://{{ .Values.obelisk.unit }}:{{ .Values.port }}\"\n",
            ),
        ];
        files.extend_from_slice(extra);
        write_repo(&files)
    }

    fn manifests(dir: &tempfile::TempDir, targets: &[&str]) -> Result<String, ObeliskK8sError> {
        let config = RepoConfig::load(dir.path()).unwrap();
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        let selection = config.select(&targets, &["app".to_owned()]).unwrap();
        let text = config
            .manifests_report(dir.path(), &selection, None, None, &global())?
            .to_string();
        Ok(anstream::adapter::strip_str(&text).to_string())
    }

    const CONFIG_MAP: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Values.obelisk.unit }}\ndata:\n  url: {{ .Values.url | quote }}\n";

    #[test]
    fn renders_each_template_with_the_generated_values_in_merge_order() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[
            ("all.gotmpl", CONFIG_MAP),
            (
                "prod.gotmpl",
                "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: only-prod\n",
            ),
            (
                "app.gotmpl",
                "apiVersion: v1\nkind: Secret\nmetadata:\n  name: a\n---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: b\n",
            ),
        ]);
        let text = manifests(&dir, &["prod"]).unwrap();
        let sources: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("# Source: "))
            .collect();
        assert_eq!(
            sources,
            [
                "# Source: .obelisk-k8s/all.gotmpl",
                "# Source: .obelisk-k8s/prod.gotmpl",
                "# Source: .obelisk-k8s/app.gotmpl",
                "# Source: .obelisk-k8s/app.gotmpl",
            ]
        );
        // Templates see the generated values (`url` comes from the generator).
        assert!(text.contains("  url: \"http://app:8080\"\n"), "{text}");
        let docs: Vec<serde_json::Value> = serde_saphyr::from_str_multiple(&text).unwrap();
        assert_eq!(docs.len(), 4);
        // `dev` has no `dev.gotmpl`: skipped.
        assert!(!manifests(&dir, &["dev"]).unwrap().contains("only-prod"));
    }

    #[test]
    fn orders_templates_by_scope_across_levels() {
        if !helm_installed() {
            return;
        }
        let cm =
            |name: &str| format!("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {name}\n");
        let (unit_cm, unit_target_cm, target_cm) = (cm("unit"), cm("unit-target"), cm("target"));
        let dir = repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  templates:\n    - { type: gotmpl, path: \"./${{obelisk:unit}}-${{obelisk:target}}.gotmpl\" }\n    - { type: gotmpl, path: ./all.gotmpl }\n",
            ),
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  templates: [{ type: gotmpl, path: ./target.gotmpl }]\n  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\" }\n",
            ),
            (
                "units/app/unit.yaml",
                "unit:\n  version: 1.0.0\n  templates:\n    - { type: gotmpl, path: \"./${{obelisk:target}}-unit.gotmpl\" }\n    - { type: gotmpl, path: ./app.gotmpl }\n",
            ),
            ("all.gotmpl", CONFIG_MAP),
            ("target.gotmpl", &target_cm),
            ("app.gotmpl", &unit_cm),
            ("app-prod.gotmpl", &unit_target_cm),
            ("prod-unit.gotmpl", &unit_target_cm),
        ]);
        let text = manifests(&dir, &["prod"]).unwrap();
        let sources: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# Source: .obelisk-k8s/"))
            .collect();
        // repo < target < unit < target + unit; within a scope, repo.yaml < target.yaml < unit.yaml.
        assert_eq!(
            sources,
            [
                "all.gotmpl",
                "target.gotmpl",
                "app.gotmpl",
                "app-prod.gotmpl",
                "prod-unit.gotmpl"
            ]
        );
    }

    #[test]
    fn a_template_can_render_nothing() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[(
            "all.gotmpl",
            "{{- if false }}\nkind: ConfigMap\n{{- end }}\n",
        )]);
        assert_eq!(manifests(&dir, &["dev"]).unwrap(), "");
    }

    #[test]
    fn several_cases_are_a_stream_under_headers() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[("all.gotmpl", CONFIG_MAP)]);
        let text = manifests(&dir, &["dev", "prod"]).unwrap();
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
    fn a_bad_template_names_its_file() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[("all.gotmpl", "x: {{ .Values.nope.deeper }}\n")]);
        let e = manifests(&dir, &["dev"]).unwrap_err().to_string();
        assert!(
            e.starts_with(".obelisk-k8s/all.gotmpl: helm couldn't render it: "),
            "{e}"
        );
    }

    #[test]
    fn a_release_name_must_fit_helm() {
        // The repo's generator renders first, so helm is needed to get to the check.
        if !helm_installed() {
            return;
        }
        let long = format!(
            "target:\n  cluster: c\n  release: {{ name: {}, namespace: n }}\n",
            "a".repeat(54)
        );
        let dir = repo(&[
            ("targets/dev/target.yaml", &long),
            ("all.gotmpl", CONFIG_MAP),
        ]);
        let error = manifests(&dir, &["dev"]).unwrap_err().to_string();
        assert!(
            error.contains("is too long: Helm release names are at most 53 characters"),
            "{error}"
        );
    }

    #[test]
    fn the_chart_carries_the_version() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[(
            "all.gotmpl",
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\ndata:\n  chart: {{ .Chart.Version | quote }}\n  app: {{ .Chart.AppVersion | quote }}\n  values: {{ .Values.obelisk.version | quote }}\n",
        )]);
        let text = manifests(&dir, &["dev"]).unwrap();
        assert!(
            text.contains("  chart: \"1.0.0\"\n  app: \"1.0.0\"\n  values: \"1.0.0\"\n"),
            "{text}"
        );
    }

    #[test]
    fn templates_render_as_the_agreed_release() {
        if !helm_installed() {
            return;
        }
        let dir = repo(&[(
            "all.gotmpl",
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n  namespace: {{ .Release.Namespace }}\ndata:\n  release: {{ .Release.Name | quote }}\n",
        )]);
        let text = manifests(&dir, &["dev"]).unwrap();
        assert!(
            text.contains("  namespace: dev\ndata:\n  release: \"app\"\n"),
            "{text}"
        );
    }

    #[test]
    fn a_version_must_be_semver_to_render_templates() {
        // Checked before `helm` runs.
        let dir = repo(&[
            ("all.gotmpl", CONFIG_MAP),
            (
                "units/app/unit.yaml",
                "unit:\n  version: b84d8f6\n  templates: [{ type: gotmpl, path: ./app.gotmpl }]\n",
            ),
            (
                "repo.yaml",
                "repo:\n  name: r\n  templates: [{ type: gotmpl, path: ./all.gotmpl }]\n",
            ),
        ]);
        let e = manifests(&dir, &["dev"]).unwrap_err().to_string();
        assert_eq!(
            e,
            "version `b84d8f6` for unit `app` on target `dev` isn't SemVer, which Helm needs for a chart version: e.g. `0.0.0-b84d8f6`, or a tag like `v1.2.3`"
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
                "unit:\n  version: \"1.0\"\n  templates: [{ type: gotmpl, path: ./app.gotmpl }]\n",
            ),
            (
                "app.gotmpl",
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\ndata:\n  chart: {{ .Chart.Version | quote }}\n",
            ),
        ]);
        let text = manifests(&dir, &["dev"]).unwrap();
        assert!(text.contains("  chart: \"1.0\"\n"), "{text}");
    }
}
