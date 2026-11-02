//! Bringing managed clusters up, down, and away, and showing where they're at. Only local k3d
//! clusters so far.
//!
//! `up` creates the cluster (or starts it, if it's stopped); `down` stops it, keeping its state;
//! `destroy` deletes it. Each is a no-op when there's nothing to do, and each refuses a k3d cluster of
//! the same name that obelisk-k8s didn't create. A cluster with `obeliskd: { supernet, subnet }` runs
//! on that obeliskd docker subnet's network, which must already exist (obeliskd keeps it after
//! `destroy`).

use std::fmt;

use crate::ObeliskK8sError;
use crate::config::global::{Cluster, GlobalConfig, K8s, Location};
use crate::k3d::{self, Existing};
use crate::obeliskd;

/// What `up`, `down` or `destroy` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Started,
    AlreadyUp,
    Stopped,
    AlreadyDown,
    Destroyed,
    /// There's no such k3d cluster (for `down` and `destroy`).
    Missing,
}

/// What `up`, `down` or `destroy` did, to which cluster, and anything worth a warning.
#[derive(Debug)]
pub struct Report {
    pub cluster: String,
    pub outcome: Outcome,
    pub warnings: Vec<String>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = &self.cluster;
        let context = k3d::context(name);
        match self.outcome {
            Outcome::Created => write!(f, "created cluster `{name}` (kubectl context `{context}`)"),
            Outcome::Started => write!(f, "started cluster `{name}` (kubectl context `{context}`)"),
            Outcome::AlreadyUp => write!(f, "cluster `{name}` is already up"),
            Outcome::Stopped => write!(f, "stopped cluster `{name}`"),
            Outcome::AlreadyDown => write!(f, "cluster `{name}` is already down"),
            Outcome::Destroyed => write!(f, "destroyed cluster `{name}`"),
            Outcome::Missing => write!(f, "cluster `{name}` doesn't exist"),
        }
    }
}

/// Brings managed cluster `name` up: creates it, or starts it if it's stopped.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if `config` has no such cluster, it isn't managed, it isn't a local k3d
/// cluster, its name is too long for k3d, its obeliskd subnet isn't there, or a k3d cluster of that
/// name exists that obelisk-k8s didn't create; [`ObeliskK8sError::K3d`] or
/// [`ObeliskK8sError::Obeliskd`] if those fail.
pub fn up(config: &GlobalConfig, name: &str) -> Result<Report, ObeliskK8sError> {
    let cluster = managed_k3d(config, name)?;
    let existing = k3d::find(name)?;
    let outcome = match plan_up(name, existing.as_ref())? {
        Outcome::Created => {
            let network = match &cluster.obeliskd {
                Some(o) => Some(obeliskd::docker_network(&o.supernet, &o.subnet)?),
                None => None,
            };
            k3d::create(name, network.as_deref())?;
            Outcome::Created
        }
        Outcome::Started => {
            k3d::start(name)?;
            Outcome::Started
        }
        outcome => outcome,
    };
    let mut warnings = warnings(cluster);
    if let Some(existing) = &existing {
        let expected = expected_network(cluster);
        if existing.network != expected {
            warnings.push(format!(
                "cluster `{name}` runs on Docker network `{}`, but its config says `{expected}`: `destroy` and `up` it to move it",
                existing.network
            ));
        }
    }
    Ok(Report {
        cluster: name.to_owned(),
        outcome,
        warnings,
    })
}

/// Takes managed cluster `name` down: stops it, keeping its state for the next `up`.
///
/// # Errors
///
/// As [`up`].
pub fn down(config: &GlobalConfig, name: &str) -> Result<Report, ObeliskK8sError> {
    managed_k3d(config, name)?;
    let outcome = match plan_down(name, k3d::find(name)?.as_ref())? {
        Outcome::Stopped => {
            k3d::stop(name)?;
            Outcome::Stopped
        }
        outcome => outcome,
    };
    Ok(Report {
        cluster: name.to_owned(),
        outcome,
        warnings: Vec::new(),
    })
}

/// Destroys managed cluster `name`: deletes it, and everything in it.
///
/// # Errors
///
/// As [`up`].
pub fn destroy(config: &GlobalConfig, name: &str) -> Result<Report, ObeliskK8sError> {
    managed_k3d(config, name)?;
    let outcome = match plan_destroy(name, k3d::find(name)?.as_ref())? {
        Outcome::Destroyed => {
            k3d::delete(name)?;
            Outcome::Destroyed
        }
        outcome => outcome,
    };
    Ok(Report {
        cluster: name.to_owned(),
        outcome,
        warnings: Vec::new(),
    })
}

