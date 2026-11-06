//! A managed cluster's base: what every cluster `cluster up` brings up gets, so apps on it are
//! reachable by name with nothing but an `HTTPRoute`.
//!
//! - An obeliskd ip for the gateway (`<cluster>-gateway`, tagged with the cluster), and hostnames
//!   `<cluster>.obd.internal` and `*.`, `*.*.`, `*.*.*.<cluster>.obd.internal` pointing at it (a `*`
//!   is exactly one label in obeliskd, hence the three).
//! - `MetalLB`, with a pool of just that ip (`autoAssign: false`, so it never hands out an address
//!   obeliskd doesn't know about).
//! - Envoy Gateway (which brings the Gateway API CRDs), a `GatewayClass` `envoy`, and a `Gateway`
//!   `private-default-gateway` in `envoy-gateway-system`, on that ip, taking HTTP routes from every
//!   namespace: the names the home lab uses, so the same `HTTPRoute` works on both.
//!
//! Every step is idempotent (`helm upgrade --install`, and only what obeliskd is missing), so `up`
//! re-runs it to repair the base. A Helm release that doesn't come up is rolled back (or removed),
//! so the next run starts clean; one left mid-operation by an interrupted run is reported, with how
//! to clear it. Each release may wait up to [`helm::TIMEOUT`], and the gateway as long again.
//!
//! Chart versions are pinned here, and Helm fetches the charts on every run, so `up` needs the
//! network. Helm never updates a chart's CRDs: after bumping a pin, apply the new chart's CRDs by
//! hand (`helm show crds …`) or `destroy` and `up` the cluster.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use crate::config::global::Obeliskd;
use crate::helm::{self, Chart, Install, Takeover};
use crate::{ObeliskK8sError, k3d, obeliskd, yaml};

/// The zone obeliskd answers for.
const ZONE: &str = "obd.internal";

const METALLB: Chart<'static> = Chart::Repo {
    url: "https://metallb.github.io/metallb",
    name: "metallb",
};
const METALLB_VERSION: &str = "0.16.1";
const METALLB_NAMESPACE: &str = "metallb-system";

const ENVOY_GATEWAY: Chart<'static> = Chart::Oci("oci://docker.io/envoyproxy/gateway-helm");
const ENVOY_GATEWAY_VERSION: &str = "1.9.3";
const GATEWAY_NAMESPACE: &str = "envoy-gateway-system";
const GATEWAY: &str = "private-default-gateway";

/// The release holding the base's own resources (the pool, the gateway).
const BASE_RELEASE: &str = "obelisk-base";

/// How often to retry the base's own release while `MetalLB`'s webhook isn't serving yet (5s apart).
const WEBHOOK_RETRIES: u32 = 12;

/// What a cluster's base came to.
#[derive(Debug)]
pub struct Base {
    /// The gateway's address.
    pub address: String,
    /// The hostnames pointing at it.
    pub hosts: Vec<String>,
}

impl std::fmt::Display for Base {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gateway on {}: http://{}",
            self.address,
            self.hosts.join(", http://")
        )
    }
}

/// The hostnames cluster `name`'s gateway answers for.
fn hosts(name: &str) -> Vec<String> {
    let host = format!("{name}.{ZONE}");
    vec![
        host.clone(),
        format!("*.{host}"),
        format!("*.*.{host}"),
        format!("*.*.*.{host}"),
    ]
}

