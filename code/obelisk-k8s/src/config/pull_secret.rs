//! `images pull-secret --cluster <name> --namespace <ns>`: what obelisk-local's `docker
//! add-pull-config` did. In namespace `<ns>` on cluster `<name>` (from the global config, through
//! its kubectl context) it server-side applies (field manager [`FIELD_MANAGER`], outside any Helm
//! release) two objects:
//!
//! - the Secret [`SECRET_NAME`] (fixed: obelisk-local's default): a
//!   `kubernetes.io/dockerconfigjson`, `{ "auths": { <host>: { username, password } } }` from the
//!   cluster's `docker.yaml` `pull.auths`, base64 in `data` (not `stringData`, which the API server folds into `data`: a
//!   server-side apply of it always conflicts with the manager that set it before);
//! - the namespace's `default` `ServiceAccount`, with `imagePullSecrets: [{ name: docker-pull }]`,
//!   so its pods pull with them without their charts saying so. That list is atomic to a
//!   server-side apply: it's all ours, so another manager's entries in it are a conflict (and
//!   `--force-conflicts` replaces them).
//!
//! The credentials' SSM refs are resolved; `--dry-run` shows them as written (`stringData`, for
//! reading), and any password that isn't just one ref masked. The namespace must exist. A remote cluster needs `--yes` (but not for `--dry-run`).

use std::fmt;
use std::io::Write as _;
use std::process::{Command, Stdio};

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::ObeliskK8sError;
use crate::config::global::docker::Docker;
use crate::config::global::{GlobalConfig, Location};
use crate::ssm::{self, Ssm};
use crate::yaml;

/// The Secret's name: obelisk-local's, so it takes over the ones it made.
pub const SECRET_NAME: &str = "docker-pull";

/// Who the server-side apply is by.
pub const FIELD_MANAGER: &str = "obelisk-k8s";

/// What `images pull-secret` applied (or, `--dry-run`, would apply).
#[derive(Debug)]
pub struct PullSecret<'g> {
    pub cluster: &'g str,
    pub namespace: String,
    /// The hosts it has credentials for.
    pub hosts: Vec<String>,
    /// The two objects as they're shown: credentials as written in `docker.yaml` (SSM refs as
    /// refs), other passwords masked.
    pub shown: String,
    pub dry_run: bool,
}

/// How `images pull-secret` runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct PullSecretOptions {
    /// Apply to a remote cluster (`--yes`).
    pub remote_ok: bool,
    /// Take fields another field manager owns (`--force-conflicts`).
    pub force_conflicts: bool,
    /// Print, and apply nothing (`--dry-run`).
    pub dry_run: bool,
}

impl GlobalConfig {
    /// Applies (unless `options.dry_run`) the pull secret in `namespace` on cluster `cluster`, `ssm`
    /// resolving its credentials (even for a dry run, so they're checked); `step` hears each step
    /// before it starts.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if `namespace` isn't a namespace name, there's no such cluster,
    /// or it has no `pull.auths`, or (unless a dry run) it's remote without `options.remote_ok` or
    /// has no kubectl context; [`ObeliskK8sError::Ssm`] if the credentials don't resolve;
    /// [`ObeliskK8sError::Kubectl`] if `kubectl apply` fails.
    pub fn pull_secret(
        &self,
        cluster: &str,
        namespace: &str,
        options: PullSecretOptions,
        ssm: &Ssm,
        mut step: impl FnMut(&str),
    ) -> Result<PullSecret<'_>, ObeliskK8sError> {
        check_namespace(namespace)?;
        let cluster = self.cluster(cluster)?;
        let docker = cluster
            .docker
            .as_ref()
            .filter(|d| !d.pull_auths.is_empty())
            .ok_or_else(|| {
                ObeliskK8sError::Usage(format!(
                    "cluster `{}` has no `docker.pull.auths` (in {}): nothing to pull with",
                    cluster.name,
                    cluster
                        .file
                        .with_file_name(crate::config::global::docker::DOCKER_FILE)
                        .display()
                ))
            })?;
        if cluster.location == Location::Remote && !options.remote_ok && !options.dry_run {
            return Err(ObeliskK8sError::Usage(format!(
                "cluster `{}` is remote: pass `--yes` to apply to it",
                cluster.name
            )));
        }
        let resolved = manifest(docker, namespace, Credentials::Resolved(ssm))?;
        let shown = manifest(docker, namespace, Credentials::Shown)?;
        if !options.dry_run {
            let context = cluster.context().ok_or_else(|| {
                ObeliskK8sError::Usage(format!(
                    "cluster `{}` has no `kubectlContext` (in {}): set one to apply to it",
                    cluster.name,
                    cluster.file.display()
                ))
            })?;
            step(&format!(
                "Applying pull secret `{SECRET_NAME}` in namespace `{namespace}` on cluster `{}`",
                cluster.name
            ));
            let said = kubectl_apply(&context, namespace, &resolved, options.force_conflicts)?;
            for line in said.lines().filter(|l| !l.trim().is_empty()) {
                step(line);
            }
        }
        Ok(PullSecret {
            cluster: &cluster.name,
            namespace: namespace.to_owned(),
            hosts: docker.pull_auths.iter().map(|a| a.host.clone()).collect(),
            shown,
            dry_run: options.dry_run,
        })
    }
}