/// One cluster's line in `cluster status`.
#[derive(Debug)]
pub struct Status {
    pub name: String,
    pub k8s: K8s,
    pub location: Location,
    pub managed: bool,
    /// For a managed local k3d cluster; `None` for one obelisk-k8s doesn't bring up.
    pub state: Option<State>,
    /// The kubectl context it's reached through: k3d's, for a local k3d cluster that exists; else the
    /// one its config names, if any.
    pub context: Option<String>,
    /// The Docker network a local k3d cluster that exists runs on.
    pub network: Option<String>,
}

/// Where a managed k3d cluster is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    /// Stopped, or only some of its servers running.
    Stopped,
    /// In the global config, but there's no k3d cluster.
    Missing,
    /// A k3d cluster of that name that obelisk-k8s didn't create.
    NotOurs,
}

impl State {
    fn of(existing: Option<&Existing>) -> Self {
        match existing {
            None => Self::Missing,
            Some(Existing { ours: false, .. }) => Self::NotOurs,
            Some(Existing { running: true, .. }) => Self::Running,
            Some(Existing { running: false, .. }) => Self::Stopped,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Missing => "missing",
            Self::NotOurs => "not ours",
        })
    }
}

/// What `cluster status` prints: a table, one cluster a line; and what doesn't add up.
#[derive(Debug)]
pub struct StatusReport {
    pub statuses: Vec<Status>,
    pub warnings: Vec<String>,
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.statuses.is_empty() {
            return writeln!(f, "no clusters in the global config");
        }
        let dash = |s: Option<String>| s.unwrap_or_else(|| "-".to_owned());
        let rows: Vec<[String; 6]> = std::iter::once(
            ["NAME", "KIND", "MANAGED", "STATE", "CONTEXT", "NETWORK"].map(str::to_owned),
        )
        .chain(self.statuses.iter().map(|s| {
            [
                s.name.clone(),
                format!("{}/{}", s.k8s, s.location),
                if s.managed { "yes" } else { "no" }.to_owned(),
                dash(s.state.map(|state| state.to_string())),
                dash(s.context.clone()),
                dash(s.network.clone()),
            ]
        }))
        .collect();
        let widths: Vec<usize> = (0..6)
            .map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0))
            .collect();
        for row in &rows {
            let line: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:width$}"))
                .collect();
            writeln!(f, "{}", line.join("   ").trim_end())?;
        }
        Ok(())
    }
}

/// The status of cluster `name` in `config`, or of every cluster in it.
///
/// # Errors
///
/// [`ObeliskK8sError::Usage`] if `config` has no cluster `name`; [`ObeliskK8sError::K3d`] if k3d
/// fails (only asked when there's a local k3d cluster to look at).
pub fn status(config: &GlobalConfig, name: Option<&str>) -> Result<StatusReport, ObeliskK8sError> {
    let clusters: Vec<&Cluster> = match name {
        Some(name) => vec![config.cluster(name)?],
        None => config.clusters.iter().collect(),
    };
    let list = if clusters.iter().any(|c| is_local_k3d(c)) {
        k3d::list()?
    } else {
        k3d::List::default()
    };
    let mut report = StatusReport {
        statuses: Vec::new(),
        warnings: Vec::new(),
    };
    for cluster in clusters {
        let existing = if is_local_k3d(cluster) {
            list.find(&cluster.name)?
        } else {
            None
        };
        if cluster.managed && is_local_k3d(cluster) {
            report.warnings.extend(warnings(cluster));
        }
        report.statuses.push(status_of(cluster, existing.as_ref()));
    }
    Ok(report)
}

fn status_of(cluster: &Cluster, existing: Option<&Existing>) -> Status {
    let local_k3d = is_local_k3d(cluster);
    Status {
        name: cluster.name.clone(),
        k8s: cluster.k8s,
        location: cluster.location,
        managed: cluster.managed,
        state: (cluster.managed && local_k3d).then(|| State::of(existing)),
        context: match existing {
            Some(_) => Some(k3d::context(&cluster.name)),
            None if local_k3d => None,
            None => cluster.kubectl_context.clone(),
        },
        network: existing.map(|e| e.network.clone()),
    }
}

fn is_local_k3d(cluster: &Cluster) -> bool {
    (cluster.k8s, cluster.location) == (K8s::K3d, Location::Local)
}

