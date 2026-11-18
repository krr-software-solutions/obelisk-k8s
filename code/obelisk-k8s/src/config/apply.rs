//! `apply`: installs what `template manifests` renders, one Helm release per unit on a target.
//!
//! Each case's manifests go in as its agreed release (`obelisk.release.{name, namespace}`, the
//! namespace created if it's missing), at its agreed version (`obelisk.release.version`), through a
//! throwaway chart that ships them as-is (no second templating), with `helm upgrade --install` on its
//! target's cluster's kubectl context: k3d's own for a local k3d cluster, else the cluster's `kubectlContext`. A release that
//! doesn't come up is rolled back (unless it takes ownership; see below). A unit with no manifests
//! sources has nothing to apply, and is skipped.
//!
//! Everything is planned first (clusters resolved, every case rendered and checked), so a mistake in
//! one case doesn't leave the others half-applied: two cases that would install the same release on
//! the same context (the second would replace the first, deleting what it had), and a case whose
//! manifests render nothing (an upgrade to it would delete everything), are errors. Applying to a
//! remote cluster needs `yes`. The build plans' warnings are handed on as they're planned, so before
//! anything installs. If an install fails, the ones before it stay applied.
//!
//! Every release is labelled (`obelisk-k8s/{repo, target, unit, ephemeral, sha}`; see
//! [`release_labels`]), so a release whose unit is gone can be found: one labelled with this repo
//! and a selected target (and the same ephemeral copy, or none) whose unit is no longer in the
//! repo for that target. When there are any, `apply` needs a [`Prune`] choice: uninstall them
//! first, or leave them. For now this is only for local clusters: remote ones aren't looked at
//! (with a choice, each selected one is a warning saying so).
//!
//! What's already in the cluster but not a release's (e.g. applied by obelisk-local) is only adopted
//! with [`Takeover::take_ownership`], and fields another field manager owns are only taken with
//! [`Takeover::force_conflicts`]: both one-offs, for moving resources over.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use crate::ObeliskK8sError;
use crate::config::build::{SourceKind, Warnings};
use crate::config::env::EnvInstance;
use crate::config::ephemeral::Instance;
use crate::config::global::{Cluster, GlobalConfig, Location};
use crate::config::manifests::ManifestsCase;
use crate::config::paths::{NOTHING_SELECTED, Selection};
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
    /// Where it went; `None` if the unit has no manifests sources (nothing to apply).
    pub release: Option<Agreed>,
    pub context: String,
}

/// What `apply` does with the releases of units that are gone (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prune {
    /// Neither said: fine while there are none, an error once there are.
    #[default]
    Unset,
    /// `--prune`: uninstall them, before installing anything.
    Uninstall,
    /// `--no-prune`: leave them (without even looking for them).
    Keep,
}

impl Prune {
    /// From `--prune` and `--no-prune` (which the CLI keeps from both being given).
    #[must_use]
    pub fn from_flags(prune: bool, no_prune: bool) -> Self {
        match (prune, no_prune) {
            (true, _) => Self::Uninstall,
            (_, true) => Self::Keep,
            _ => Self::Unset,
        }
    }
}

/// A release whose unit is gone: one of this repo's on a selected target (plain `apply`), or one
/// of an env's whose unit left its blueprint (`env apply`). Its repo, target and unit are from its
/// labels (`?` for one it lacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub repo: String,
    pub target: String,
    pub unit: String,
    pub release: String,
    pub namespace: String,
    pub context: String,
}

impl Orphan {
    /// From a listed release and its labels.
    pub(crate) fn of(
        listed: helm::Listed,
        labels: Option<&serde_json::Map<String, serde_json::Value>>,
        context: &str,
    ) -> Self {
        let label = |key: &str| {
            labels
                .and_then(|l| l.get(key))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_owned()
        };
        Self {
            repo: label(REPO),
            target: label(TARGET),
            unit: label(UNIT),
            release: listed.name,
            namespace: listed.namespace,
            context: context.to_owned(),
        }
    }
}

impl fmt::Display for Orphan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unit `{}/{}/{}`: release `{}` in namespace `{}` on `{}`",
            self.repo, self.target, self.unit, self.release, self.namespace, self.context
        )
    }
}

