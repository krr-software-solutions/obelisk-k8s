//! `apply`: installs what `template manifests` renders, one Helm release per unit on a target.
//!
//! Each case's manifests go in as its agreed release (`obelisk.release.{name, namespace}`, the
//! namespace created if it's missing), at its agreed version, through a throwaway chart that ships
//! them as-is (no second templating), with `helm upgrade --install` on its target's cluster's kubectl
//! context: k3d's own for a local k3d cluster, else the cluster's `kubectlContext`. A release that
//! doesn't come up is rolled back (unless it takes ownership; see below). A unit with no templates
//! has nothing to apply, and is skipped.
//!
//! Everything is planned first (clusters resolved, every case rendered and checked), so a mistake in
//! one case doesn't leave the others half-applied: two cases that would install the same release on
//! the same context (the second would replace the first, deleting what it had), and a case whose
//! templates render nothing (an upgrade to it would delete everything), are errors. Applying to a
//! remote cluster needs `yes`. If an install fails, the ones before it stay applied.
//!
//! What's already in the cluster but not a release's (e.g. applied by obelisk-local) is only adopted
//! with [`Takeover::take_ownership`], and fields another field manager owns are only taken with
//! [`Takeover::force_conflicts`]: both one-offs, for moving resources over.

use std::fmt;
use std::path::Path;

use crate::ObeliskK8sError;
use crate::config::ephemeral::Ephemeral;
use crate::config::global::{Cluster, GlobalConfig, Location};
use crate::config::manifests::ManifestsCase;
use crate::config::paths::Selection;
use crate::config::paths::Source;
use crate::config::repo::{RepoConfig, Target};
use crate::config::run::RunInputs;
use crate::config::values::Agreed;
use crate::helm;
pub use crate::helm::Takeover;
use crate::ssm::Ssm;

/// What `apply` did with one case.
#[derive(Debug)]
pub struct Applied<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// Where it went; `None` if the unit has no templates (nothing to apply).
    pub release: Option<Agreed>,
    pub context: String,
}

/// What `apply` did, case by case.
#[derive(Debug)]
pub struct ApplyReport<'a>(pub Vec<Applied<'a>>);

impl fmt::Display for ApplyReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for applied in &self.0 {
            let (target, unit) = (applied.target, applied.unit);
            match &applied.release {
                Some(release) => writeln!(
                    f,
                    "applied unit `{unit}` on target `{target}`: release `{}` (version `{}`) in namespace `{}` on `{}`",
                    release.release_name, release.version, release.namespace, applied.context
                )?,
                None => writeln!(
                    f,
                    "skipped unit `{unit}` on target `{target}`: it has no templates"
                )?,
            }
        }
        Ok(())
    }
}

/// One case, ready to install.
#[derive(Debug)]
struct Planned<'a> {
    case: ManifestsCase<'a>,
    context: String,
}

