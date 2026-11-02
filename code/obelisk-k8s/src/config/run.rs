//! What every case in a run shares, beyond the repo's config.

use std::path::Path;

use serde_json::Value;

use crate::config::ephemeral::{Ephemeral, check_selection};
use crate::config::global::{Cluster, GlobalConfig};
use crate::config::paths::Selection;
use crate::config::repo::Target;
use crate::{ObeliskK8sError, git};

/// The git facts, the ephemeral copy (if any), and the global config the targets' clusters come
/// from.
#[derive(Debug)]
pub(crate) struct RunInputs<'a> {
    pub(crate) git: Value,
    pub(crate) ephemeral: Option<&'a Ephemeral>,
    global: &'a GlobalConfig,
}

impl<'a> RunInputs<'a> {
    /// Gathers them for `selection`, after checking it: it needs `ephemeral` exactly when one of its
    /// targets is ephemeral, and every one of its targets' clusters must be in `global`.
    ///
    /// # Errors
    ///
    /// As [`check_selection`] and [`RunInputs::cluster`].
    pub(crate) fn new(
        repo_root: &Path,
        selection: &Selection<'_>,
        ephemeral: Option<&'a Ephemeral>,
        global: &'a GlobalConfig,
    ) -> Result<Self, ObeliskK8sError> {
        check_selection(selection, ephemeral)?;
        let inputs = Self {
            git: git::facts(repo_root),
            ephemeral,
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