/// What to uninstall, given the `orphans` found and the `prune` choice (`whose` names them in the
/// error, e.g. "units of repo `r` that are gone"): none for `Keep` or when there are none, all of
/// them for `Uninstall`.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if there are some and `prune` is `Unset`.
pub(crate) fn decide_prune(
    orphans: Vec<Orphan>,
    prune: Prune,
    whose: &str,
) -> Result<Vec<Orphan>, ObeliskK8sError> {
    match prune {
        _ if orphans.is_empty() => Ok(Vec::new()),
        Prune::Unset => {
            let listed: Vec<String> = orphans.iter().map(|o| format!("\n  {o}")).collect();
            Err(ObeliskK8sError::Usage(format!(
                "{whose} still have releases:{}\npass `--prune` to uninstall them before applying, or `--no-prune` to leave them",
                listed.concat()
            )))
        }
        Prune::Uninstall => Ok(orphans),
        Prune::Keep => Ok(Vec::new()),
    }
}

/// Whether cluster `name` is in `global` and local.
pub(crate) fn is_local(global: &GlobalConfig, name: &str) -> bool {
    global
        .cluster(name)
        .is_ok_and(|c| c.location == Location::Local)
}

/// Why installing over an existing release (its labels: `None` if there's none) would take it
/// from its owner, if it would: as part of env `env` (`None`: a plain `apply`). A plain `apply`
/// never takes an env's release; an env only takes its own (not one applied outside it, nor
/// another env's). A release without labels (from before them) is anyone's.
pub(crate) fn owner_problem(
    existing: Option<&serde_json::Map<String, serde_json::Value>>,
    env: Option<&str>,
) -> Option<String> {
    let existing = existing?;
    let owner = existing.get(ENV).and_then(serde_json::Value::as_str);
    let labelled = existing.contains_key(REPO);
    match (owner, env) {
        (Some(owner), Some(env)) if owner == env => None,
        (Some(owner), _) => Some(format!(
            "it belongs to env `{owner}` (`obelisk-k8s env down {owner}` takes it down)"
        )),
        (None, Some(env)) if labelled => Some(format!(
            "it was applied outside an env, so env `{env}` won't take it: uninstall it, or give this env's release another name or namespace"
        )),
        (None, _) => None,
    }
}

/// Checks no case in `plan` would install over a release it doesn't own (see [`owner_problem`]).
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] naming the first that would; [`ObeliskK8sError::Helm`] if a
/// release's labels can't be read.
pub(crate) fn check_owners(plan: &[Planned<'_>], env: Option<&str>) -> Result<(), ObeliskK8sError> {
    for planned in plan {
        let Some(release) = &planned.case.release else {
            continue;
        };
        let listed = helm::Listed {
            name: release.release_name.clone(),
            namespace: release.namespace.clone(),
        };
        let existing = helm::release_labels(&planned.context, &listed)?;
        if let Some(problem) = owner_problem(existing.as_ref(), env) {
            return Err(ObeliskK8sError::Usage(format!(
                "unit `{}` on target `{}` would install over release `{}` in namespace `{}` on `{}`, but {problem}",
                planned.case.unit,
                planned.case.target,
                listed.name,
                listed.namespace,
                planned.context
            )));
        }
    }
    Ok(())
}

/// What `apply` did, case by case.
#[derive(Debug)]
pub struct ApplyReport<'a> {
    /// Uninstalled first (`--prune`).
    pub pruned: Vec<Orphan>,
    pub applied: Vec<Applied<'a>>,
}

impl fmt::Display for ApplyReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for orphan in &self.pruned {
            writeln!(f, "pruned {orphan}")?;
        }
        for applied in &self.applied {
            let (target, unit) = (applied.target, applied.unit);
            match &applied.release {
                Some(release) => writeln!(
                    f,
                    "applied unit `{unit}` on target `{target}`: release `{}` (version `{}`) in namespace `{}` on `{}`",
                    release.release_name, release.version, release.namespace, applied.context
                )?,
                None => writeln!(
                    f,
                    "skipped unit `{unit}` on target `{target}`: it has no manifests"
                )?,
            }
        }
        Ok(())
    }
}

/// One case, ready to install (or diff).
#[derive(Debug)]
pub(crate) struct Planned<'a> {
    pub(crate) case: ManifestsCase<'a>,
    pub(crate) context: String,
    /// The release's labels (see [`release_labels`]).
    pub(crate) labels: Vec<(String, Option<String>)>,
}