impl RepoConfig {
    /// Applies every case in `selection` to the clusters in `global` (remote ones only with
    /// `remote_ok`), taking over what `takeover` allows: plans them all, then installs each.
    /// `progress` hears about each install as it starts.
    ///
    /// # Errors
    ///
    /// Whatever the ephemeral checks or rendering fail with; [`ObeliskK8sError::Usage`] if a
    /// target's cluster isn't in `global`, has no kubectl context, or is remote without
    /// `remote_ok`, two cases would install the same release, or a case renders nothing;
    /// [`ObeliskK8sError::Helm`] if an install fails (the cases before it stay applied).
    #[allow(clippy::too_many_arguments)]
    pub fn apply<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        remote_ok: bool,
        takeover: Takeover,
        ssm: &Ssm,
        mut progress: impl FnMut(&str),
    ) -> Result<ApplyReport<'a>, ObeliskK8sError> {
        let plan = self.plan(
            repo_root, selection, shown_root, ephemeral, global, remote_ok, ssm,
        )?;
        let mut applied = Vec::new();
        for Planned { case, context } in plan {
            if let Some(release) = &case.release {
                progress(&format!(
                    "applying unit `{}` on target `{}` to `{context}`...",
                    case.unit, case.target
                ));
                helm::install_manifests(
                    &release.release_name,
                    &release.version,
                    &case.manifests,
                    &release.namespace,
                    &context,
                    takeover,
                )
                .map_err(|error| takeover_hint(takeover, error))?;
            }
            applied.push(Applied {
                target: case.target,
                unit: case.unit,
                release: case.release,
                context,
            });
        }
        Ok(ApplyReport(applied))
    }

    /// Every case in `selection`, rendered and checked, with the context to install it on.
    #[allow(clippy::too_many_arguments)]
    fn plan<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        remote_ok: bool,
        ssm: &Ssm,
    ) -> Result<Vec<Planned<'a>>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, ephemeral, global, Some(ssm))?;
        let mut contexts = Vec::new();
        for (target, _) in &selection.cases {
            let cluster = inputs.cluster(target)?;
            let context = context_for(target, cluster)?;
            check_remote(target, cluster, remote_ok)?;
            contexts.push(context);
        }
        let mut plan: Vec<Planned<'a>> = Vec::new();
        let mut capabilities: Vec<(String, helm::Capabilities)> = Vec::new();
        for ((target, unit), context) in selection.cases.iter().zip(contexts) {
            // A chart renders for the cluster it goes to: ask each such cluster once.
            let has_chart = self
                .resolve_templates(repo_root, target, unit)?
                .iter()
                .any(|t| matches!(t.source, Source::Helm(_)));
            if has_chart && !capabilities.iter().any(|(c, _)| *c == context) {
                capabilities.push((context.clone(), helm::capabilities(&context)?));
            }
            let found = capabilities
                .iter()
                .find(|(c, _)| has_chart && *c == context)
                .map(|(_, caps)| caps);
            let case = self.manifests_for(repo_root, shown_root, target, unit, &inputs, found)?;
            if let Some(release) = &case.release {
                check_renders_something(&case, release)?;
                check_unique(&plan, &case, release, &context)?;
            }
            plan.push(Planned { case, context });
        }
        Ok(plan)
    }
}

/// The kubectl context to apply `target` with, on `cluster`.
fn context_for(target: &Target, cluster: &Cluster) -> Result<String, ObeliskK8sError> {
    cluster.context().ok_or_else(|| {
        ObeliskK8sError::Usage(format!(
            "target `{}` deploys to cluster `{}`, which has no `kubectlContext` (in {}): set one to apply to it",
            target.name,
            cluster.name,
            cluster.file.display()
        ))
    })
}

/// A remote cluster takes `remote_ok` (`--yes`): applying there is the costly mistake.
fn check_remote(
    target: &Target,
    cluster: &Cluster,
    remote_ok: bool,
) -> Result<(), ObeliskK8sError> {
    if cluster.location == Location::Remote && !remote_ok {
        return Err(ObeliskK8sError::Usage(format!(
            "target `{}` deploys to remote cluster `{}`: pass `--yes` to apply to it",
            target.name, cluster.name
        )));
    }
    Ok(())
}

/// Templates that render nothing would install an empty release, and an upgrade to one deletes
/// everything the release had.
fn check_renders_something(
    case: &ManifestsCase<'_>,
    release: &Agreed,
) -> Result<(), ObeliskK8sError> {
    if case.manifests.trim().is_empty() {
        return Err(ObeliskK8sError::Usage(format!(
            "unit `{}` on target `{}` renders no resources: applying it would empty release `{}` in `{}` (to remove a release, use `helm uninstall`)",
            case.unit, case.target, release.release_name, release.namespace
        )));
    }
    Ok(())
}

