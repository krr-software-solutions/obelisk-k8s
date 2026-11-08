//! `images build` (without `--no-push`): building each selected case's images (see
//! `config::images`), then getting them to its target's cluster.
//!
//! Where an image goes is its target's cluster's (see `config::global::docker`):
//!
//! - a cluster with a `docker.yaml`: `docker tag <name>:<version> <prefix>/<name>:<version>`, then
//!   `docker push` it: with its `push` credentials (their SSM refs resolved) written to a throwaway
//!   docker client config (on the user's own docker context), or else with the user's own `docker
//!   login`;
//! - a local k3d cluster without one: `k3d image import <name>:<version>`;
//! - any other cluster: an error, before anything builds or pushes.
//!
//! Every destination and credential is checked first; then each image builds, each tag once; then
//! each delivery happens once, however many cases share it (a ref pushed once for every cluster
//! whose `docker.yaml` pushes it the same way).

use std::fmt;
use std::path::Path;

use serde_json::Value;

use crate::ObeliskK8sError;
use crate::config::ephemeral::Ephemeral;
use crate::config::global::docker::Push;
use crate::config::global::{Cluster, GlobalConfig};
use crate::config::images::{Cases, Image, MAX_REPOSITORY, build_one, distinct};
use crate::config::paths::Selection;
use crate::config::repo::RepoConfig;
use crate::docker::{PushAuth, UserDocker};
use crate::ssm::Ssm;

/// Where a cluster's images go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination<'g> {
    /// Into local k3d cluster `cluster`'s nodes.
    K3d { cluster: &'g str },
    /// To a registry, as `<prefix>/<name>:<version>`.
    Registry { cluster: &'g str, push: &'g Push },
}

impl<'g> Destination<'g> {
    /// Where `cluster`'s images go.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if it has no `docker.yaml` and isn't a local k3d cluster.
    pub fn of(cluster: &'g Cluster) -> Result<Self, ObeliskK8sError> {
        match &cluster.docker {
            Some(docker) => Ok(Self::Registry {
                cluster: &cluster.name,
                push: &docker.push,
            }),
            None if cluster.is_local_k3d() => Ok(Self::K3d {
                cluster: &cluster.name,
            }),
            None => Err(ObeliskK8sError::Usage(format!(
                "cluster `{}` has nowhere to push images: add {}",
                cluster.name,
                cluster
                    .file
                    .with_file_name(crate::config::global::docker::DOCKER_FILE)
                    .display()
            ))),
        }
    }

    /// The repository image `name` is delivered as: as is into k3d (its name checked as docker
    /// names it locally: see `config::images`), under the prefix to a registry.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if that's over docker's limit for a repository name.
    pub fn repository(&self, name: &str) -> Result<String, ObeliskK8sError> {
        match self {
            Self::K3d { .. } => Ok(name.to_owned()),
            Self::Registry { cluster, push } => {
                let repository = format!("{}/{name}", push.prefix);
                if repository.len() > MAX_REPOSITORY {
                    return Err(ObeliskK8sError::Usage(format!(
                        "image `{name}` would push to cluster `{cluster}`'s registry as `{repository}`, over docker's {MAX_REPOSITORY} characters for a repository: shorten the name or `push.prefix`"
                    )));
                }
                Ok(repository)
            }
        }
    }

    /// The cluster it's for.
    #[must_use]
    pub fn cluster(&self) -> &'g str {
        match self {
            Self::K3d { cluster } | Self::Registry { cluster, .. } => cluster,
        }
    }
}

/// What `images build` delivered: each pushed or imported image, in order, with the cases it's for.
#[derive(Debug)]
pub struct PushedImages<'a> {
    pub images: Vec<Pushed<'a>>,
}

/// One image delivered to one destination.
#[derive(Debug)]
pub struct Pushed<'a> {
    /// What it was delivered as (`<prefix>/<name>:<version>`, or `<name>:<version>` into k3d).
    pub image_ref: String,
    /// Into a k3d cluster (else pushed to a registry).
    pub imported: bool,
    /// The k3d cluster, or the registry host.
    pub to: String,
    /// The clusters it's for.
    pub clusters: Vec<String>,
    pub cases: Cases<'a>,
}

/// One delivery to make: an image to a k3d cluster, or as one ref (with one set of credentials) to
/// a registry, however many clusters share it.
#[derive(Debug, PartialEq, Eq)]
struct Delivery<'g, 'a> {
    tag: String,
    image_ref: String,
    to: Destination<'g>,
    clusters: Vec<&'g str>,
    cases: Cases<'a>,
}