/// Uninstalls each of `orphans`, in order (each waits up to 5m).
pub(crate) fn uninstall(
    orphans: Vec<Orphan>,
    progress: &mut dyn FnMut(&str),
) -> Result<Vec<Orphan>, ObeliskK8sError> {
    let mut pruned = Vec::new();
    for orphan in orphans {
        progress(&format!("pruning {orphan}..."));
        helm::uninstall(
            &orphan.context,
            &helm::Listed {
                name: orphan.release.clone(),
                namespace: orphan.namespace.clone(),
            },
        )?;
        pruned.push(orphan);
    }
    Ok(pruned)
}

/// Installs each case of `plan` in order (one without manifests is skipped), stopping at the
/// first that fails (the ones before it stay installed).
pub(crate) fn install<'a>(
    plan: Vec<Planned<'a>>,
    takeover: Takeover,
    progress: &mut dyn FnMut(&str),
) -> Result<Vec<Applied<'a>>, ObeliskK8sError> {
    let mut applied = Vec::new();
    for Planned {
        case,
        context,
        labels,
    } in plan
    {
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
                &labels,
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
    Ok(applied)
}

pub(crate) const REPO: &str = "obelisk-k8s/repo";
pub(crate) const TARGET: &str = "obelisk-k8s/target";
pub(crate) const UNIT: &str = "obelisk-k8s/unit";
const EPHEMERAL: &str = "obelisk-k8s/ephemeral";
const SHA: &str = "obelisk-k8s/sha";
pub(crate) const ENV: &str = "obelisk-k8s/env";
const BLUEPRINT: &str = "obelisk-k8s/blueprint";

/// The labels `apply` puts on every release (`helm list -l obelisk-k8s/unit=app`): the repo,
/// target and unit it is, the ephemeral copy (removed for a target that isn't ephemeral), the
/// commit it was rendered from (`obelisk.git.sha.value`; removed from a release outside git), and
/// the env and blueprint it's part of (removed outside an env).
pub(crate) fn release_labels(
    repo: &str,
    target: &str,
    unit: &str,
    ephemeral: Option<&str>,
    git: &serde_json::Value,
    env: Option<&EnvInstance>,
) -> Vec<(String, Option<String>)> {
    let sha = git["sha"]["value"].as_str().map(str::to_owned);
    vec![
        (REPO.to_owned(), Some(repo.to_owned())),
        (TARGET.to_owned(), Some(target.to_owned())),
        (UNIT.to_owned(), Some(unit.to_owned())),
        (EPHEMERAL.to_owned(), ephemeral.map(str::to_owned)),
        (SHA.to_owned(), sha),
        (ENV.to_owned(), env.map(|e| e.name.clone())),
        (BLUEPRINT.to_owned(), env.map(|e| e.blueprint.clone())),
    ]
}

/// The selector for the releases of repo `repo`'s target `target` (copy `ephemeral`, or none),
/// applied outside an env (an env's are its own to prune), whose unit isn't one of `units`.
fn orphan_selector(repo: &str, target: &str, ephemeral: Option<&str>, units: &[&str]) -> String {
    let mut parts = vec![
        format!("{REPO}={repo}"),
        format!("{TARGET}={target}"),
        format!("!{ENV}"),
    ];
    parts.push(match ephemeral {
        Some(copy) => format!("{EPHEMERAL}={copy}"),
        None => format!("!{EPHEMERAL}"),
    });
    parts.extend(units.iter().map(|u| format!("{UNIT}!={u}")));
    parts.join(",")
}

