//! What runs a resolved env (see `config::env`): `env template`, `env diff`, `env apply` and
//! `env down`.
//!
//! Each unit renders from its own checkout as its unit on its target, exactly as `template` /
//! `diff` / `apply` would render it there, with what the env adds:
//!
//! - `obelisk.env: { name, blueprint, repo }` and `obelisk.paths.blueprint` (the directory holding
//!   `blueprint.yaml`, in the env's checkout), so a build file can take values from the blueprint
//!   repo;
//! - for an ephemeral blueprint, the copy `env.ephemeral`, with no ephemeral values (tentative:
//!   values come from the blueprint repo);
//! - the release labels `obelisk-k8s/env` and `obelisk-k8s/blueprint`.
//!
//! `diff` plans every unit before touching a cluster (two units that would be the same release
//! are an error). `apply` first finds the env's releases whose unit left its blueprint (labelled
//! with the env, on any local cluster, but whose `repo/target/unit` labels name no unit of it: they
//! need `--prune` or `--no-prune`, decided before anything changes; remote clusters aren't looked
//! on, for now, which is a warning) and uninstalls what it prunes. Then it goes unit by unit,
//! dependencies first, planning each only once what it depends on is installed (a unit's build may
//! call another's API, e.g. for an app token): plan, check no other unit is the same release, check
//! no release it'd install belongs to someone else (another env, or a plain `apply`), build and
//! deliver its images (TODO, fast-follow: a tag already built locally isn't rebuilt, which trusts
//! the tag to name its contents; see `images_push`), install. Last, it removes the checkouts it no
//! longer pins. So a unit that fails to render stops `apply` after the units before it are in.
//!
//! `env down` undoes an env, even one whose file is gone: it uninstalls every release labelled
//! with it (on the local clusters, for now) and removes its checkouts. The env's file stays.

use std::collections::HashSet;
use std::fmt;
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::ObeliskK8sError;
use crate::config::apply::{
    self, ApplyReport, ENV, Orphan, Planned, Prune, Takeover, check_owners, check_unique,
    decide_prune, is_local,
};
use crate::config::diff::{self, DiffOptions, DiffReport};
use crate::config::env::{EnvInstance, EnvPlan, EnvUnit};
use crate::config::ephemeral::{Ephemeral, Instance};
use crate::config::global::GlobalConfig;
use crate::config::paths::{CASE_HEADER, Selection};
use crate::config::repo::RepoConfig;
use crate::helm;
use crate::ssm::Ssm;

/// Which `template` stage to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvTemplate {
    Build,
    Values,
    Manifests,
}

/// What an env's units render with: the env as they see it, and its copy (its `ephemeral` name,
/// without values).
pub(crate) struct Rendering {
    env: EnvInstance,
    copy: Option<Ephemeral>,
}

impl Rendering {
    fn instance(&self) -> Instance<'_> {
        Instance {
            ephemeral: self.copy.as_ref(),
            env: Some(&self.env),
        }
    }
}

impl EnvPlan {
    pub(crate) fn rendering(&self) -> Rendering {
        Rendering {
            env: self.instance(),
            copy: self.env.ephemeral.as_ref().map(|name| {
                Ephemeral::without_values(
                    name.clone(),
                    PathBuf::from(format!("env `{}`", self.env.name)),
                )
            }),
        }
    }

    /// `unit`'s config (loaded from its checkout while resolving), and its one case.
    fn case_of(&self, unit: &EnvUnit) -> Result<(&RepoConfig, Selection<'_>), ObeliskK8sError> {
        let config = &self.configs[&unit.at.checkout];
        Ok((config, config.select_one(&unit.id.target, &unit.id.unit)?))
    }