impl Delivery<'_, '_> {
    /// Whether it's the same delivery as `tag` delivered as `image_ref` to `to`: the same image into
    /// the same k3d cluster, or the same ref pushed with the same credentials.
    fn same(&self, tag: &str, image_ref: &str, to: &Destination<'_>) -> bool {
        match (&self.to, to) {
            (Destination::K3d { cluster: a }, Destination::K3d { cluster: b }) => {
                self.tag == tag && a == b
            }
            (Destination::Registry { push: a, .. }, Destination::Registry { push: b, .. }) => {
                self.image_ref == image_ref && a.credentials == b.credentials
            }
            _ => false,
        }
    }
}

/// The deliveries for `builds` (`(image, cases)`), each case's destination from `destination`
/// (by target name): each once, in order.
fn plan<'g, 'a>(
    builds: &[(&Image, Cases<'a>)],
    mut destination: impl FnMut(&str) -> Result<Destination<'g>, ObeliskK8sError>,
) -> Result<Vec<Delivery<'g, 'a>>, ObeliskK8sError> {
    let mut deliveries: Vec<Delivery<'g, 'a>> = Vec::new();
    for (image, cases) in builds {
        let tag = image.tag();
        for &(target, unit) in cases {
            let to = destination(target)?;
            let cluster = to.cluster();
            let image_ref = format!("{}:{}", to.repository(&image.name)?, image.version);
            match deliveries
                .iter_mut()
                .find(|d| d.same(&tag, &image_ref, &to))
            {
                Some(delivery) => {
                    if !delivery.clusters.contains(&cluster) {
                        delivery.clusters.push(cluster);
                    }
                    delivery.cases.push((target, unit));
                }
                None => deliveries.push(Delivery {
                    tag: tag.clone(),
                    image_ref,
                    to,
                    clusters: vec![cluster],
                    cases: vec![(target, unit)],
                }),
            }
        }
    }
    Ok(deliveries)
}

impl RepoConfig {
    /// Builds and delivers the images of every case in `selection` (only those named `only`, if
    /// any) to each case's target's cluster. `ssm` resolves push credentials' refs; `shown_root`,
    /// `warn` and `step` are as for [`RepoConfig::images_build`].
    ///
    /// Everything that can fail without building is checked first: every case's destination, and
    /// every registry's credentials (resolved, and written to a throwaway client config).
    ///
    /// # Errors
    ///
    /// As [`RepoConfig::images_build`]; [`ObeliskK8sError::Usage`] if a case's cluster has nowhere
    /// to push images; [`ObeliskK8sError::Ssm`] if credentials' refs can't be resolved;
    /// [`ObeliskK8sError::Image`] if `docker tag`, `push` or `k3d image import` fails (nothing
    /// after it is delivered).
    #[allow(clippy::too_many_arguments)]
    pub fn images_push<'a, 'g>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        shown_root: Option<&Path>,
        ephemeral: Option<&Ephemeral>,
        global: &'g GlobalConfig,
        only: &[String],
        ssm: &Ssm,
        warn: impl FnMut(&str),
        mut step: impl FnMut(&str),
    ) -> Result<PushedImages<'a>, ObeliskK8sError> {
        let report =
            self.images_report(repo_root, selection, shown_root, ephemeral, global, warn)?;
        let builds = distinct(&report, only)?;
        let images: Vec<(&Image, Cases<'a>)> = builds
            .iter()
            .map(|(_, image, cases)| (*image, cases.clone()))
            .collect();
        let deliveries = plan(&images, |target| {
            let cluster = self
                .targets
                .iter()
                .find(|t| t.name == target)
                .map(|t| t.cluster.as_str())
                .ok_or_else(|| ObeliskK8sError::Usage(format!("no target `{target}`")))?;
            Destination::of(global.cluster(cluster)?).map_err(|e| match e {
                ObeliskK8sError::Usage(message) => {
                    ObeliskK8sError::Usage(format!("{message} (or pass `--no-push` to only build)"))
                }
                other => other,
            })
        })?;
        // Resolved now, so a bad one fails before anything builds; written just before a push.
        let mut logins: Vec<Login<'g>> = resolve_logins(&deliveries, ssm)?;
        let mut user = None;

        for (tag, image, _) in &builds {
            build_one(tag, image, repo_root, &mut step)?;
        }

        let mut images = Vec::new();
        for delivery in deliveries {
            let (imported, to) = match delivery.to {
                Destination::K3d { cluster } => {
                    step(&format!(
                        "Importing image `{}` into k3d cluster `{cluster}`",
                        delivery.image_ref
                    ));
                    crate::docker::k3d_import(cluster, &delivery.tag)?;
                    (true, cluster.to_owned())
                }
                Destination::Registry { push, .. } => {
                    let login = logins.iter_mut().find(|l| {
                        l.push.credentials == push.credentials && l.push.host() == push.host()
                    });
                    let auth = match login {
                        Some(login) => Some(login.auth(&mut user)?),
                        None => None,
                    };
                    step(&format!("Pushing image `{}`", delivery.image_ref));
                    crate::docker::push(auth, &delivery.tag, &delivery.image_ref)?;
                    (false, push.host().to_owned())
                }
            };
            images.push(Pushed {
                image_ref: delivery.image_ref,
                imported,
                to,
                clusters: delivery.clusters.iter().map(|&c| c.to_owned()).collect(),
                cases: delivery.cases,
            });
        }
        Ok(PushedImages { images })
    }
}

/// One registry's resolved push credentials, and their throwaway client config once it's written.
struct Login<'g> {
    push: &'g Push,
    username: String,
    password: String,
    auth: Option<PushAuth>,
}