/// A namespace name: an RFC 1123 label (else `kubectl` might quietly use its context's namespace).
fn check_namespace(namespace: &str) -> Result<(), ObeliskK8sError> {
    let ok = !namespace.is_empty()
        && namespace.len() <= 63
        && namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !namespace.starts_with('-')
        && !namespace.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(ObeliskK8sError::Usage(format!(
            "`{namespace}` isn't a namespace name: at most 63 lowercase letters, digits and `-`, starting and ending with a letter or digit"
        )))
    }
}

/// Which credentials go in the Secret.
#[derive(Clone, Copy)]
enum Credentials<'s> {
    /// As they're applied: SSM refs resolved with this.
    Resolved(&'s Ssm),
    /// As they're shown: as written, but a password masked unless it's just one SSM ref.
    Shown,
}

/// The mask for a password that isn't just one SSM ref (some of it is in `docker.yaml` itself).
const MASKED: &str = "<masked>";

/// The Secret and the `default` `ServiceAccount` for `namespace`, as YAML, with `credentials`. The
/// Secret's `.dockerconfigjson` is base64 in `data`, but shown (`Credentials::Shown`) as plain JSON
/// in `stringData`, so the reader can see what's in it.
fn manifest(
    docker: &Docker,
    namespace: &str,
    credentials: Credentials<'_>,
) -> Result<String, ObeliskK8sError> {
    let mut auths: Vec<(String, Value)> = docker
        .pull_auths
        .iter()
        .map(|auth| {
            let password = &auth.credentials.password;
            let password = match credentials {
                Credentials::Shown if !ssm::is_one_ref(password) => MASKED.to_owned(),
                _ => password.clone(),
            };
            let value = serde_json::json!({
                "username": auth.credentials.username,
                "password": password,
            });
            (auth.host.clone(), value)
        })
        .collect();
    let what = |host: &str| format!("{}, at the pull auth for `{host}`", docker.file.display());
    let resolve = match credentials {
        Credentials::Resolved(ssm) => Some(ssm),
        Credentials::Shown => None,
    };
    ssm::resolve_or_check(resolve, auths.iter_mut().map(|(host, v)| (what(host), v)))?;
    let config = serde_json::json!({ "auths": auths.into_iter().collect::<Map<String, Value>>() })
        .to_string();
    let data = match credentials {
        Credentials::Resolved(_) => serde_json::json!({
            "data": {
                ".dockerconfigjson": base64::engine::general_purpose::STANDARD.encode(config),
            },
        }),
        Credentials::Shown => serde_json::json!({ "stringData": { ".dockerconfigjson": config } }),
    };
    let mut secret = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": SECRET_NAME,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/name": SECRET_NAME },
        },
        "type": "kubernetes.io/dockerconfigjson",
    });
    if let (Value::Object(secret), Value::Object(data)) = (&mut secret, data) {
        secret.extend(data);
    }
    let account = serde_json::json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": { "name": "default", "namespace": namespace },
        "imagePullSecrets": [{ "name": SECRET_NAME }],
    });
    let mut text = String::new();
    for doc in [secret, account] {
        text.push_str("---\n");
        text.push_str(&yaml::to_string(&doc, &docker.file)?);
    }
    Ok(text)
}