    /// Every unit planned (rendered and checked, as `apply` plans), dependencies first, before any
    /// is installed (`diff`); two units that would be the same release are an error.
    pub(crate) fn plan_all<'a>(
        &'a self,
        rendering: &Rendering,
        global: &GlobalConfig,
        remote_ok: bool,
        ssm: &Ssm,
        warn: &mut dyn FnMut(&str),
    ) -> Result<Vec<Planned<'a>>, ObeliskK8sError> {
        let mut all: Vec<Planned<'a>> = Vec::new();
        for unit in &self.units {
            let (config, selection) = self.case_of(unit)?;
            let planned = config.plan(
                &unit.at.checkout,
                &selection,
                None,
                rendering.instance(),
                global,
                remote_ok,
                ssm,
                &mut *warn,
            )?;
            for p in planned {
                if let Some(release) = &p.case.release {
                    check_unique(&all, &p.case, release, &p.context)?;
                }
                all.push(p);
            }
        }
        Ok(all)
    }

    /// What `env template <stage>` prints: each unit's, dependencies first, under a magenta
    /// `# [ env=…, unit=repo/target/unit ]` header.
    ///
    /// # Errors
    ///
    /// Whatever rendering the stage fails with.
    pub fn template(
        &self,
        global: &GlobalConfig,
        stage: EnvTemplate,
        mut warn: impl FnMut(&str),
    ) -> Result<String, ObeliskK8sError> {
        let rendering = self.rendering();
        let instance = rendering.instance();
        let mut out = String::new();
        for unit in &self.units {
            let (config, selection) = self.case_of(unit)?;
            let root = &unit.at.checkout;
            let text = match stage {
                EnvTemplate::Build => config
                    .build_report(root, &selection, None, instance, global, &mut warn)?
                    .to_string(),
                EnvTemplate::Values => config
                    .values_report(root, &selection, None, instance, global, None, &mut warn)?
                    .to_string(),
                EnvTemplate::Manifests => config
                    .manifests_report(root, &selection, None, instance, global, None, &mut warn)?
                    .to_string(),
            };
            let _ = write!(
                out,
                "# {CASE_HEADER}[ env={}, unit={} ]{CASE_HEADER:#}\n{text}",
                self.env.name, unit.id
            );
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// What applying would change, unit by unit.
    ///
    /// # Errors
    ///
    /// As [`RepoConfig::diff`].
    pub fn diff(
        &self,
        global: &GlobalConfig,
        options: DiffOptions,
        ssm: &Ssm,
        mut warn: impl FnMut(&str),
        mut progress: impl FnMut(&str),
    ) -> Result<DiffReport<'_>, ObeliskK8sError> {
        helm::check_diff_plugin()?;
        let rendering = self.rendering();
        let plan = self.plan_all(&rendering, global, true, ssm, &mut warn)?;
        Ok(DiffReport {
            diffs: diff::diff_planned(plan, options, &mut progress)?,
        })
    }

    /// Applies the env (see the module docs).
    ///
    /// # Errors
    ///
    /// As [`RepoConfig::apply`]; [`ObeliskK8sError::Usage`] if a release it would install belongs
    /// to someone else, or releases of units that left the blueprint remain and `prune` is
    /// [`Prune::Unset`]. An env on a remote cluster is a warning (its releases there aren't looked
    /// for).
    pub fn apply(
        &self,
        global: &GlobalConfig,
        remote_ok: bool,
        ssm: &Ssm,
        prune: Prune,
        mut warn: impl FnMut(&str),
        mut progress: impl FnMut(&str),
    ) -> Result<ApplyReport<'_>, ObeliskK8sError> {
        if let Some(cluster) = self.cluster.as_deref()
            && !is_local(global, cluster)
        {
            warn(&format!(
                "env `{}` deploys to remote cluster `{cluster}`: releases of units that left its blueprint aren't looked for there (local clusters only, for now)",
                self.env.name
            ));
        }
        // Decided before anything changes: what's left of units that left the blueprint.
        // `--no-prune` leaves them, so there's no need to look.
        let orphans = if prune == Prune::Keep {
            Vec::new()
        } else {
            let ids: HashSet<String> = self.units.iter().map(|u| u.id.to_string()).collect();
            env_orphans(&self.env.name, global, &ids, &mut warn)?
        };
        let whose = format!("units that left env `{}`'s blueprint", self.env.name);
        let doomed = decide_prune(orphans, prune, &whose)?;
        let pruned = apply::uninstall(doomed, &mut progress)?;

        // Unit by unit, dependencies first: each planned only once what it depends on is
        // installed (its build may call it: e.g. an app token from another unit's API).
        let rendering = self.rendering();
        let mut releases: HashSet<(String, String, String)> = HashSet::new();
        let mut applied = Vec::new();
        for unit in &self.units {
            let (config, selection) = self.case_of(unit)?;
            progress(&format!("planning unit `{}`...", unit.id));
            let plan = config.plan(
                &unit.at.checkout,
                &selection,
                None,
                rendering.instance(),
                global,
                remote_ok,
                ssm,
                &mut warn,
            )?;
            for p in &plan {
                if let Some(r) = &p.case.release {
                    let key = (
                        p.context.clone(),
                        r.release_name.clone(),
                        r.namespace.clone(),
                    );
                    if !releases.insert(key) {
                        return Err(ObeliskK8sError::Usage(format!(
                            "unit `{}` would be release `{}` in namespace `{}` on `{}`, which an earlier unit of env `{}` already is: give them different release names or namespaces",
                            unit.id, r.release_name, r.namespace, p.context, self.env.name
                        )));
                    }
                }
            }
            check_owners(&plan, Some(&self.env.name))?;
            config.images_push(
                &unit.at.checkout,
                &selection,
                None,
                rendering.instance(),
                global,
                &[],
                ssm,
                true,
                &mut warn,
                &mut progress,
            )?;
            applied.extend(apply::install(plan, Takeover::default(), &mut progress)?);
        }
        global.remove_checkouts_except(&self.env.name, &self.checkouts(), &mut progress)?;
        Ok(ApplyReport { pruned, applied })
    }
}