/// Installs (or repairs) cluster `name`'s base, on its obeliskd subnet.
///
/// # Errors
///
/// [`ObeliskK8sError::Obeliskd`], [`ObeliskK8sError::Helm`] or [`ObeliskK8sError::Kubectl`] if a
/// step fails.
pub(crate) fn install(name: &str, subnet: &Obeliskd) -> Result<Base, ObeliskK8sError> {
    let hosts = hosts(name);
    let ip = format!("{name}-gateway");
    let address = obeliskd::claim(&obeliskd::IpClaim {
        supernet: &subnet.supernet,
        subnet: &subnet.subnet,
        ip: &ip,
        tag: name,
        hosts: &hosts,
    })?;
    let context = k3d::context(name);
    let install = |release, chart, version, namespace| {
        helm::upgrade_install(&Install {
            release,
            chart,
            version,
            namespace,
            context: &context,
            takeover: Takeover::default(),
        })
    };
    install("metallb", METALLB, Some(METALLB_VERSION), METALLB_NAMESPACE)?;
    install(
        "envoy-gateway",
        ENVOY_GATEWAY,
        Some(ENVOY_GATEWAY_VERSION),
        GATEWAY_NAMESPACE,
    )?;
    let manifests = manifests(&address)?;
    // MetalLB's webhook can take a moment to serve after its release is ready: retry while it does.
    let mut tries = 0;
    loop {
        match helm::install_manifests(
            BASE_RELEASE,
            "0.0.0",
            &manifests,
            GATEWAY_NAMESPACE,
            &context,
            Takeover::default(),
        ) {
            Err(ObeliskK8sError::Helm(message))
                if message.contains("webhook") && tries < WEBHOOK_RETRIES =>
            {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            result => break result?,
        }
    }
    wait_for_gateway(&context)?;
    Ok(Base { address, hosts })
}

/// Releases what cluster `name`'s base holds outside the cluster: the obeliskd ips tagged with its
/// name in its subnet, and their hostnames.
///
/// # Errors
///
/// [`ObeliskK8sError::Obeliskd`] if obeliskd fails.
pub(crate) fn release(name: &str, subnet: &Obeliskd) -> Result<(), ObeliskK8sError> {
    obeliskd::release(&subnet.supernet, &subnet.subnet, name)
}

/// The base's own resources, for gateway address `address`.
fn manifests(address: &str) -> Result<String, ObeliskK8sError> {
    let pool = "obelisk-gateway";
    let docs: [Value; 5] = [
        json!({
            "apiVersion": "metallb.io/v1beta1",
            "kind": "IPAddressPool",
            "metadata": { "name": pool, "namespace": METALLB_NAMESPACE },
            "spec": { "addresses": [format!("{address}/32")], "autoAssign": false },
        }),
        json!({
            "apiVersion": "metallb.io/v1beta1",
            "kind": "L2Advertisement",
            "metadata": { "name": pool, "namespace": METALLB_NAMESPACE },
            "spec": { "ipAddressPools": [pool] },
        }),
        json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "GatewayClass",
            "metadata": { "name": "envoy" },
            "spec": { "controllerName": "gateway.envoyproxy.io/gatewayclass-controller" },
        }),
        json!({
            "apiVersion": "gateway.envoyproxy.io/v1alpha1",
            "kind": "EnvoyProxy",
            "metadata": { "name": format!("{GATEWAY}-config"), "namespace": GATEWAY_NAMESPACE },
            "spec": {
                "provider": {
                    "type": "Kubernetes",
                    "kubernetes": {
                        "envoyService": {
                            "type": "LoadBalancer",
                            "name": GATEWAY,
                            "annotations": { "metallb.io/loadBalancerIPs": address },
                        },
                    },
                },
            },
        }),
        json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "Gateway",
            "metadata": { "name": GATEWAY, "namespace": GATEWAY_NAMESPACE },
            "spec": {
                "gatewayClassName": "envoy",
                "infrastructure": {
                    "parametersRef": {
                        "group": "gateway.envoyproxy.io",
                        "kind": "EnvoyProxy",
                        "name": format!("{GATEWAY}-config"),
                    },
                },
                "listeners": [{
                    "name": "http",
                    "protocol": "HTTP",
                    "port": 80,
                    "allowedRoutes": { "namespaces": { "from": "All" } },
                }],
            },
        }),
    ];
    let docs = docs
        .iter()
        .map(|doc| yaml::to_string(doc, Path::new("the cluster base")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(docs.join("---\n"))
}

/// Waits for the gateway to be programmed (its Envoy running, on its address).
fn wait_for_gateway(context: &str) -> Result<(), ObeliskK8sError> {
    let output = Command::new("kubectl")
        .args(["--context", context, "--namespace", GATEWAY_NAMESPACE])
        .args(["wait", "--for=condition=Programmed"])
        .arg(format!("--timeout={}", helm::TIMEOUT))
        .arg(format!("gateway/{GATEWAY}"))
        .output()
        .map_err(|error| {
            ObeliskK8sError::Kubectl(format!("can't run `kubectl` (is it installed?): {error}"))
        })?;
    if !output.status.success() {
        return Err(ObeliskK8sError::Kubectl(format!(
            "gateway `{GATEWAY}` on `{context}` didn't come up: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_for_the_cluster_and_three_levels_below() {
        assert_eq!(
            hosts("krrss"),
            [
                "krrss.obd.internal",
                "*.krrss.obd.internal",
                "*.*.krrss.obd.internal",
                "*.*.*.krrss.obd.internal",
            ]
        );
    }

    #[test]
    fn pins_the_gateway_to_its_address_and_nothing_else() {
        let text = manifests("10.57.0.16").unwrap();
        let docs = yaml::parse_values_stream(&text, Path::new("base")).unwrap();
        let kinds: Vec<&str> = docs.iter().map(|d| d["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "IPAddressPool",
                "L2Advertisement",
                "GatewayClass",
                "EnvoyProxy",
                "Gateway"
            ]
        );
        assert_eq!(docs[0]["spec"]["addresses"], json!(["10.57.0.16/32"]));
        assert_eq!(docs[0]["spec"]["autoAssign"], false);
        assert_eq!(
            docs[3]["spec"]["provider"]["kubernetes"]["envoyService"]["annotations"]["metallb.io/loadBalancerIPs"],
            "10.57.0.16"
        );
        assert_eq!(docs[4]["metadata"]["name"], "private-default-gateway");
        // The pieces name each other.
        assert_eq!(
            docs[1]["spec"]["ipAddressPools"],
            json!([docs[0]["metadata"]["name"]])
        );
        assert_eq!(
            docs[4]["spec"]["infrastructure"]["parametersRef"]["name"],
            docs[3]["metadata"]["name"]
        );
        assert_eq!(
            docs[4]["spec"]["gatewayClassName"],
            docs[2]["metadata"]["name"]
        );
        assert_eq!(
            docs[3]["metadata"]["namespace"],
            docs[4]["metadata"]["namespace"]
        );
    }
}