impl Login<'_> {
    /// Its client config, written the first time it's needed (on `user`'s docker context, asked
    /// for the first time any is needed).
    fn auth(&mut self, user: &mut Option<UserDocker>) -> Result<&PushAuth, ObeliskK8sError> {
        if self.auth.is_none() {
            let user = match user {
                Some(user) => user,
                None => user.insert(UserDocker::current()?),
            };
            let auth = PushAuth::new(
                &PushAuth::parent(),
                user,
                self.push.host(),
                &self.username,
                &self.password,
            )?;
            self.auth = Some(auth);
        }
        Ok(self.auth.as_ref().expect("just written"))
    }
}

/// The credentials for each registry `deliveries` push to with credentials (each registry host
/// and set of credentials once), their SSM refs resolved together.
fn resolve_logins<'g>(
    deliveries: &[Delivery<'g, '_>],
    ssm: &Ssm,
) -> Result<Vec<Login<'g>>, ObeliskK8sError> {
    let mut pushes: Vec<&'g Push> = Vec::new();
    for delivery in deliveries {
        if let Destination::Registry { push, .. } = delivery.to
            && push.credentials.is_some()
            && !pushes
                .iter()
                .any(|p| p.host() == push.host() && p.credentials == push.credentials)
        {
            pushes.push(push);
        }
    }
    let mut values: Vec<(String, Value)> = Vec::new();
    for push in &pushes {
        if let Some(credentials) = &push.credentials {
            let what = format!("the push credentials for {}", push.host());
            values.push((what.clone(), Value::String(credentials.username.clone())));
            values.push((what, Value::String(credentials.password.clone())));
        }
    }
    ssm.resolve_all(values.iter_mut().map(|(what, value)| (what.clone(), value)))?;
    let text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    Ok(pushes
        .into_iter()
        .zip(values.chunks(2))
        .map(|(push, pair)| Login {
            push,
            username: text(&pair[0].1),
            password: text(&pair[1].1),
            auth: None,
        })
        .collect())
}