/// Env `env`'s releases (labelled with it) on every local cluster whose unit (its
/// `obelisk-k8s/{repo,target,unit}` labels) isn't one of `units` (as `repo/target/unit`): it left
/// the blueprint. A cluster it can't look on (e.g. stopped) is a warning.
fn env_orphans(
    env: &str,
    global: &GlobalConfig,
    units: &HashSet<String>,
    warn: &mut dyn FnMut(&str),
) -> Result<Vec<Orphan>, ObeliskK8sError> {
    let mut orphans = Vec::new();
    for (context, listed) in env_releases(env, global, warn) {
        let labels = helm::release_labels(&context, &listed)?;
        let orphan = Orphan::of(listed, labels.as_ref(), &context);
        if left_blueprint(&orphan, units) {
            orphans.push(orphan);
        }
    }
    Ok(orphans)
}

/// Whether `release`'s unit (from its labels) isn't one of `units`; one whose labels don't say is
/// taken as gone (it's the env's, but no unit of it now).
fn left_blueprint(release: &Orphan, units: &HashSet<String>) -> bool {
    !units.contains(&format!(
        "{}/{}/{}",
        release.repo, release.target, release.unit
    ))
}

/// Every release labelled with env `env` on each local cluster with a kubectl context, with that
/// context; a cluster it can't look on (e.g. stopped) is a warning, and so are the remote clusters,
/// which aren't looked on (local clusters only, for now).
fn env_releases(
    env: &str,
    global: &GlobalConfig,
    warn: &mut dyn FnMut(&str),
) -> Vec<(String, helm::Listed)> {
    let selector = format!("{ENV}={env}");
    let remote: Vec<&str> = global
        .clusters
        .iter()
        .filter(|c| !is_local(global, &c.name))
        .map(|c| c.name.as_str())
        .collect();
    if !remote.is_empty() {
        warn(&format!(
            "env `{env}`'s releases aren't looked for on remote clusters ({}): local clusters only, for now",
            remote.join(", ")
        ));
    }
    let mut found = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for cluster in global.clusters.iter().filter(|c| is_local(global, &c.name)) {
        // Two clusters reached through one context are one place to look (else a release there
        // would be listed, and uninstalled, twice).
        let Some(context) = cluster.context().filter(|c| seen.insert(c.clone())) else {
            continue;
        };
        match helm::list_releases(&context, &selector) {
            Ok(listed) => found.extend(listed.into_iter().map(|l| (context.clone(), l))),
            Err(error) => warn(&format!(
                "couldn't look for env `{env}`'s releases on cluster `{}`: {error}",
                cluster.name
            )),
        }
    }
    found
}

