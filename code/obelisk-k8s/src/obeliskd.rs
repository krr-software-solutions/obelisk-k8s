//! Asking obeliskd (`obeliskd` CLI, talking to its daemon) about the subnets local clusters run on.
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
    let output = Command::new("obeliskd")
        .args(["subnet", "list", "--format", "json"])
        .output()
        .map_err(|error| {
            ObeliskK8sError::Obeliskd(format!("can't run `obeliskd` (is it installed?): {error}"))
        })?;
    if !output.status.success() {
        return Err(ObeliskK8sError::Obeliskd(format!(
            "`obeliskd subnet list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let supernets: Vec<Supernet> = serde_json::from_slice(&output.stdout).map_err(|error| {
        ObeliskK8sError::Obeliskd(format!(
            "`obeliskd subnet list` printed something unexpected: {error}"
        ))
    })?;
    let network = find(&supernets, supernet, subnet)?;
    if !crate::k3d::network_exists(&network)? {
        return Err(ObeliskK8sError::Usage(format!(
            "obeliskd has subnet `{subnet}` in supernet `{supernet}`, but there's no Docker network `{network}` for it: is the obeliskd daemon running?"
        )));
    }
    Ok(network)
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
