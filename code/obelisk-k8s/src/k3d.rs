//! Running k3d: the local clusters obelisk-k8s creates, starts, stops and deletes.
//!
//! A cluster obelisk-k8s creates carries the [`OWNER_LABEL`] Docker label on its first server, so it
//! never deletes (or adopts) a k3d cluster someone made by hand under the same name.

use std::process::Command;

use serde::Deserialize;

use crate::ObeliskK8sError;

/// The Docker label marking a k3d cluster as obelisk-k8s's (on its first server's container).
pub(crate) const OWNER_LABEL: &str = "obelisk-k8s.managed";

/// The kubectl context k3d makes for cluster `name`.
pub(crate) fn context(name: &str) -> String {
    format!("k3d-{name}")
}

/// The Docker network k3d makes for cluster `name` when it isn't given one.
pub(crate) fn default_network(name: &str) -> String {
    format!("k3d-{name}")
}

/// Cluster `name`'s first server container, whose name is also its hostname.
fn server_container(name: &str) -> String {
    format!("k3d-{name}-server-0")
}

/// How long a cluster name can be: its server containers' hostnames (`k3d-<name>-server-0`) must
/// fit in Linux's 64.
pub(crate) const NAME_MAX: usize = 64 - "k3d--server-0".len() - 1;

/// A k3d cluster that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Existing {
    /// obelisk-k8s created it.
    pub(crate) ours: bool,
    /// All its servers are running (with some stopped, it's not: `up` starts the rest).
    pub(crate) running: bool,
    /// The Docker network it runs on.
    pub(crate) network: String,
}

/// The k3d cluster named `name`, if there is one.
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `k3d` (or `docker`) can't be run, fails, or prints something unexpected.
pub(crate) fn find(name: &str) -> Result<Option<Existing>, ObeliskK8sError> {
    list()?.find(name)
}

/// Every k3d cluster, as `k3d cluster list` says (asking Docker whose each is only on [`List::find`]).
#[derive(Debug, Default)]
pub(crate) struct List(Vec<Listed>);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    name: String,
    network: ListedNetwork,
    servers_running: u32,
    servers_count: u32,
}

#[derive(Debug, Deserialize)]
struct ListedNetwork {
    name: String,
}

/// Lists the k3d clusters.
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `k3d` can't be run, fails, or prints something unexpected.
pub(crate) fn list() -> Result<List, ObeliskK8sError> {
    parse_list(&run("k3d", &["cluster", "list", "--output", "json"])?)
}

fn parse_list(json: &str) -> Result<List, ObeliskK8sError> {
    // `null` too, in case k3d prints an empty list that way.
    serde_json::from_str::<Option<Vec<Listed>>>(json)
        .map(|listed| List(listed.unwrap_or_default()))
        .map_err(|error| {
            ObeliskK8sError::K3d(format!(
                "`k3d cluster list` printed something unexpected: {error}"
            ))
        })
}

impl List {
    /// The cluster named `name`, if there is one.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::K3d`] if `docker` can't be run or fails.
    pub(crate) fn find(&self, name: &str) -> Result<Option<Existing>, ObeliskK8sError> {
        let Some(cluster) = self.0.iter().find(|c| c.name == name) else {
            return Ok(None);
        };
        let format = format!("{{{{index .Config.Labels \"{OWNER_LABEL}\"}}}}");
        let label = run(
            "docker",
            &["inspect", "--format", &format, &server_container(name)],
        )?;
        Ok(Some(existing(cluster, &label)))
    }
}

fn existing(cluster: &Listed, owner_label: &str) -> Existing {
    Existing {
        ours: owner_label.trim() == "true",
        running: cluster.servers_count > 0 && cluster.servers_running == cluster.servers_count,
        network: cluster.network.name.clone(),
    }
}

/// Creates k3d cluster `name`, marked as obelisk-k8s's, on Docker network `network` (k3d makes its
/// own without one), and waits for it. Its kubeconfig context ([`context`]) is added to the default
/// kubeconfig, without switching to it.
///
/// k3s's own load balancing and ingress are left out (`MetalLB` and the cluster's own units take
/// their place), and so is k3d's load balancer container: the API server is reached directly.
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `k3d` can't be run or fails.
pub(crate) fn create(name: &str, network: Option<&str>) -> Result<(), ObeliskK8sError> {
    let args = create_args(name, network);
    run("k3d", &args.iter().map(String::as_str).collect::<Vec<_>>()).map(drop)
}

fn create_args(name: &str, network: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "cluster",
        "create",
        name,
        "--runtime-label",
        &format!("{OWNER_LABEL}=true@server:0"),
        "--no-lb",
        "--k3s-arg",
        "--disable=traefik@server:*",
        "--k3s-arg",
        "--disable=servicelb@server:*",
        "--kubeconfig-update-default",
        // Never switch the user's current context (k3d's default is to).
        "--kubeconfig-switch-context=false",
        "--wait",
    ]
    .map(str::to_owned)
    .into();
    if let Some(network) = network {
        args.extend(["--network".to_owned(), network.to_owned()]);
    }
    args
}