/// What `env down` did.
#[derive(Debug, Default)]
pub struct EnvDownReport {
    /// `(release, namespace, context)`.
    pub uninstalled: Vec<(String, String, String)>,
    pub removed: Vec<PathBuf>,
    /// Clusters it couldn't look on (e.g. stopped), and why.
    pub warnings: Vec<String>,
}

impl fmt::Display for EnvDownReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (release, namespace, context) in &self.uninstalled {
            writeln!(
                f,
                "uninstalled release `{release}` in namespace `{namespace}` on `{context}`"
            )?;
        }
        for dir in &self.removed {
            writeln!(f, "removed checkout {}", dir.display())?;
        }
        if self.uninstalled.is_empty() && self.removed.is_empty() {
            writeln!(f, "nothing to take down")?;
        }
        Ok(())
    }
}

impl GlobalConfig {
    /// Takes env `name` down (see the module docs): uninstalls its releases on every local
    /// cluster with a kubectl context (one it can't look on is a warning), then removes its
    /// checkouts. Its file, if there is one, stays: `env apply` brings it back.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if `name` isn't a valid name; [`ObeliskK8sError::Helm`] if an
    /// uninstall fails; and whatever removing a checkout fails with.
    pub fn env_down(
        &self,
        name: &str,
        progress: &mut dyn FnMut(&str),
    ) -> Result<EnvDownReport, ObeliskK8sError> {
        if !crate::config::is_valid_name(name) {
            return Err(ObeliskK8sError::Usage(format!(
                "`{name}` isn't a valid env name ({})",
                crate::config::NAME_RULE
            )));
        }
        let mut report = EnvDownReport::default();
        let mut warnings = Vec::new();
        let releases = env_releases(name, self, &mut |w| warnings.push(w.to_owned()));
        report.warnings = warnings;
        for (context, release) in releases {
            progress(&format!(
                "uninstalling release `{}` in namespace `{}` on `{context}`...",
                release.name, release.namespace
            ));
            helm::uninstall(&context, &release)?;
            report
                .uninstalled
                .push((release.name, release.namespace, context));
        }
        report.removed = self.remove_checkouts_except(name, &HashSet::new(), progress)?;
        let dir = crate::checkout::env_dir(&self.dir, name);
        if dir.is_dir() && crate::config::env::read_dir(&dir)?.is_empty() {
            std::fs::remove_dir(&dir).map_err(|source| ObeliskK8sError::Io {
                path: dir.clone(),
                source,
            })?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(repo: &str, target: &str, unit: &str) -> Orphan {
        Orphan {
            repo: repo.to_owned(),
            target: target.to_owned(),
            unit: unit.to_owned(),
            release: unit.to_owned(),
            namespace: "ns".to_owned(),
            context: "k3d-c".to_owned(),
        }
    }

    #[test]
    fn a_release_is_gone_when_its_unit_left_the_blueprint() {
        let units: HashSet<String> = ["app/local/a".to_owned()].into();
        assert!(!left_blueprint(&release("app", "local", "a"), &units));
        assert!(left_blueprint(&release("app", "local", "b"), &units));
        assert!(
            left_blueprint(&release("app", "other", "a"), &units),
            "moved target"
        );
        // Labels that don't say which unit: the env's, but no unit of it now.
        assert!(left_blueprint(&release("?", "?", "?"), &units));
    }
}