/// The cluster `name` in `config`, if it's one obelisk-k8s can bring up, down and away.
fn managed_k3d<'a>(config: &'a GlobalConfig, name: &str) -> Result<&'a Cluster, ObeliskK8sError> {
    let cluster = config.cluster(name)?;
    if !cluster.managed {
        return Err(ObeliskK8sError::Usage(format!(
            "cluster `{name}` isn't managed (`managed: true` in {}): obelisk-k8s doesn't create or delete it",
            cluster.file.display()
        )));
    }
    if !is_local_k3d(cluster) {
        return Err(ObeliskK8sError::Usage(format!(
            "cluster `{name}` is {} on {}: obelisk-k8s can only bring up local k3d clusters so far",
            cluster.k8s, cluster.location
        )));
    }
    if name.len() > k3d::NAME_MAX {
        return Err(ObeliskK8sError::Usage(format!(
            "cluster name `{name}` is too long for k3d: at most {} characters, so its containers' hostnames fit",
            k3d::NAME_MAX
        )));
    }
    Ok(cluster)
}

/// The Docker network managed k3d cluster `cluster` should run on.
fn expected_network(cluster: &Cluster) -> String {
    cluster.obeliskd.as_ref().map_or_else(
        || k3d::default_network(&cluster.name),
        |o| obeliskd::network_name(&o.supernet, &o.subnet),
    )
}

/// What the cluster says that doesn't add up.
fn warnings(cluster: &Cluster) -> Vec<String> {
    let context = k3d::context(&cluster.name);
    cluster
        .kubectl_context
        .as_ref()
        .filter(|c| **c != context)
        .map(|set| {
            format!(
                "cluster `{}`: `kubectlContext` is `{set}`, but k3d names its context `{context}`",
                cluster.name
            )
        })
        .into_iter()
        .collect()
}

fn foreign(name: &str) -> ObeliskK8sError {
    ObeliskK8sError::Usage(format!(
        "a k3d cluster `{name}` exists that obelisk-k8s didn't create: delete it yourself (`k3d cluster delete {name}`), or name the managed cluster something else"
    ))
}

/// What `up` has to do, given the k3d cluster of that name (if any).
fn plan_up(name: &str, existing: Option<&Existing>) -> Result<Outcome, ObeliskK8sError> {
    match existing {
        None => Ok(Outcome::Created),
        Some(Existing { ours: false, .. }) => Err(foreign(name)),
        Some(Existing { running: false, .. }) => Ok(Outcome::Started),
        Some(Existing { running: true, .. }) => Ok(Outcome::AlreadyUp),
    }
}

/// What `down` has to do, given the k3d cluster of that name (if any).
fn plan_down(name: &str, existing: Option<&Existing>) -> Result<Outcome, ObeliskK8sError> {
    match existing {
        None => Ok(Outcome::Missing),
        Some(Existing { ours: false, .. }) => Err(foreign(name)),
        Some(Existing { running: true, .. }) => Ok(Outcome::Stopped),
        Some(Existing { running: false, .. }) => Ok(Outcome::AlreadyDown),
    }
}