/// The `kubectl` arguments that server-side apply a manifest (on stdin) on `context`, in
/// `namespace`.
fn apply_args(context: &str, namespace: &str, force_conflicts: bool) -> Vec<String> {
    let mut args: Vec<String> = ["--context", context, "--namespace", namespace]
        .into_iter()
        .chain(["apply", "--server-side", "--field-manager", FIELD_MANAGER])
        .map(str::to_owned)
        .collect();
    if force_conflicts {
        args.push("--force-conflicts".to_owned());
    }
    args.extend(["-f".to_owned(), "-".to_owned()]);
    args
}

/// `kubectl apply --server-side` of `manifest` on `context`: what it said (stdout, then stderr).
fn kubectl_apply(
    context: &str,
    namespace: &str,
    manifest: &str,
    force_conflicts: bool,
) -> Result<String, ObeliskK8sError> {
    let cant_run = |error: std::io::Error| {
        ObeliskK8sError::Kubectl(format!("can't run `kubectl` (is it installed?): {error}"))
    };
    let mut child = Command::new("kubectl")
        .args(apply_args(context, namespace, force_conflicts))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(cant_run)?;
    if let Some(mut input) = child.stdin.take() {
        // A kubectl that exits before reading (a bad context) says why on stderr: report that.
        match input.write_all(manifest.as_bytes()) {
            Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                return Err(ObeliskK8sError::Kubectl(format!(
                    "writing to `kubectl apply`: {error}"
                )));
            }
            _ => {}
        }
    }
    let output = child.wait_with_output().map_err(cant_run)?;
    let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    if output.status.success() {
        Ok(format!(
            "{}\n{}",
            said(&output.stdout),
            said(&output.stderr)
        ))
    } else {
        Err(ObeliskK8sError::Kubectl(format!(
            "`kubectl apply --server-side` on `{context}` failed: {}",
            said(&output.stderr)
        )))
    }
}

