//! What every case in a run shares, beyond the repo's config.

use std::path::Path;

use serde_json::Value;

use crate::config::ephemeral::{Ephemeral, check_selection};
use crate::config::global::{Cluster, GlobalConfig, normalize};
use crate::config::paths::Selection;
use crate::config::repo::{REPO_CONFIG_DIR, Target};
use crate::ssm::Ssm;
use crate::{ObeliskK8sError, git};

/// The git facts, the ephemeral copy (if any), the global config the targets' clusters come from,
/// where things are (`obelisk.paths`), and what resolves SSM refs (if they're resolved).
#[derive(Debug)]
pub(crate) struct RunInputs<'a> {
    pub(crate) git: Value,
    /// `obelisk.paths`: `{ repo: { root, obelisk }, global: { obelisk } }`, the repo root, its
    /// `.obelisk-k8s/`, and the global config's directory; absolute and normalized.
    pub(crate) paths: Value,
    pub(crate) ephemeral: Option<&'a Ephemeral>,
    /// `None`: refs are only checked, and left as written.
    pub(crate) ssm: Option<&'a Ssm>,
    global: &'a GlobalConfig,
}

impl<'a> RunInputs<'a> {
    /// Gathers them for `selection`, after checking it: it needs `ephemeral` exactly when one of its
    /// targets is ephemeral, and every one of its targets' clusters must be in `global`.
    ///
    /// # Errors
    ///
    /// As [`check_selection`] and [`RunInputs::cluster`]; [`ObeliskK8sError::Io`] if `repo_root`
    /// can't be made absolute; [`ObeliskK8sError::Usage`] if it or the global root isn't UTF-8 (they
    /// go into the values as text).
    pub(crate) fn new(
        repo_root: &Path,
        selection: &Selection<'_>,
        ephemeral: Option<&'a Ephemeral>,
        global: &'a GlobalConfig,
        ssm: Option<&'a Ssm>,
    ) -> Result<Self, ObeliskK8sError> {
        check_selection(selection, ephemeral)?;
        let repo_root_absolute =
            std::path::absolute(repo_root).map_err(|source| ObeliskK8sError::Io {
                path: repo_root.to_owned(),
                source,
            })?;
        let repo_root_absolute = normalize(&repo_root_absolute);
        let inputs = Self {
            git: git::facts(repo_root),
            paths: serde_json::json!({
                "repo": {
                    "root": utf8(&repo_root_absolute)?,
                    "obelisk": utf8(&repo_root_absolute.join(REPO_CONFIG_DIR))?,
                },
                "global": { "obelisk": utf8(&global.dir)? },
            }),
            ephemeral,
            ssm,
            global,
        };
        for (target, _) in &selection.cases {
            inputs.cluster(target)?;
        }
        Ok(inputs)
    }

    /// The cluster `target` deploys to.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if it isn't in the global config (naming the ones that are).
    pub(crate) fn cluster(&self, target: &Target) -> Result<&'a Cluster, ObeliskK8sError> {
        self.global.cluster(&target.cluster).map_err(|_| {
            let known: Vec<_> = self.global.clusters.iter().map(|c| c.name.as_str()).collect();
            let known = if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            };
            ObeliskK8sError::Usage(format!(
                "target `{}` deploys to cluster `{}`, which isn't in the global config at {} (it has: {known}): add it with `obelisk-k8s config global cluster add`",
                target.name,
                target.cluster,
                self.global.root.display()
            ))
        })
    }

    /// The copy `target` renders as: `None` unless it's ephemeral.
    pub(crate) fn ephemeral_for(&self, target: &Target) -> Option<&'a Ephemeral> {
        self.ephemeral.filter(|_| target.ephemeral)
    }
}

/// `path` as text, for the values.
fn utf8(path: &Path) -> Result<&str, ObeliskK8sError> {
    path.to_str().ok_or_else(|| {
        ObeliskK8sError::Usage(format!(
            "{} isn't UTF-8, so it can't go into `obelisk.paths`",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::global;

    #[test]
    fn the_paths_are_absolute_and_normalized() {
        let global = global();
        let selection = Selection {
            cases: Vec::new(),
            single: false,
        };
        let inputs = RunInputs::new(
            Path::new("some/./repo/../repo"),
            &selection,
            None,
            &global,
            None,
        )
        .unwrap();
        let repo = std::env::current_dir().unwrap().join("some/repo");
        assert_eq!(
            inputs.paths,
            serde_json::json!({
                "repo": {
                    "root": repo.to_str().unwrap(),
                    "obelisk": repo.join(".obelisk-k8s").to_str().unwrap(),
                },
                "global": { "obelisk": "/home/me/.obelisk-k8s" },
            })
        );
    }
}