/// What `destroy` has to do, given the k3d cluster of that name (if any).
fn plan_destroy(name: &str, existing: Option<&Existing>) -> Result<Outcome, ObeliskK8sError> {
    match existing {
        None => Ok(Outcome::Missing),
        Some(Existing { ours: false, .. }) => Err(foreign(name)),
        Some(Existing { ours: true, .. }) => Ok(Outcome::Destroyed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing(ours: bool, running: bool) -> Existing {
        Existing {
            ours,
            running,
            network: "obd-main-x".to_owned(),
        }
    }

    fn config(clusters: &[(&str, &str)]) -> (tempfile::TempDir, GlobalConfig) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in clusters {
            let path = dir.path().join(format!("clusters/{name}/cluster.yaml"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("cluster:\n{body}")).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("clusters")).unwrap();
        let config = GlobalConfig::load(dir.path(), None).unwrap();
        (dir, config)
    }

    const K3D: &str = "  k8s: k3d\n  location: local\n  managed: true\n";

    #[test]
    fn only_managed_local_k3d_clusters() {
        let long = "a".repeat(k3d::NAME_MAX + 1);
        let (_dir, config) = config(&[
            ("k", K3D),
            ("u", "  k8s: k3d\n  location: local\n  managed: false\n"),
            (
                "t",
                "  k8s: talos\n  location: remote\n  managed: true\n  kubectlContext: x\n",
            ),
            (&long, K3D),
        ]);
        assert!(managed_k3d(&config, "k").is_ok());
        let error = |name| managed_k3d(&config, name).unwrap_err().to_string();
        assert!(
            error("nope").starts_with("no cluster `nope` (there are: "),
            "{}",
            error("nope")
        );
        assert!(
            error("u").starts_with("cluster `u` isn't managed"),
            "{}",
            error("u")
        );
        assert_eq!(
            error("t"),
            "cluster `t` is talos on remote: obelisk-k8s can only bring up local k3d clusters so far"
        );
        assert!(
            error(&long).contains("is too long for k3d"),
            "{}",
            error(&long)
        );
    }

    #[test]
    fn warns_about_a_context_k3d_wont_make() {
        let (_dir, config) = config(&[
            ("plain", K3D),
            ("same", &format!("{K3D}  kubectlContext: k3d-same\n")),
            ("odd", &format!("{K3D}  kubectlContext: mine\n")),
        ]);
        let warnings = |name| warnings(config.cluster(name).unwrap());
        assert_eq!(warnings("plain"), Vec::<String>::new());
        assert_eq!(warnings("same"), Vec::<String>::new());
        assert_eq!(
            warnings("odd"),
            ["cluster `odd`: `kubectlContext` is `mine`, but k3d names its context `k3d-odd`"]
        );
    }

    #[test]
    fn expects_obeliskds_network_or_k3ds_own() {
        let (_dir, config) = config(&[
            ("plain", K3D),
            (
                "obd",
                &format!("{K3D}  obeliskd: {{ supernet: main, subnet: x }}\n"),
            ),
        ]);
        assert_eq!(
            expected_network(config.cluster("plain").unwrap()),
            "k3d-plain"
        );
        assert_eq!(
            expected_network(config.cluster("obd").unwrap()),
            "obd-main-x"
        );
    }

    #[test]
    fn status_shows_what_k3d_says() {
        let (_dir, config) = config(&[
            ("up", K3D),
            (
                "gone",
                &format!("{K3D}  obeliskd: {{ supernet: main, subnet: x }}\n"),
            ),
            ("hand", "  k8s: k3d\n  location: local\n  managed: false\n"),
            (
                "remote",
                "  k8s: talos\n  location: remote\n  managed: false\n  kubectlContext: admin@r\n",
            ),
        ]);
        let up = existing(true, true);
        let theirs = existing(false, true);
        let report = StatusReport {
            statuses: vec![
                status_of(config.cluster("up").unwrap(), Some(&up)),
                // Missing: no context or network, whatever the config says it would be.
                status_of(config.cluster("gone").unwrap(), None),
                status_of(config.cluster("hand").unwrap(), Some(&theirs)),
                status_of(config.cluster("remote").unwrap(), None),
            ],
            warnings: Vec::new(),
        };
        assert_eq!(
            report.to_string(),
            "NAME     KIND           MANAGED   STATE     CONTEXT    NETWORK\n\
             up       k3d/local      yes       running   k3d-up     obd-main-x\n\
             gone     k3d/local      yes       missing   -          -\n\
             hand     k3d/local      no        -         k3d-hand   obd-main-x\n\
             remote   talos/remote   no        -         admin@r    -\n"
        );
    }

    #[test]
    fn status_doesnt_ask_k3d_without_a_local_k3d_cluster() {
        // So this runs anywhere.
        let (_dir, config) = config(&[(
            "remote",
            "  k8s: talos\n  location: remote\n  managed: false\n",
        )]);
        assert_eq!(status(&config, None).unwrap().statuses.len(), 1);
        assert!(status(&config, Some("nope")).is_err());
    }

    #[test]
    fn states() {
        assert_eq!(State::of(None), State::Missing);
        assert_eq!(State::of(Some(&existing(true, true))), State::Running);
        assert_eq!(State::of(Some(&existing(true, false))), State::Stopped);
        assert_eq!(State::of(Some(&existing(false, true))), State::NotOurs);
    }

    #[test]
    fn plans() {
        let (up, stopped, theirs) = (
            existing(true, true),
            existing(true, false),
            existing(false, true),
        );
        assert_eq!(plan_up("c", None).unwrap(), Outcome::Created);
        assert_eq!(plan_up("c", Some(&up)).unwrap(), Outcome::AlreadyUp);
        assert_eq!(plan_up("c", Some(&stopped)).unwrap(), Outcome::Started);
        let error = plan_up("c", Some(&theirs)).unwrap_err().to_string();
        assert!(
            error.starts_with("a k3d cluster `c` exists that obelisk-k8s didn't create"),
            "{error}"
        );

        assert_eq!(plan_down("c", None).unwrap(), Outcome::Missing);
        assert_eq!(plan_down("c", Some(&up)).unwrap(), Outcome::Stopped);
        assert_eq!(
            plan_down("c", Some(&stopped)).unwrap(),
            Outcome::AlreadyDown
        );
        assert!(plan_down("c", Some(&theirs)).is_err());

        assert_eq!(plan_destroy("c", None).unwrap(), Outcome::Missing);
        assert_eq!(plan_destroy("c", Some(&up)).unwrap(), Outcome::Destroyed);
        assert_eq!(
            plan_destroy("c", Some(&stopped)).unwrap(),
            Outcome::Destroyed
        );
        assert!(plan_destroy("c", Some(&theirs)).is_err());
    }
}
