//! Asking obeliskd (`obeliskd` CLI, talking to its daemon) for what local clusters need: the docker
//! subnet a cluster runs on, and the ips and hostnames its base claims (and releases).
//!
//! A managed k3d cluster with `obeliskd: { supernet, subnet }` runs on that docker subnet: obeliskd
//! has already made it a Docker network (`obd-<supernet>-<subnet>`), whose first block Docker hands
//! out to containers and whose rest obeliskd hands out as named ips (e.g. for `MetalLB`).

use std::process::Command;

use serde::Deserialize;

use crate::ObeliskK8sError;

#[derive(Deserialize)]
struct Supernet {
    name: String,
    subnets: Vec<Subnet>,
}

#[derive(Deserialize)]
struct Subnet {
    name: String,
    kind: String,
}

/// The Docker network of docker subnet `subnet` of supernet `supernet`, checked with obeliskd and
/// Docker (without it, k3d would quietly make a network of that name itself).
///
/// # Errors
///
/// [`ObeliskK8sError::Obeliskd`] if `obeliskd` can't be run or fails; [`ObeliskK8sError::Usage`] if
/// there's no such subnet (naming the command that allocates one), it isn't a docker subnet, or its
/// network isn't there; [`ObeliskK8sError::K3d`] if `docker` can't be run.
pub(crate) fn docker_network(supernet: &str, subnet: &str) -> Result<String, ObeliskK8sError> {
    let supernets: Vec<Supernet> = json(
        &run(&["subnet", "list", "--format", "json"])?,
        "subnet list",
    )?;
    let network = find(&supernets, supernet, subnet)?;
    if !crate::k3d::network_exists(&network)? {
        return Err(ObeliskK8sError::Usage(format!(
            "obeliskd has subnet `{subnet}` in supernet `{supernet}`, but there's no Docker network `{network}` for it: is the obeliskd daemon running?"
        )));
    }
    Ok(network)
}

/// An ip, and the hostnames pointing at it, that a cluster needs from obeliskd.
#[derive(Debug)]
pub(crate) struct IpClaim<'a> {
    pub(crate) supernet: &'a str,
    pub(crate) subnet: &'a str,
    /// Its name in the subnet.
    pub(crate) ip: &'a str,
    /// Its tag: [`release`] frees every ip with it in the subnet.
    pub(crate) tag: &'a str,
    pub(crate) hosts: &'a [String],
}

#[derive(Deserialize)]
struct Listed {
    name: String,
    subnets: Vec<ListedSubnet>,
}

#[derive(Deserialize)]
struct ListedSubnet {
    name: String,
    #[serde(default)]
    ips: Vec<ListedIp>,
}

#[derive(Deserialize)]
struct ListedIp {
    name: String,
    address: String,
    tag: Option<String>,
    #[serde(default)]
    hosts: Vec<String>,
}

/// Makes sure obeliskd has `claim`'s ip (allocating it, tagged, if it's missing) and that each of
/// its hosts points at it; returns its address.
///
/// # Errors
///
/// [`ObeliskK8sError::Obeliskd`] if `obeliskd` can't be run or fails (e.g. a host already points at
/// another ip), or the ip exists with another tag.
pub(crate) fn claim(claim: &IpClaim<'_>) -> Result<String, ObeliskK8sError> {
    if listed_ip(claim)?.is_none() {
        let allocated = run(&[
            "ip",
            "alloc",
            "--supernet",
            claim.supernet,
            "--subnet",
            claim.subnet,
            "--ip",
            claim.ip,
            "--tag",
            claim.tag,
        ]);
        // Another run may have allocated it between the listing and now: then it's there to use.
        match allocated {
            Err(ObeliskK8sError::Obeliskd(message)) if message.contains("already exists") => {}
            other => {
                other?;
            }
        }
    }
    let ip = listed_ip(claim)?.ok_or_else(|| {
        ObeliskK8sError::Obeliskd(format!(
            "obeliskd allocated ip `{}`, but doesn't list it",
            claim.ip
        ))
    })?;
    for host in missing_hosts(claim, &ip)? {
        run(&[
            "host",
            "alloc",
            "--host",
            host,
            "--supernet",
            claim.supernet,
            "--subnet",
            claim.subnet,
            "--ip",
            claim.ip,
        ])?;
    }
    Ok(ip.address)
}