impl RepoConfig {
    /// Applies every case in `selection` to the clusters in `global` (remote ones only with
    /// `remote_ok`), taking over what `takeover` allows: plans them all, then installs each.
    /// `warn` hears each build plan's warnings as it's planned (once each, and before anything
    /// installs); `progress` hears about each install as it starts.
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
        instance: Instance<'_>,
        global: &GlobalConfig,
        remote_ok: bool,
        takeover: Takeover,
        ssm: &Ssm,
        prune: Prune,
        mut warn: impl FnMut(&str),
        mut progress: impl FnMut(&str),
    ) -> Result<ApplyReport<'a>, ObeliskK8sError> {
        if prune == Prune::Uninstall
            && let Some((target, unit)) = self.left_out(selection)
        {
            return Err(ObeliskK8sError::Usage(format!(
                "`--prune` needs every unit of each selected target, but unit `{unit}` (on target `{target}`) isn't selected: drop `-u` (with a subset, a release another unit now installs could look gone)"
            )));
        }
        if prune == Prune::Uninstall && !self.skipped.is_empty() {
            return Err(ObeliskK8sError::Usage(format!(
                "`--prune` won't run while entries of repo `{}` are skipped (a unit skipped this way would look gone): {}",
                self.name,
                self.skipped.join("; ")
            )));
        }
        if prune != Prune::Unset {
            for (target, cluster) in remote_targets(selection, global) {
                warn(&format!(
                    "target `{target}` deploys to remote cluster `{cluster}`: releases of units that are gone aren't looked for there (local clusters only, for now)"
                ));
            }
        }
        let plan = self.plan(
            repo_root, selection, shown_root, instance, global, remote_ok, ssm, &mut warn,
        )?;
        check_owners(&plan, None)?;
        // `--no-prune` leaves them, so there's no need to look.
        let orphans = if prune == Prune::Keep {
            Vec::new()
        } else {
            self.orphans(selection, &plan, instance, global, prune, &mut warn)?
        };
        if plan.is_empty() && orphans.is_empty() {
            return Err(ObeliskK8sError::Usage(NOTHING_SELECTED.to_owned()));
        }
        let whose = format!("units of repo `{}` that are gone", self.name);
        let doomed = decide_prune(orphans, prune, &whose)?;
        let pruned = uninstall(doomed, &mut progress)?;
        let applied = install(plan, takeover, &mut progress)?;
        Ok(ApplyReport { pruned, applied })
    }

    /// The releases of units that are gone (see the module docs), for each selected target on a
    /// local cluster (as the selected ephemeral copy, on an ephemeral target; one without a copy
    /// can't say whose they'd be, so it's skipped), whether or not a selected unit still deploys
    /// there. A release the plan installs anyway (same name, namespace and context) isn't one: it's
    /// upgraded in place.
    ///
    /// A cluster it can't look on is an error for a target the plan installs on, or with `--prune`;
    /// otherwise (a target only selected by default, with nothing to install) a warning, so a
    /// stopped cluster the run doesn't touch can't stop it.
    pub(crate) fn orphans(
        &self,
        selection: &Selection<'_>,
        plan: &[Planned<'_>],
        instance: Instance<'_>,
        global: &GlobalConfig,
        prune: Prune,
        warn: &mut dyn FnMut(&str),
    ) -> Result<Vec<Orphan>, ObeliskK8sError> {
        let installs = installs(plan);
        let mut orphans = Vec::new();
        for target in &selection.targets {
            let Some(context) = global
                .cluster(&target.cluster)
                .ok()
                .filter(|c| c.location == Location::Local)
                .and_then(Cluster::context)
            else {
                continue;
            };
            let copy = match (target.ephemeral, instance.ephemeral) {
                (false, _) => None,
                (true, Some(copy)) => Some(copy.name.as_str()),
                (true, None) => continue,
            };
            let units: Vec<&str> = self
                .units
                .iter()
                .filter(|u| self.unit_target_names(u).contains(&target.name.as_str()))
                .map(|u| u.name.as_str())
                .collect();
            let selector = orphan_selector(&self.name, &target.name, copy, &units);
            let listed = match helm::list_releases(&context, &selector) {
                Ok(listed) => listed,
                Err(error) => {
                    let installs_there = selection.cases.iter().any(|(t, _)| t.name == target.name);
                    if installs_there || prune == Prune::Uninstall {
                        return Err(error);
                    }
                    warn(&format!(
                        "couldn't look for releases of units that are gone on target `{}`: {error}",
                        target.name
                    ));
                    continue;
                }
            };
            let listed = listed.into_iter().map(|l| (context.clone(), l));
            for (context, listed) in not_installed(listed, &installs) {
                let labels = helm::release_labels(&context, &listed)?;
                orphans.push(Orphan::of(listed, labels.as_ref(), &context));
            }
        }
        Ok(orphans)
    }

    /// A unit that deploys to a selected target but isn't selected (`-u` narrowed it), if any.
    fn left_out<'s>(&'s self, selection: &Selection<'s>) -> Option<(&'s str, &'s str)> {
        for target in selection.targets.iter().map(|t| t.name.as_str()) {
            for unit in &self.units {
                let deploys = self.unit_target_names(unit).contains(&target);
                let selected = selection
                    .cases
                    .iter()
                    .any(|(t, u)| t.name == target && u.name == unit.name);
                if deploys && !selected {
                    return Some((target, unit.name.as_str()));
                }
            }
        }
        None
    }

    /// Every case in `selection`, rendered and checked, with the context to install it on (shared
    /// with `diff`, which shows what installing it would change). `warn` hears each build plan's
    /// warnings, once each.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        instance: Instance<'_>,
        global: &GlobalConfig,
        remote_ok: bool,
        ssm: &Ssm,
        warn: impl FnMut(&str),
    ) -> Result<Vec<Planned<'a>>, ObeliskK8sError> {
        let inputs = RunInputs::new(repo_root, selection, instance, global, Some(ssm))?;
        let mut contexts = Vec::new();
        for (target, _) in &selection.cases {
            let cluster = inputs.cluster(target)?;
            let context = context_for(target, cluster)?;
            check_remote(target, cluster, remote_ok)?;
            contexts.push(context);
        }
        let mut plan: Vec<Planned<'a>> = Vec::new();
        let mut warnings = Warnings::new(warn);
        let mut capabilities: Vec<(String, helm::Capabilities)> = Vec::new();
        for ((target, unit), context) in selection.cases.iter().zip(contexts) {
            let obelisk = self.obelisk_values(target, unit, &inputs)?;
            let build = self.build_plan(repo_root, shown_root, target, unit, &obelisk)?;
            warnings.add(&build.warnings);
            // A chart renders for the cluster it goes to: ask each such cluster once.
            let has_chart = build
                .phases
                .iter()
                .flat_map(|p| &p.manifests)
                .any(|s| matches!(s.kind, SourceKind::Helm(_)));
            if has_chart && !capabilities.iter().any(|(c, _)| *c == context) {
                capabilities.push((context.clone(), helm::capabilities(&context)?));
            }
            let found = capabilities
                .iter()
                .find(|(c, _)| has_chart && *c == context)
                .map(|(_, caps)| caps);
            let case = Self::manifests_for(
                repo_root, shown_root, target, unit, &inputs, &build, obelisk, found,
            )?;
            if let Some(release) = &case.release {
                check_renders_something(&case, release)?;
                check_unique(&plan, &case, release, &context)?;
            }
            let copy = inputs.ephemeral_for(target).map(|e| e.name.as_str());
            let labels = release_labels(
                &self.name,
                &target.name,
                &unit.name,
                copy,
                &inputs.git,
                inputs.env,
            );
            plan.push(Planned {
                case,
                context,
                labels,
            });
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

/// The selected targets on a remote cluster, with it: pruning skips them (local clusters only, for
/// now).
fn remote_targets<'s>(selection: &Selection<'s>, global: &GlobalConfig) -> Vec<(&'s str, &'s str)> {
    selection
        .targets
        .iter()
        .filter(|t| {
            global
                .cluster(&t.cluster)
                .is_ok_and(|c| c.location != Location::Local)
        })
        .map(|t| (t.name.as_str(), t.cluster.as_str()))
        .collect()
}