impl fmt::Display for PushedImages<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.images.is_empty() {
            return writeln!(f, "No images to build");
        }
        for pushed in &self.images {
            let cases: Vec<String> = pushed
                .cases
                .iter()
                .map(|(target, unit)| format!("{unit} on {target}"))
                .collect();
            if pushed.imported {
                writeln!(
                    f,
                    "Imported {} into k3d cluster `{}` ({})",
                    pushed.image_ref,
                    pushed.to,
                    cases.join(", ")
                )?;
            } else {
                let clusters: Vec<String> =
                    pushed.clusters.iter().map(|c| format!("`{c}`")).collect();
                writeln!(
                    f,
                    "Pushed {} to {} (for {} {}: {})",
                    pushed.image_ref,
                    pushed.to,
                    if clusters.len() == 1 {
                        "cluster"
                    } else {
                        "clusters"
                    },
                    clusters.join(", "),
                    cases.join(", ")
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::global::docker::{Credentials, Docker};
    use crate::config::global::{K8s, Location};
    use crate::config::images::Dockerfile;
    use crate::config::test_support::{TARGET, global, helm_installed, write_repo, write_script};

    fn with_docker(mut cluster: Cluster, name: &str, prefix: &str, user: &str) -> Cluster {
        cluster.name = name.to_owned();
        cluster.docker = Some(Docker {
            file: format!("clusters/{name}/docker.yaml").into(),
            push: Push {
                prefix: prefix.to_owned(),
                credentials: Some(Credentials {
                    username: user.to_owned(),
                    password: "p".to_owned(),
                }),
            },
            pull_auths: Vec::new(),
        });
        cluster
    }

    #[test]
    fn a_clusters_destination() {
        let global = global();
        let k3d = global.cluster("c").unwrap();
        assert_eq!(
            Destination::of(k3d).unwrap(),
            Destination::K3d { cluster: "c" }
        );
        assert_eq!(
            Destination::of(k3d).unwrap().repository("app").unwrap(),
            "app"
        );

        let registry = with_docker(k3d.clone(), "c", "reg.example.com/krrss", "u");
        let to = Destination::of(&registry).unwrap();
        assert_eq!(to.repository("app").unwrap(), "reg.example.com/krrss/app");
        assert_eq!(to.cluster(), "c");
        // A repository path keeps its layout, under the prefix or as is.
        let name = "krrss-mc/auth/web-server";
        assert_eq!(
            Destination::of(k3d).unwrap().repository(name).unwrap(),
            name
        );
        let registry = with_docker(k3d.clone(), "c", "reg.internal:5000/krrss", "u");
        let to = Destination::of(&registry).unwrap();
        assert_eq!(
            to.repository(name).unwrap(),
            "reg.internal:5000/krrss/krrss-mc/auth/web-server"
        );
        // Up to docker's limit, prefix included (`reg.internal:5000/krrss/`: 24).
        assert!(to.repository(&"a".repeat(231)).is_ok());
        let long = "a".repeat(232);
        assert_eq!(
            to.repository(&long).unwrap_err().to_string(),
            format!(
                "image `{long}` would push to cluster `c`'s registry as `reg.internal:5000/krrss/{long}`, over docker's 255 characters for a repository: shorten the name or `push.prefix`"
            )
        );

        let mut remote = k3d.clone();
        (remote.k8s, remote.location) = (K8s::Talos, Location::Remote);
        assert_eq!(
            Destination::of(&remote).unwrap_err().to_string(),
            "cluster `c` has nowhere to push images: add clusters/c/docker.yaml"
        );
    }

    #[test]
    fn each_delivery_once_however_many_clusters_share_it() {
        let base = global().cluster("c").unwrap().clone();
        // `dev` and `prod` push the same way to one registry; `other` with other credentials.
        let dev = with_docker(base.clone(), "dev", "reg.example.com/krrss", "u");
        let prod = with_docker(base.clone(), "prod", "reg.example.com/krrss", "u");
        let other = with_docker(base.clone(), "other", "reg.example.com/krrss", "v");
        let clusters = [&dev, &prod, &other, &base];
        let image = Image {
            name: "app".to_owned(),
            version: "1".to_owned(),
            pre_build: None,
            dockerfile: Dockerfile::File {
                path: "/r/Dockerfile".into(),
            },
            context_path: "/r".into(),
        };
        let builds = vec![(
            &image,
            vec![
                ("dev", "app"),
                ("prod", "app"),
                ("other", "app"),
                ("c", "app"),
                ("c", "web"),
            ],
        )];
        let deliveries = plan(&builds, |target| {
            let cluster = clusters.iter().find(|c| c.name == target).unwrap();
            Destination::of(cluster)
        })
        .unwrap();
        let summary: Vec<_> = deliveries
            .iter()
            .map(|d| (d.image_ref.as_str(), d.clusters.clone(), d.cases.len()))
            .collect();
        assert_eq!(
            summary,
            [
                ("reg.example.com/krrss/app:1", vec!["dev", "prod"], 2),
                ("reg.example.com/krrss/app:1", vec!["other"], 1),
                ("app:1", vec!["c"], 2),
            ]
        );
    }

    #[test]
    fn nowhere_to_push_fails_before_anything_builds() {
        if !helm_installed() {
            return;
        }
        let dir = write_repo(&[
            ("repo.yaml", "repo:\n  name: r\n  build: ./build.yaml\n"),
            ("targets/prod/target.yaml", TARGET),
            (
                "units/app/unit.yaml",
                "unit:\n  release: { version: 1.0.0 }\n",
            ),
            ("app/Dockerfile", "FROM scratch\n"),
            (
                "build.yaml",
                "build:\n  phases:\n    - name: p\n      images: [{ type: inline, images: [{ name: app, version: v1, preBuild: { type: file, path: ./app/pre.sh }, dockerfile: { type: file, path: ./app/Dockerfile } }] }]\n",
            ),
        ]);
        let marker = dir.path().join("built");
        write_script(
            &dir.path().join(".obelisk-k8s/app/pre.sh"),
            &format!("#!/bin/sh\ntouch {}\n", marker.display()),
        );
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config
            .select(&["prod".to_owned()], &["app".to_owned()])
            .unwrap();
        let mut global = global();
        (global.clusters[0].k8s, global.clusters[0].location) = (K8s::Talos, Location::Remote);
        let (ssm, _) = crate::ssm::tests::fake(&[]);
        let error = config
            .images_push(
                dir.path(),
                &selection,
                None,
                None,
                &global,
                &[],
                &ssm,
                |_| {},
                |_| {},
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "cluster `c` has nowhere to push images: add clusters/c/docker.yaml (or pass `--no-push` to only build)"
        );
        assert!(!marker.exists(), "nothing built");
    }
}