/// The hosts `claim` wants that `ip` (its ip, as obeliskd lists it) doesn't have yet.
///
/// # Errors
///
/// [`ObeliskK8sError::Obeliskd`] if `ip` has another tag: it isn't the claimant's.
fn missing_hosts<'a>(
    claim: &IpClaim<'a>,
    ip: &ListedIp,
) -> Result<Vec<&'a String>, ObeliskK8sError> {
    if ip.tag.as_deref() != Some(claim.tag) {
        return Err(ObeliskK8sError::Obeliskd(format!(
            "obeliskd ip `{}` in {}/{} is tagged {}, not `{}`: it isn't this cluster's",
            claim.ip,
            claim.supernet,
            claim.subnet,
            ip.tag
                .as_ref()
                .map_or_else(|| "nothing".to_owned(), |t| format!("`{t}`")),
            claim.tag
        )));
    }
    Ok(claim
        .hosts
        .iter()
        .filter(|h| !ip.hosts.contains(h))
        .collect())
}

fn listed_ip(claim: &IpClaim<'_>) -> Result<Option<ListedIp>, ObeliskK8sError> {
    let listed: Vec<Listed> = json(&run(&["host", "list", "--format", "json"])?, "host list")?;
    Ok(find_ip(listed, claim))
}

fn find_ip(listed: Vec<Listed>, claim: &IpClaim<'_>) -> Option<ListedIp> {
    listed
        .into_iter()
        .filter(|s| s.name == claim.supernet)
        .flat_map(|s| s.subnets)
        .filter(|s| s.name == claim.subnet)
        .flat_map(|s| s.ips)
        .find(|ip| ip.name == claim.ip)
}

/// Frees every ip tagged `tag` in subnet `subnet` of `supernet`, and the hosts pointing at them (a
/// no-op if there are none). Tags are global in obeliskd, so this is scoped to the subnet: the same
/// tag elsewhere is someone else's.
///
/// # Errors
///
/// [`ObeliskK8sError::Obeliskd`] if `obeliskd` can't be run or fails.
pub(crate) fn release(supernet: &str, subnet: &str, tag: &str) -> Result<(), ObeliskK8sError> {
    let listed: Vec<Listed> = json(&run(&["host", "list", "--format", "json"])?, "host list")?;
    if has_tag(&listed, supernet, subnet, tag) {
        run(&release_args(supernet, subnet, tag))?;
    }
    Ok(())
}

fn release_args<'a>(supernet: &'a str, subnet: &'a str, tag: &'a str) -> [&'a str; 9] {
    [
        "ip",
        "dealloc",
        "--tag",
        tag,
        "--supernet",
        supernet,
        "--subnet",
        subnet,
        "--recursive",
    ]
}

fn has_tag(listed: &[Listed], supernet: &str, subnet: &str, tag: &str) -> bool {
    listed
        .iter()
        .filter(|s| s.name == supernet)
        .flat_map(|s| &s.subnets)
        .filter(|s| s.name == subnet)
        .flat_map(|s| &s.ips)
        .any(|ip| ip.tag.as_deref() == Some(tag))
}

/// Runs `obeliskd` with `args` and returns its stdout.
fn run(args: &[&str]) -> Result<Vec<u8>, ObeliskK8sError> {
    let shown = format!(
        "obeliskd {}",
        args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
    );
    let output = Command::new("obeliskd")
        .args(args)
        .output()
        .map_err(|error| {
            ObeliskK8sError::Obeliskd(format!("can't run `obeliskd` (is it installed?): {error}"))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ObeliskK8sError::Obeliskd(format!(
            "`{shown}` failed: {}",
            stderr.trim().trim_start_matches("ERROR - ")
        )));
    }
    Ok(output.stdout)
}

fn json<T: serde::de::DeserializeOwned>(out: &[u8], what: &str) -> Result<T, ObeliskK8sError> {
    serde_json::from_slice(out).map_err(|error| {
        ObeliskK8sError::Obeliskd(format!(
            "`obeliskd {what}` printed something unexpected: {error}"
        ))
    })
}

/// The Docker network obeliskd makes for docker subnet `subnet` of `supernet`.
pub(crate) fn network_name(supernet: &str, subnet: &str) -> String {
    format!("obd-{supernet}-{subnet}")
}