/// Every release `plan` installs, as `(context, release, namespace)`.
pub(crate) fn installs(plan: &[Planned<'_>]) -> HashSet<(String, String, String)> {
    plan.iter()
        .filter_map(|p| {
            let r = p.case.release.as_ref()?;
            Some((
                p.context.clone(),
                r.release_name.clone(),
                r.namespace.clone(),
            ))
        })
        .collect()
}

/// The `(context, release)`s of `listed` that aren't in `installs`: a release the plan installs
/// anyway is upgraded in place, so it's never one to prune.
pub(crate) fn not_installed(
    listed: impl IntoIterator<Item = (String, helm::Listed)>,
    installs: &HashSet<(String, String, String)>,
) -> Vec<(String, helm::Listed)> {
    listed
        .into_iter()
        .filter(|(context, l)| {
            !installs.contains(&(context.clone(), l.name.clone(), l.namespace.clone()))
        })
        .collect()
}

/// Manifests sources that render nothing would install an empty release, and an upgrade to one deletes
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
pub(crate) fn check_unique(
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
pub(crate) fn takeover_hint(takeover: Takeover, error: ObeliskK8sError) -> ObeliskK8sError {
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

    #[test]
    fn labels_say_which_repo_target_unit_and_commit() {
        let git = serde_json::json!({ "sha": { "short": "abc", "value": "abc123" } });
        let label = |labels: &[(String, Option<String>)], key: &str| {
            labels.iter().find(|(k, _)| k == key).unwrap().1.clone()
        };
        let labels = release_labels("r", "t", "u", Some("e1"), &git, None);
        assert_eq!(label(&labels, "obelisk-k8s/repo").as_deref(), Some("r"));
        assert_eq!(label(&labels, "obelisk-k8s/target").as_deref(), Some("t"));
        assert_eq!(label(&labels, "obelisk-k8s/unit").as_deref(), Some("u"));
        assert_eq!(label(&labels, "obelisk-k8s/sha").as_deref(), Some("abc123"));
        assert_eq!(
            label(&labels, "obelisk-k8s/ephemeral").as_deref(),
            Some("e1")
        );
        // Outside git, or not ephemeral: the label is removed, not left from an earlier apply.
        let labels = release_labels("r", "t", "u", None, &serde_json::Value::Null, None);
        assert_eq!(label(&labels, "obelisk-k8s/sha"), None);
        assert_eq!(label(&labels, "obelisk-k8s/ephemeral"), None);
    }

    #[test]
    fn pruning_skips_remote_targets() {
        let (_g, global) = global(&CLUSTERS);
        let (_dir, config) = repo(&[]);
        let local = config.select(&["dev".to_owned()], &[]).unwrap();
        assert_eq!(remote_targets(&local, &global), []);
        let both = config
            .select(&["dev".to_owned(), "prod".to_owned()], &[])
            .unwrap();
        assert_eq!(remote_targets(&both, &global), [("prod", "r")]);
    }

    #[test]
    fn a_release_the_plan_installs_is_never_pruned() {
        let listed = |name: &str, ns: &str| helm::Listed {
            name: name.to_owned(),
            namespace: ns.to_owned(),
        };
        let installs: HashSet<_> = [("k3d-c".to_owned(), "a".to_owned(), "ns".to_owned())].into();
        let kept = not_installed(
            [
                ("k3d-c".to_owned(), listed("a", "ns")),
                // Same name, another namespace or context: not the same release.
                ("k3d-c".to_owned(), listed("a", "other")),
                ("k3d-d".to_owned(), listed("a", "ns")),
                ("k3d-c".to_owned(), listed("b", "ns")),
            ],
            &installs,
        );
        let kept: Vec<(&str, &str, &str)> = kept
            .iter()
            .map(|(c, l)| (c.as_str(), l.name.as_str(), l.namespace.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                ("k3d-c", "a", "other"),
                ("k3d-d", "a", "ns"),
                ("k3d-c", "b", "ns")
            ]
        );
    }

    #[test]
    fn a_copy_can_be_named_for_an_ephemeral_target_with_no_units_left() {
        // `app` only deploys to `dev` now: ephemeral `eph` has no units left, but its copy `e1`
        // may still have releases there, so `--ephemeral-name e1` must get as far as looking.
        let (_dir, config) = repo(&[
            (
                "units/app/unit.yaml",
                "unit:\n  targets: [dev]\n  release: { version: v1 }\n",
            ),
            (
                "targets/eph/target.yaml",
                "target:\n  cluster: k\n  ephemeral: true\n  release: { name: app, namespace: n }\n",
            ),
        ]);
        let (_g, global) = global(&CLUSTERS);
        let selection = config.select_for_apply(&["eph".to_owned()], &[]).unwrap();
        let copy = Ephemeral::new("e1".into(), EphemeralValues::Inline("{}".into())).unwrap();
        let mut warnings = Vec::new();
        let error = config
            .apply(
                Path::new("."),
                &selection,
                None,
                Some(&copy).into(),
                &global,
                false,
                Takeover::default(),
                &Ssm::with_fetch(Box::new(|_| panic!("no refs here"))),
                Prune::Unset,
                |w| warnings.push(w.to_owned()),
                |_| {},
            )
            .unwrap_err()
            .to_string();
        // There's no cluster here to look on: a warning (nothing is installed there), then nothing
        // to do; never "none of the targets being rendered is ephemeral".
        assert_eq!(error, NOTHING_SELECTED, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w
                .starts_with("couldn't look for releases of units that are gone on target `eph`")),
            "{warnings:?}"
        );
        // `--prune` must see that target, so there it's an error.
        let error = config
            .apply(
                Path::new("."),
                &selection,
                None,
                Some(&copy).into(),
                &global,
                false,
                Takeover::default(),
                &Ssm::with_fetch(Box::new(|_| panic!("no refs here"))),
                Prune::Uninstall,
                |_| {},
                |_| {},
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("can't"), "{error}");
        // Rendering still refuses a copy no case needs.
        let rendered = config.select(&["dev".to_owned()], &[]).unwrap();
        let error = crate::config::ephemeral::check_selection(&rendered, Some(&copy))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("none of the targets being rendered is ephemeral"),
            "{error}"
        );
    }

    #[test]
    fn pruning_refuses_while_units_are_skipped() {
        // `units/web/` lost its `unit.yaml` (mid-edit, a typo): `web` isn't in the repo, so its
        // release would look gone.
        let (_dir, config) = repo(&[("units/web/unit.yml", "unit: {}\n")]);
        assert_eq!(config.skipped.len(), 1, "{:?}", config.skipped);
        let (_g, global) = global(&CLUSTERS);
        let selection = config.select_for_apply(&["dev".to_owned()], &[]).unwrap();
        let error = config
            .apply(
                Path::new("."),
                &selection,
                None,
                Instance::default(),
                &global,
                false,
                Takeover::default(),
                &Ssm::with_fetch(Box::new(|_| panic!("no refs here"))),
                Prune::Uninstall,
                |_| {},
                |_| {},
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("`--prune` won't run while entries of repo `r` are skipped"),
            "{error}"
        );
    }

    #[test]
    fn pruning_looks_at_targets_whose_units_are_all_gone() {
        // `app` only deploys to `prod` now: `dev` has no units left, but may still hold releases.
        let (_dir, config) = repo(&[(
            "units/app/unit.yaml",
            "unit:\n  targets: [prod]\n  release: { version: v1 }\n",
        )]);
        let dev = ["dev".to_owned()];
        assert!(config.select(&dev, &[]).is_err(), "nothing to render there");
        let selection = config.select_for_apply(&dev, &[]).unwrap();
        assert_eq!(selection.cases.len(), 0);
        let targets: Vec<&str> = selection.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(targets, ["dev"]);
    }

    fn orphan(unit: &str) -> Orphan {
        Orphan {
            repo: "r".to_owned(),
            target: "t".to_owned(),
            unit: unit.to_owned(),
            release: unit.to_owned(),
            namespace: "ns".to_owned(),
            context: "k3d-c".to_owned(),
        }
    }

    #[test]
    fn deciding_what_to_prune() {
        assert_eq!(Prune::from_flags(false, false), Prune::Unset);
        assert_eq!(Prune::from_flags(true, false), Prune::Uninstall);
        assert_eq!(Prune::from_flags(false, true), Prune::Keep);
        // None found: any choice (or none) is fine.
        for prune in [Prune::Unset, Prune::Uninstall, Prune::Keep] {
            assert_eq!(decide_prune(Vec::new(), prune, "x").unwrap(), []);
        }
        let found = vec![orphan("old")];
        assert_eq!(
            decide_prune(found.clone(), Prune::Uninstall, "x").unwrap(),
            found
        );
        assert_eq!(decide_prune(found.clone(), Prune::Keep, "x").unwrap(), []);
        let error = decide_prune(found, Prune::Unset, "units of repo `r` that are gone")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "units of repo `r` that are gone still have releases:\n  unit `r/t/old`: release `old` in namespace `ns` on `k3d-c`\npass `--prune` to uninstall them before applying, or `--no-prune` to leave them"
        );
    }

    #[test]
    fn who_may_install_over_a_release() {
        let labels = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), serde_json::Value::from(*v)))
                .collect::<serde_json::Map<_, _>>()
        };
        let plain = labels(&[(REPO, "r"), (UNIT, "a")]);
        let of_e1 = labels(&[(REPO, "r"), (UNIT, "a"), (ENV, "e1")]);
        let unlabelled = labels(&[]);
        // No release yet, or one from before labels: anyone may.
        for env in [None, Some("e1")] {
            assert_eq!(owner_problem(None, env), None);
            assert_eq!(owner_problem(Some(&unlabelled), env), None);
        }
        // A plain apply may take a plain release, never an env's.
        assert_eq!(owner_problem(Some(&plain), None), None);
        let problem = owner_problem(Some(&of_e1), None).unwrap();
        assert!(problem.starts_with("it belongs to env `e1`"), "{problem}");
        // An env takes only its own.
        assert_eq!(owner_problem(Some(&of_e1), Some("e1")), None);
        let problem = owner_problem(Some(&of_e1), Some("e2")).unwrap();
        assert!(problem.starts_with("it belongs to env `e1`"), "{problem}");
        let problem = owner_problem(Some(&plain), Some("e2")).unwrap();
        assert!(
            problem.starts_with("it was applied outside an env"),
            "{problem}"
        );
    }

    #[test]
    fn pruning_needs_every_unit_of_a_target() {
        let (_dir, config) = repo(&[("units/web/unit.yaml", "unit: {}\n")]);
        let all = config.select(&["dev".to_owned()], &[]).unwrap();
        assert_eq!(config.left_out(&all), None);
        let some = config
            .select(&["dev".to_owned()], &["app".to_owned()])
            .unwrap();
        assert_eq!(config.left_out(&some), Some(("dev", "web")));
    }

    #[test]
    fn selects_the_releases_of_units_that_are_gone() {
        assert_eq!(
            orphan_selector("r", "t", None, &["a", "b"]),
            "obelisk-k8s/repo=r,obelisk-k8s/target=t,!obelisk-k8s/env,!obelisk-k8s/ephemeral,obelisk-k8s/unit!=a,obelisk-k8s/unit!=b"
        );
        assert_eq!(
            orphan_selector("r", "t", Some("e1"), &[]),
            "obelisk-k8s/repo=r,obelisk-k8s/target=t,!obelisk-k8s/env,obelisk-k8s/ephemeral=e1"
        );
    }
    use crate::config::ephemeral::{Ephemeral, EphemeralValues};
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
    /// config) and `blind` (no context); unit `app`, whose build file (the repo's) has one phase
    /// with manifests source `app.gotmpl`; `extra` on top.
    fn repo(extra: &[(&str, &str)]) -> (tempfile::TempDir, RepoConfig) {
        let target = |cluster: &str| format!("target:\n  cluster: {cluster}\n{RELEASE}");
        let (dev, prod, lost, blind) = (target("k"), target("r"), target("nope"), target("b"));
        let mut files = vec![
            ("repo.yaml", "repo:\n  name: r\n  build: ./build.gotmpl\n"),
            (
                "build.gotmpl",
                "build:\n  phases:\n    - name: out\n      manifests: [{ type: gotmpl, path: ./app.gotmpl }]\n",
            ),
            ("targets/dev/target.yaml", dev.as_str()),
            ("targets/prod/target.yaml", prod.as_str()),
            ("targets/lost/target.yaml", lost.as_str()),
            ("targets/blind/target.yaml", blind.as_str()),
            ("units/app/unit.yaml", "unit:\n  release: { version: v1 }\n"),
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
        config.plan(
            dir.path(),
            &selection,
            None,
            Instance::default(),
            global,
            remote_ok,
            &ssm,
            |_| {},
        )
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
                "unit:\n  release: { version: v1 }\n",
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
    fn a_unit_without_manifests_is_skipped() {
        if !helm_installed() {
            return;
        }
        let (_g, global) = global(&CLUSTERS);
        let (dir, config) = repo(&[(
            "build.gotmpl",
            "build:\n  phases:\n    - { name: only-values, values: [{ type: inline, values: { a: 1 } }] }\n    - { name: missing, manifests: [{ type: gotmpl, path: ./nope.gotmpl }] }\n",
        )]);
        let selection = config.select(&["dev".to_owned()], &[]).unwrap();
        let report = config
            .apply(
                dir.path(),
                &selection,
                None,
                Instance::default(),
                &global,
                false,
                Takeover::default(),
                &Ssm::with_fetch(Box::new(|_| panic!("no refs here"))),
                // There's no cluster to look for units that are gone on.
                Prune::Keep,
                |_| {},
                |_| {},
            )
            .unwrap();
        assert_eq!(
            report.to_string(),
            "skipped unit `app` on target `dev`: it has no manifests\n"
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
