//! `diff`: what `apply` would change, case by case, without changing anything.
//!
//! Planned exactly as `apply` plans (see `config::apply`: clusters resolved, every case rendered and
//! checked, charts rendered for their cluster's capabilities), remote clusters included (it only
//! reads). Each case with manifests then goes into the same throwaway chart `apply` would install,
//! and the helm-diff plugin (`helm diff upgrade`) compares it with what's live on the cluster: a
//! three-way merge (the release's last manifests, what's live, and what would be installed), with
//! the manifests normalized so style alone isn't a change. A release that isn't installed yet diffs
//! as all new. Secrets' data is masked by helm-diff unless [`DiffOptions::show_secrets`].

use std::fmt;
use std::path::Path;

use crate::ObeliskK8sError;
use crate::config::apply::{Planned, Takeover, takeover_hint};
use crate::config::ephemeral::Ephemeral;
use crate::config::global::GlobalConfig;
use crate::config::paths::{CASE_HEADER, Selection};
use crate::config::repo::RepoConfig;
use crate::config::values::Agreed;
use crate::helm;
pub use crate::helm::DiffOptions;
use crate::ssm::Ssm;

/// What `diff` found for one case.
#[derive(Debug)]
pub struct Diffed<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    /// Where it would go; `None` if the unit has no manifests sources (nothing to diff).
    pub release: Option<Agreed>,
    pub context: String,
    /// helm-diff's output, as it printed it (empty when nothing would change).
    pub diff: String,
}

impl Diffed<'_> {
    /// Whether applying this case would change anything.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.diff.trim().is_empty()
    }
}

/// What `diff` found, case by case: each under a magenta `[ target=…, unit=…, release=…,
/// namespace=…, context=… ]` header, then the diff (or `no changes`), a blank line between.
#[derive(Debug)]
pub struct DiffReport<'a> {
    pub diffs: Vec<Diffed<'a>>,
}

impl DiffReport<'_> {
    /// Whether applying would change anything.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.diffs.iter().any(Diffed::changed)
    }
}

impl fmt::Display for DiffReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, diffed) in self.diffs.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            let (target, unit) = (diffed.target, diffed.unit);
            let Some(release) = &diffed.release else {
                writeln!(
                    f,
                    "{CASE_HEADER}[ target={target}, unit={unit} ]{CASE_HEADER:#}\nno manifests: nothing to diff"
                )?;
                continue;
            };
            writeln!(
                f,
                "{CASE_HEADER}[ target={target}, unit={unit}, release={}, namespace={}, context={} ]{CASE_HEADER:#}",
                release.release_name, release.namespace, diffed.context
            )?;
            if diffed.changed() {
                writeln!(f, "{}", diffed.diff.trim_end())?;
            } else {
                writeln!(f, "no changes")?;
            }
        }
        Ok(())
    }
}

impl RepoConfig {
    /// What applying every case in `selection` to the clusters in `global` would change, diffed as
    /// `options` says. Nothing is installed. `warn` hears each build plan's warnings as it's planned
    /// (once each, and before anything is diffed); `progress` hears about each diff as it starts.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Helm`] if the helm-diff plugin isn't installed (before anything renders),
    /// or a diff fails; otherwise as planning for [`RepoConfig::apply`] does.
    #[allow(clippy::too_many_arguments)]
    pub fn diff<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &GlobalConfig,
        options: DiffOptions,
        ssm: &Ssm,
        warn: impl FnMut(&str),
        mut progress: impl FnMut(&str),
    ) -> Result<DiffReport<'a>, ObeliskK8sError> {
        helm::check_diff_plugin()?;
        // It only reads, so a remote cluster needs no `--yes`.
        let plan = self.plan(
            repo_root, selection, shown_root, ephemeral, global, true, ssm, warn,
        )?;
        let mut diffs = Vec::new();
        for Planned { case, context } in plan {
            let diff = match &case.release {
                Some(release) => {
                    progress(&format!(
                        "diffing unit `{}` on target `{}` against `{context}`...",
                        case.unit, case.target
                    ));
                    // A refusal `apply` would hit too: point at the flag that gets past it (diff
                    // has no `--force-conflicts`).
                    let takeover = Takeover {
                        take_ownership: options.take_ownership,
                        force_conflicts: true,
                    };
                    diff_case(release, &case.manifests, &context, options)
                        .map_err(|error| takeover_hint(takeover, error))?
                }
                None => String::new(),
            };
            diffs.push(Diffed {
                target: case.target,
                unit: case.unit,
                release: case.release,
                context,
                diff,
            });
        }
        Ok(DiffReport { diffs })
    }
}

/// Diffs `manifests` as `release` on `context`, through the chart `apply` would install.
fn diff_case(
    release: &Agreed,
    manifests: &str,
    context: &str,
    options: DiffOptions,
) -> Result<String, ObeliskK8sError> {
    let name = &release.release_name;
    let chart = helm::manifests_chart(name, &release.version, manifests)?;
    helm::diff_upgrade(&helm::Diff {
        release: name,
        chart: chart.path(),
        namespace: &release.namespace,
        context,
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diffed(unit: &'static str, diff: &str, release: bool) -> Diffed<'static> {
        Diffed {
            target: "prod",
            unit,
            release: release.then(|| Agreed {
                release_name: unit.to_owned(),
                namespace: "ns".to_owned(),
                version: "1.0.0".to_owned(),
            }),
            context: "admin@c".to_owned(),
            diff: diff.to_owned(),
        }
    }

    #[test]
    fn reports_each_case_under_a_header() {
        let report = DiffReport {
            diffs: vec![
                diffed(
                    "a",
                    "ns, a, ConfigMap (v1) has changed:\n- x\n+ y\n\n",
                    true,
                ),
                diffed("b", "", true),
                diffed("c", "", false),
            ],
        };
        let text = anstream::adapter::strip_str(&report.to_string()).to_string();
        assert_eq!(
            text,
            "[ target=prod, unit=a, release=a, namespace=ns, context=admin@c ]\n\
             ns, a, ConfigMap (v1) has changed:\n- x\n+ y\n\n\
             [ target=prod, unit=b, release=b, namespace=ns, context=admin@c ]\n\
             no changes\n\n\
             [ target=prod, unit=c ]\n\
             no manifests: nothing to diff\n"
        );
        assert!(
            report
                .to_string()
                .starts_with(&format!("{CASE_HEADER}[ target=prod, unit=a,"))
        );
    }

    #[test]
    fn changed_if_any_case_would_change() {
        let report = |diffs| DiffReport { diffs }.changed();
        assert!(!report(vec![]));
        assert!(!report(vec![diffed("a", "", true), diffed("b", "", false)]));
        assert!(report(vec![
            diffed("a", "", true),
            diffed("b", "- x\n", true)
        ]));
    }
}