/// The network of docker subnet `subnet` of `supernet`, if obeliskd's listing has it.
fn find(supernets: &[Supernet], supernet: &str, subnet: &str) -> Result<String, ObeliskK8sError> {
    let Some(found) = supernets
        .iter()
        .find(|s| s.name == supernet)
        .and_then(|s| s.subnets.iter().find(|s| s.name == subnet))
    else {
        return Err(ObeliskK8sError::Usage(format!(
            "obeliskd has no subnet `{subnet}` in supernet `{supernet}`: allocate one, e.g. `obeliskd subnet alloc --supernet {supernet} --subnet {subnet} --kind docker --size 22 --reserve-size 24`"
        )));
    };
    if found.kind != "docker" {
        return Err(ObeliskK8sError::Usage(format!(
            "obeliskd subnet `{subnet}` in supernet `{supernet}` is a `{}` subnet, but a k3d cluster needs a `docker` one",
            found.kind
        )));
    }
    Ok(network_name(supernet, subnet))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supernets() -> Vec<Supernet> {
        serde_json::from_str(
            r#"[{"name":"obd","cidr":"10.57.0.0/16","subnets":[
                {"name":"k3d-dev","kind":"docker","cidr":"10.57.0.0/22","dockerRange":"10.57.0.0/24","dockerGateway":"10.57.0.1"},
                {"name":"lan","kind":"host","cidr":"10.57.4.0/24"}]}]"#,
        )
        .unwrap()
    }

    /// Trimmed from `obeliskd host list --format json`.
    const HOSTS: &str = r#"[{"name":"main","cidr":"10.57.0.0/16","subnets":[
        {"name":"k","kind":"docker","cidr":"10.57.0.0/22","ips":[
            {"name":"k-gateway","address":"10.57.0.16","tag":"k","hosts":["k.obd.internal"]},
            {"name":"stray","address":"10.57.0.17","hosts":[]}]},
        {"name":"empty","kind":"docker","cidr":"10.57.4.0/22"}]}]"#;

    fn listed() -> Vec<Listed> {
        serde_json::from_str(HOSTS).unwrap()
    }

    fn claim_for<'a>(ip: &'a str, hosts: &'a [String]) -> IpClaim<'a> {
        IpClaim {
            supernet: "main",
            subnet: "k",
            ip,
            tag: "k",
            hosts,
        }
    }

    #[test]
    fn claims_only_the_hosts_that_are_missing() {
        let hosts = ["k.obd.internal".to_owned(), "*.k.obd.internal".to_owned()];
        let claim = claim_for("k-gateway", &hosts);
        let ip = find_ip(listed(), &claim).unwrap();
        assert_eq!(ip.address, "10.57.0.16");
        assert_eq!(missing_hosts(&claim, &ip).unwrap(), [&hosts[1]]);
        assert!(find_ip(listed(), &claim_for("nope", &hosts)).is_none());
    }

    #[test]
    fn wont_take_over_someone_elses_ip() {
        let claim = claim_for("stray", &[]);
        let ip = find_ip(listed(), &claim).unwrap();
        let error = missing_hosts(&claim, &ip).unwrap_err().to_string();
        assert_eq!(
            error,
            "obeliskd ip `stray` in main/k is tagged nothing, not `k`: it isn't this cluster's"
        );
    }

    #[test]
    fn releases_only_the_tag_in_the_clusters_subnet() {
        assert!(has_tag(&listed(), "main", "k", "k"));
        assert!(!has_tag(&listed(), "main", "k", "other"));
        assert!(!has_tag(&listed(), "main", "empty", "k"), "another subnet");
        assert!(!has_tag(&listed(), "other", "k", "k"), "another supernet");
        assert_eq!(
            release_args("main", "k", "k").join(" "),
            "ip dealloc --tag k --supernet main --subnet k --recursive"
        );
    }

    #[test]
    fn finds_a_docker_subnet() {
        assert_eq!(
            find(&supernets(), "obd", "k3d-dev").unwrap(),
            "obd-obd-k3d-dev"
        );
    }

    #[test]
    fn says_how_to_allocate_a_missing_one() {
        for (supernet, subnet) in [("obd", "nope"), ("nope", "k3d-dev")] {
            let error = find(&supernets(), supernet, subnet)
                .unwrap_err()
                .to_string();
            assert!(
                error.ends_with(&format!(
                    "`obeliskd subnet alloc --supernet {supernet} --subnet {subnet} --kind docker --size 22 --reserve-size 24`"
                )),
                "{error}"
            );
        }
        let error = find(&supernets(), "obd", "lan").unwrap_err().to_string();
        assert!(error.contains("is a `host` subnet"), "{error}");
    }
}