/// Starts stopped k3d cluster `name` (it doesn't touch the kubeconfig).
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `k3d` can't be run or fails.
pub(crate) fn start(name: &str) -> Result<(), ObeliskK8sError> {
    run("k3d", &["cluster", "start", name, "--wait"]).map(drop)
}

/// Stops k3d cluster `name`, keeping its containers and state.
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `k3d` can't be run or fails.
pub(crate) fn stop(name: &str) -> Result<(), ObeliskK8sError> {
    run("k3d", &["cluster", "stop", name]).map(drop)
}

/// Deletes k3d cluster `name` (and its kubeconfig context). A network it was given is left alone.
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `k3d` can't be run or fails.
pub(crate) fn delete(name: &str) -> Result<(), ObeliskK8sError> {
    run("k3d", &["cluster", "delete", name]).map(drop)
}

/// Whether Docker network `network` exists.
///
/// # Errors
///
/// [`ObeliskK8sError::K3d`] if `docker` can't be run.
pub(crate) fn network_exists(network: &str) -> Result<bool, ObeliskK8sError> {
    let output = Command::new("docker")
        .args(["network", "inspect", "--format", "{{.Name}}", network])
        .output()
        .map_err(|error| {
            ObeliskK8sError::K3d(format!("can't run `docker` (is it installed?): {error}"))
        })?;
    Ok(output.status.success())
}

/// Runs `program` with `args` and returns its stdout.
fn run(program: &str, args: &[&str]) -> Result<String, ObeliskK8sError> {
    // e.g. `k3d cluster create`: what failed, without every flag.
    let shown = std::iter::once(program)
        .chain(args.iter().copied().take_while(|a| !a.starts_with('-')))
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new(program).args(args).output().map_err(|error| {
        ObeliskK8sError::K3d(format!("can't run `{program}` (is it installed?): {error}"))
    })?;
    if !output.status.success() {
        return Err(ObeliskK8sError::K3d(format!(
            "`{shown}` failed: {}",
            failure(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| ObeliskK8sError::K3d(format!("`{shown}` printed something that isn't UTF-8")))
}

/// What a failed run said, minus colors and k3d's progress lines: its `ERRO` / `FATA` lines when it
/// logged any, else everything.
fn failure(stderr: &str) -> String {
    let plain = strip_colors(stderr);
    let errors: Vec<&str> = plain
        .lines()
        .filter(|l| l.starts_with("ERRO") || l.starts_with("FATA"))
        .collect();
    if errors.is_empty() {
        plain.trim().to_owned()
    } else {
        errors.join("\n")
    }
}

/// `text` without ANSI color sequences (`ESC [ … m`).
fn strip_colors(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_never_switches_the_current_context() {
        for network in [None, Some("obd-main-x")] {
            let args = create_args("c", network);
            assert!(
                args.iter()
                    .any(|a| a == "--kubeconfig-switch-context=false"),
                "{args:?}"
            );
            assert!(
                !args.iter().any(|a| a == "--kubeconfig-switch-context"),
                "{args:?}"
            );
        }
    }

    #[test]
    fn create_marks_the_cluster_and_uses_the_network() {
        let args = create_args("c", Some("obd-main-x"));
        assert!(
            args.contains(&"obelisk-k8s.managed=true@server:0".to_owned()),
            "{args:?}"
        );
        assert!(
            args.ends_with(&["--network".to_owned(), "obd-main-x".to_owned()]),
            "{args:?}"
        );
        assert!(!create_args("c", None).contains(&"--network".to_owned()));
    }

    #[test]
    fn reads_the_cluster_list() {
        // Trimmed from `k3d cluster list --output json` (k3d v5.9.0).
        let list = parse_list(
            r#"[{"name":"a","network":{"name":"obd-main-x","id":"","isExternal":true},"nodes":[],
                 "serversRunning":1,"serversCount":1,"agentsRunning":0,"agentsCount":0},
                {"name":"b","network":{"name":"k3d-b"},"serversRunning":1,"serversCount":3}]"#,
        )
        .unwrap();
        let [a, b] = list.0.as_slice() else {
            panic!("{list:?}")
        };
        assert_eq!(
            existing(a, "true\n"),
            Existing {
                ours: true,
                running: true,
                network: "obd-main-x".to_owned()
            }
        );
        // Not ours without the label; not running with a server stopped.
        assert_eq!(
            existing(b, "\n"),
            Existing {
                ours: false,
                running: false,
                network: "k3d-b".to_owned()
            }
        );
        assert!(parse_list("[]").unwrap().0.is_empty());
        assert!(parse_list("null").unwrap().0.is_empty());
        assert!(parse_list("nope").is_err());
    }

    #[test]
    fn a_failure_is_its_error_lines_without_colors() {
        let stderr = "\x1b[36mINFO\x1b[0m[0000] Prep: Network\n\x1b[31mFATA\x1b[0m[0001] Failed Cluster Creation: boom\n";
        assert_eq!(failure(stderr), "FATA[0001] Failed Cluster Creation: boom");
        assert_eq!(failure("  just this\n"), "just this");
    }

    #[test]
    fn names_fit_in_a_hostname() {
        assert_eq!(server_container(&"a".repeat(NAME_MAX)).len(), 63);
    }
}