/// One release per case: a second case installing the same release on the same context would
/// replace the first.
fn check_unique(
    plan: &[Planned<'_>],
    case: &ManifestsCase<'_>,
    release: &Agreed,
    context: &str,
) -> Result<(), ObeliskK8sError> {
    let same = plan.iter().find(|p| {
        p.context == context
            && p.case.release.as_ref().is_some_and(|r| {
                r.release_name == release.release_name && r.namespace == release.namespace
            })
    });
    if let Some(other) = same {
        return Err(ObeliskK8sError::Usage(format!(
            "unit `{}` on target `{}` and unit `{}` on target `{}` would both be release `{}` in namespace `{}` on `{context}`: give them different release names or namespaces",
            other.case.unit,
            other.case.target,
            case.unit,
            case.target,
            release.release_name,
            release.namespace
        )));
    }
    Ok(())
}

/// Points a refusal `apply`'s [`Takeover`] flags would get past at the flag, unless it's already on.
fn takeover_hint(takeover: Takeover, error: ObeliskK8sError) -> ObeliskK8sError {
    match error {
        ObeliskK8sError::Helm(message) => {
            let hint = if !takeover.take_ownership && message.contains("invalid ownership metadata")
            {
                " (it's already in the cluster, but not this release's: `--take-ownership` adopts it)"
            } else if !takeover.force_conflicts
                && message.contains("Apply failed with")
                && message.contains("conflict")
            {
                " (another field manager owns those fields: `--force-conflicts` takes them)"
            } else {
                ""
            };
            ObeliskK8sError::Helm(format!("{message}{hint}"))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{helm_installed, write_repo};

    /// A global config with `clusters`, as `(name, cluster.yaml body)`.
    fn global(clusters: &[(&str, &str)]) -> (tempfile::TempDir, GlobalConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("clusters")).unwrap();
        std::fs::write(dir.path().join("repos.yaml"), "repos: []\n").unwrap();
        for (name, body) in clusters {
            let path = dir.path().join(format!("clusters/{name}/cluster.yaml"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("cluster:\n{body}")).unwrap();
        }
        let config = GlobalConfig::load(dir.path(), None).unwrap();
        (dir, config)
    }

    const CLUSTERS: [(&str, &str); 3] = [
        ("k", "  k8s: k3d\n  location: local\n  managed: true\n"),
        (
            "r",
            "  k8s: talos\n  location: remote\n  managed: false\n  kubectlContext: admin@r\n",
        ),
        ("b", "  k8s: talos\n  location: local\n  managed: false\n"),
    ];

    const RELEASE: &str =
        "  release: { name: \"${{obelisk:unit}}\", namespace: \"${{obelisk:target}}\" }\n";
    const CONFIG_MAP: &str =
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Release.Name }}\n";

    /// Targets `dev` (on local k3d `k`), `prod` (remote `r`), `lost` (cluster not in the global
    /// config) and `blind` (no context); unit `app` with template `app.gotmpl`; `extra` on top.
    fn repo(extra: &[(&str, &str)]) -> (tempfile::TempDir, RepoConfig) {
        let target = |cluster: &str| format!("target:\n  cluster: {cluster}\n{RELEASE}");
        let (dev, prod, lost, blind) = (target("k"), target("r"), target("nope"), target("b"));
        let mut files = vec![
            ("repo.yaml", "repo:\n  name: r\n"),
            ("targets/dev/target.yaml", dev.as_str()),
            ("targets/prod/target.yaml", prod.as_str()),
            ("targets/lost/target.yaml", lost.as_str()),
            ("targets/blind/target.yaml", blind.as_str()),
            (
                "units/app/unit.yaml",
                "unit:\n  version: v1\n  templates: [{ type: gotmpl, path: ./app.gotmpl }]\n",
            ),
            ("app.gotmpl", CONFIG_MAP),
        ];
        files.retain(|(p, _)| !extra.iter().any(|(q, _)| q == p));
        files.extend_from_slice(extra);
        let dir = write_repo(&files);
        let config = RepoConfig::load(dir.path()).unwrap();
        (dir, config)
    }

    fn plan_for<'a>(
        dir: &tempfile::TempDir,
        config: &'a RepoConfig,
        global: &GlobalConfig,
        targets: &[&str],
        remote_ok: bool,
    ) -> Result<Vec<Planned<'a>>, ObeliskK8sError> {
        let targets: Vec<String> = targets.iter().map(|t| (*t).to_owned()).collect();
        let selection = config.select(&targets, &[]).unwrap();
        let ssm = Ssm::with_fetch(Box::new(|_| panic!("no refs here")));
        config.plan(dir.path(), &selection, None, None, global, remote_ok, &ssm)
    }

    #[test]
    fn plans_each_case_as_its_release_on_its_clusters_context() {
        if !helm_installed() {
            return;
        }
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[]);
        let plan = plan_for(&dir, &config, &global, &["dev", "prod"], true).unwrap();
        let summary: Vec<(String, Agreed)> = plan
            .iter()
            .map(|p| (p.context.clone(), p.case.release.clone().unwrap()))
            .collect();
        let release = |namespace: &str| Agreed {
            release_name: "app".to_owned(),
            namespace: namespace.to_owned(),
            version: "v1".to_owned(),
        };
        assert_eq!(
            summary,
            [
                ("k3d-k".to_owned(), release("dev")),
                ("admin@r".to_owned(), release("prod")),
            ]
        );
        assert!(
            plan[0].case.manifests.contains("  name: app\n"),
            "{}",
            plan[0].case.manifests
        );
    }

    #[test]
    fn a_remote_cluster_needs_yes_before_anything_renders() {
        // A template that can't render: getting past the check would be a Helm error.
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[("app.gotmpl", "{{ nope }}\n")]);
        assert_eq!(
            plan_for(&dir, &config, &global, &["dev", "prod"], false)
                .unwrap_err()
                .to_string(),
            "target `prod` deploys to remote cluster `r`: pass `--yes` to apply to it"
        );
        if helm_installed() {
            let error = plan_for(&dir, &config, &global, &["prod"], true).unwrap_err();
            assert!(matches!(error, ObeliskK8sError::Helm(_)), "{error}");
        }
    }

    #[test]
    fn a_target_needs_a_known_cluster_with_a_context() {
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[]);
        let error = |target| {
            plan_for(&dir, &config, &global, &[target], true)
                .unwrap_err()
                .to_string()
        };
        assert!(
            error("lost").starts_with(
                "target `lost` deploys to cluster `nope`, which isn't in the global config"
            ),
            "{}",
            error("lost")
        );
        assert!(
            error("blind").starts_with(
                "target `blind` deploys to cluster `b`, which has no `kubectlContext`"
            ),
            "{}",
            error("blind")
        );
    }

    #[test]
    fn two_cases_cant_be_one_release() {
        if !helm_installed() {
            return;
        }
        // Both units on `dev` agree on release `shared` (from the target).
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[
            (
                "targets/dev/target.yaml",
                "target:\n  cluster: k\n  release: { name: shared, namespace: ns }\n",
            ),
            (
                "units/other/unit.yaml",
                "unit:\n  version: v1\n  templates: [{ type: gotmpl, path: ./app.gotmpl }]\n",
            ),
        ]);
        assert_eq!(
            plan_for(&dir, &config, &global, &["dev"], false)
                .unwrap_err()
                .to_string(),
            "unit `app` on target `dev` and unit `other` on target `dev` would both be release `shared` in namespace `ns` on `k3d-k`: give them different release names or namespaces"
        );
    }

    #[test]
    fn a_case_must_render_something() {
        if !helm_installed() {
            return;
        }
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[("app.gotmpl", "{{- /* nothing */ -}}\n")]);
        let error = plan_for(&dir, &config, &global, &["dev"], false)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("unit `app` on target `dev` renders no resources"),
            "{error}"
        );
    }

    #[test]
    fn a_unit_without_templates_is_skipped() {
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[("units/app/unit.yaml", "unit:\n  version: v1\n")]);
        let selection = config.select(&["dev".to_owned()], &[]).unwrap();
        let report = config
            .apply(
                dir.path(),
                &selection,
                None,
                None,
                &global,
                false,
                Takeover::default(),
                &Ssm::with_fetch(Box::new(|_| panic!("no refs here"))),
                |_| {},
            )
            .unwrap();
        assert_eq!(
            report.to_string(),
            "skipped unit `app` on target `dev`: it has no templates\n"
        );
    }

    #[test]
    fn a_refusal_the_takeover_flags_would_pass_points_at_them() {
        let hinted_with = |takeover, message: &str| match takeover_hint(
            takeover,
            ObeliskK8sError::Helm(message.to_owned()),
        ) {
            ObeliskK8sError::Helm(message) => message,
            other => panic!("{other:?}"),
        };
        let hinted = |message: &str| hinted_with(Takeover::default(), message);
        assert!(
            hinted("x exists and cannot be imported: invalid ownership metadata")
                .ends_with("`--take-ownership` adopts it)")
        );
        assert!(
            hinted("Apply failed with 1 conflict: conflict with \"kubectl\": .spec.replicas")
                .ends_with("`--force-conflicts` takes them)")
        );
        assert_eq!(hinted("timed out"), "timed out");
        let both = Takeover {
            take_ownership: true,
            force_conflicts: true,
        };
        let ownership = "x exists and cannot be imported: invalid ownership metadata";
        assert_eq!(hinted_with(both, ownership), ownership);
        let conflict = "Apply failed with 1 conflict: conflict with \"kubectl\": .spec.replicas";
        assert_eq!(hinted_with(both, conflict), conflict);
    }
}