/// What was applied, and (`--dry-run`) the objects.
impl fmt::Display for PullSecret<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let done = if self.dry_run {
            "Would apply"
        } else {
            "Applied"
        };
        writeln!(
            f,
            "{done} pull secret `{SECRET_NAME}` ({}) and the default ServiceAccount in namespace `{}` on cluster `{}`",
            self.hosts.join(", "),
            self.namespace,
            self.cluster
        )?;
        if self.dry_run {
            f.write_str(&self.shown)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::global::docker::{Auth, Credentials as Creds, Push};

    /// Pulling from `reg.internal` (password `password`) and `reg.example.com` (from SSM).
    fn docker(password: &str) -> Docker {
        let auth = |host: &str, password: &str| Auth {
            host: host.to_owned(),
            credentials: Creds {
                username: "puller".to_owned(),
                password: password.to_owned(),
            },
        };
        Docker {
            file: "~/.obelisk-k8s/clusters/c/docker.yaml".into(),
            push: Push {
                prefix: "reg.internal/krrss".to_owned(),
                credentials: None,
            },
            pull_auths: vec![
                auth("reg.internal", password),
                auth("reg.example.com", "${{obelisk:aws-ssm:/pull/pw}}"),
            ],
        }
    }

    #[test]
    fn a_secret_and_the_default_service_account_using_it() {
        // A password the YAML and JSON both escape: never shown, in any form.
        let tricky = "a\"b\\c\td-secretpart";
        let (ssm, _) = crate::ssm::tests::fake(&[("/pull/pw", tricky)]);
        let docker = docker("lit\"eral\\pw");
        let applied = manifest(&docker, "app-prod", Credentials::Resolved(&ssm)).unwrap();
        let documents = crate::yaml::parse_values_stream(&applied, "x".as_ref()).unwrap();
        let encoded = documents[0]["data"][".dockerconfigjson"].as_str().unwrap();
        let config: Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            config,
            serde_json::json!({ "auths": {
                "reg.internal": { "username": "puller", "password": "lit\"eral\\pw" },
                "reg.example.com": { "username": "puller", "password": tricky },
            } })
        );
        assert_eq!(documents[0].get("stringData"), None);
        assert_eq!(
            documents[1],
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": { "name": "default", "namespace": "app-prod" },
                "imagePullSecrets": [{ "name": "docker-pull" }],
            })
        );

        // Only a password that's just one ref is shown as written.
        for (password, as_is) in [
            ("${{obelisk:aws-ssm:/pull/pw}}", true),
            ("hunter2-${{obelisk:aws-ssm:/pull/pw}}", false),
            ("$${{obelisk:lit}}hunter2", false),
            ("${{obelisk:aws-ssm:/a}}${{obelisk:aws-ssm:/b}}", false),
        ] {
            assert_eq!(ssm::is_one_ref(password), as_is, "{password}");
        }
        let shown = manifest(&docker, "app-prod", Credentials::Shown).unwrap();
        assert!(
            !shown.contains("secretpart") && !shown.contains("eral"),
            "{shown}"
        );
        assert!(shown.contains("<masked>"), "{shown}");
        assert!(shown.contains("${{obelisk:aws-ssm:/pull/pw}}"), "{shown}");
    }

    #[test]
    fn applies_server_side_as_obelisk_k8s_in_the_namespace() {
        assert_eq!(
            apply_args("ctx", "ns", false),
            [
                "--context",
                "ctx",
                "--namespace",
                "ns",
                "apply",
                "--server-side",
                "--field-manager",
                "obelisk-k8s",
                "-f",
                "-"
            ]
        );
        assert!(apply_args("ctx", "ns", true).contains(&"--force-conflicts".to_owned()));
    }

    #[test]
    fn refuses_what_it_cant_apply() {
        let (ssm, _) = crate::ssm::tests::fake(&[("/pull/pw", "s3cret")]);
        let mut global = crate::config::test_support::global();
        let dry = PullSecretOptions {
            dry_run: true,
            ..PullSecretOptions::default()
        };
        let error = |global: &GlobalConfig, ns: &str, options| {
            global
                .pull_secret("c", ns, options, &ssm, |_| {})
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error(&global, "ns", dry),
            "cluster `c` has no `docker.pull.auths` (in clusters/c/docker.yaml): nothing to pull with"
        );
        assert_eq!(
            global
                .pull_secret("nope", "ns", dry, &ssm, |_| {})
                .unwrap_err()
                .to_string(),
            "no cluster `nope` (there are: c, obd)"
        );
        for bad in ["", "-ns", "ns-", "Ns", "n_s", &"n".repeat(64)] {
            assert!(
                error(&global, bad, dry).contains("isn't a namespace name"),
                "{bad}"
            );
        }

        global.clusters[0].docker = Some(docker("pw"));
        let done = global.pull_secret("c", "ns", dry, &ssm, |_| {}).unwrap();
        assert!(done.to_string().starts_with("Would apply pull secret `docker-pull` (reg.internal, reg.example.com) and the default ServiceAccount in namespace `ns` on cluster `c`\n---\n"));

        // Remote: a dry run needs no `--yes`; applying does, and a kubectl context.
        global.clusters[0].location = Location::Remote;
        assert!(global.pull_secret("c", "ns", dry, &ssm, |_| {}).is_ok());
        assert_eq!(
            error(&global, "ns", PullSecretOptions::default()),
            "cluster `c` is remote: pass `--yes` to apply to it"
        );
        global.clusters[0].k8s = crate::config::global::K8s::Talos;
        let yes = PullSecretOptions {
            remote_ok: true,
            ..PullSecretOptions::default()
        };
        assert_eq!(
            error(&global, "ns", yes),
            "cluster `c` has no `kubectlContext` (in clusters/c/cluster.yaml): set one to apply to it"
        );
    }
}
