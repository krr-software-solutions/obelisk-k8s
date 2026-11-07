//! A cluster's `docker.yaml` (`clusters/<dir>/docker.yaml`, next to its `cluster.yaml`): where its
//! images are pushed, and the credentials its pods pull them with.
//!
//! ```yaml
//! docker:
//!   push:
//!     prefix: docker.private.homelab-1.example.com/krrss   # images push as <prefix>/<name>:<version>
//!     username: "${{obelisk:aws-ssm:/infra/nexus/push/username}}"   # optional, with `password`:
//!     password: "${{obelisk:aws-ssm:/infra/nexus/push/password}}"   #   else your own `docker login`
//!   pull:                                                 # optional
//!     auths:
//!       - host: docker.example.com
//!         username: "${{obelisk:aws-ssm:/infra/nexus/pull/username}}"
//!         password: "${{obelisk:aws-ssm:/infra/nexus/pull/password}}"
//! ```
//!
//! Its pods pull an image by the name it's pushed as. `pull.auths` only supplies credentials (one
//! per host), for `images pull-secret` (see `config::pull_secret`). Strings may hold
//! `${{obelisk:aws-ssm:…}}` refs, resolved when they're used. A local k3d cluster without one gets
//! its images with `k3d image import`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::ObeliskK8sError;
use crate::config::config_error;
use crate::ssm;
use crate::yaml::{self, Spanned};

/// The file's name, in a cluster's directory.
pub const DOCKER_FILE: &str = "docker.yaml";

/// A cluster's `docker.yaml`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Docker {
    /// As the user should see it.
    pub file: PathBuf,
    pub push: Push,
    /// Each host once.
    pub pull_auths: Vec<Auth>,
}

/// Where images push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Push {
    /// `<host>[/<path>]`: an image pushes as `<prefix>/<name>:<version>`.
    pub prefix: String,
    /// `None`: push with the user's own `docker login`.
    pub credentials: Option<Credentials>,
}

/// A username and password, as written (they may hold SSM refs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Credentials for pulling from `host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Auth {
    pub host: String,
    pub credentials: Credentials,
}

impl Push {
    /// The registry host: `prefix` up to its first `/`.
    #[must_use]
    pub fn host(&self) -> &str {
        host_of(&self.prefix)
    }
}

/// `prefix`'s host: up to its first `/`.
#[must_use]
pub fn host_of(prefix: &str) -> &str {
    prefix.split_once('/').map_or(prefix, |(host, _)| host)
}

/// The `docker.yaml` in cluster directory `dir` (shown as `shown_dir`), if there is one.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if it isn't valid YAML of the right shape; [`ObeliskK8sError::Config`] if
/// what it says is wrong (an empty or malformed prefix or host, a username without a password, a
/// host twice, a bad SSM ref).
pub(crate) fn load(dir: &Path, shown_dir: &Path) -> Result<Option<Docker>, ObeliskK8sError> {
    let path = dir.join(DOCKER_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let file = shown_dir.join(DOCKER_FILE);
    let DockerFile { docker } = yaml::read_file(&path, &file)?;
    let error = |at, message: String| config_error(&file, Some(at), message);

    let prefix = docker.push.prefix;
    check_prefix(&prefix.value)
        .map_err(|m| error(prefix.at, format!("`docker.push.prefix`: {m}")))?;
    let push_credentials = credentials(
        docker.push.username,
        docker.push.password,
        "docker.push",
        &file,
    )?;

    let mut pull_auths: Vec<Auth> = Vec::new();
    for (i, auth) in docker
        .pull
        .map(|p| p.auths)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let at = format!("docker.pull.auths[{i}]");
        let host = auth.host;
        check_host(&host.value).map_err(|m| error(host.at, format!("`{at}.host`: {m}")))?;
        if pull_auths
            .iter()
            .any(|a| a.host.eq_ignore_ascii_case(&host.value))
        {
            return Err(error(
                host.at,
                format!("`{at}.host`: `{}` is already listed", host.value),
            ));
        }
        let credentials = credentials(Some(auth.username), Some(auth.password), &at, &file)?
            .expect("both are given");
        pull_auths.push(Auth {
            host: host.value,
            credentials,
        });
    }
    Ok(Some(Docker {
        file,
        push: Push {
            prefix: prefix.value,
            credentials: push_credentials,
        },
        pull_auths,
    }))
}

/// A username and password, both or neither, their SSM refs checked.
fn credentials(
    username: Option<Spanned<String>>,
    password: Option<Spanned<String>>,
    at: &str,
    file: &Path,
) -> Result<Option<Credentials>, ObeliskK8sError> {
    match (username, password) {
        (None, None) => Ok(None),
        (Some(username), Some(password)) => {
            for (key, value) in [("username", &username), ("password", &password)] {
                if value.value.is_empty() {
                    return Err(config_error(
                        file,
                        Some(value.at),
                        format!("`{at}.{key}` is empty"),
                    ));
                }
                ssm::check(
                    &Value::String(value.value.clone()),
                    &format!("{}, at `{at}.{key}`", file.display()),
                )?;
            }
            Ok(Some(Credentials {
                username: username.value,
                password: password.value,
            }))
        }
        (Some(given), None) | (None, Some(given)) => Err(config_error(
            file,
            Some(given.at),
            format!("`{at}`: give both `username` and `password`, or neither"),
        )),
    }
}

/// A push prefix: `<host>[/<path>]`, no scheme, no tag. The host must look like a registry, as
/// docker tells them: with a `.` or a `:`, or `localhost` (else docker would push to Docker Hub
/// under that name).
pub(crate) fn check_prefix(prefix: &str) -> Result<(), String> {
    if prefix.contains("://") {
        return Err(format!(
            "`{prefix}` has a scheme: write just `<host>[/<path>]`"
        ));
    }
    let host = host_of(prefix);
    check_host(host)?;
    if !(host.contains('.') || host.contains(':') || host == "localhost") {
        return Err(format!(
            "`{prefix}` doesn't start with a registry host (one with a `.` or a `:`, or `localhost`): docker would push it to Docker Hub"
        ));
    }
    if ["docker.io", "index.docker.io", "registry-1.docker.io"].contains(&host) {
        return Err(format!(
            "`{prefix}` is Docker Hub, which obelisk-k8s doesn't push to (yet)"
        ));
    }
    let Some((_, path)) = prefix.split_once('/') else {
        return Ok(());
    };
    if path.contains(':') || path.contains('@') {
        return Err(format!(
            "`{prefix}` has a tag or digest: the image's own `<name>:<version>` follows it"
        ));
    }
    if let Some(bad) = path.split('/').find(|segment| !is_path_segment(segment)) {
        return Err(format!(
            "`{prefix}`'s path segment `{bad}` isn't a docker repository one: lowercase letters and digits, separated by single `.`/`_`/`-` (or `__`)"
        ));
    }
    Ok(())
}

/// A docker repository path segment: `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`.
fn is_path_segment(segment: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut parts = segment.split(|c: char| !alnum(c));
    let separators: Vec<&str> = segment.split(alnum).filter(|s| !s.is_empty()).collect();
    let separator_ok = |s: &&str| matches!(*s, "." | "_" | "__") || s.chars().all(|c| c == '-');
    !segment.is_empty()
        && segment.starts_with(alnum)
        && segment.ends_with(alnum)
        && parts.all(|p| p.is_empty() || p.chars().all(alnum))
        && separators.iter().all(separator_ok)
}

/// A registry host: `<name>[:<port>]`, no scheme or path; the port a number.
pub(crate) fn check_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("is empty".to_owned());
    }
    if host.contains("://") || host.contains('/') {
        return Err(format!(
            "`{host}` must be just a host (and port): no scheme or path"
        ));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(format!("`{host}` has whitespace"));
    }
    let (name, port) = host
        .split_once(':')
        .map_or((host, None), |(n, p)| (n, Some(p)));
    if name.is_empty() {
        return Err(format!("`{host}` has no host name"));
    }
    if let Some(port) = port
        && port.parse::<u16>().map_or(true, |p| p == 0)
    {
        return Err(format!("`{host}`'s port `{port}` isn't a port number"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DockerFile {
    docker: DockerSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DockerSection {
    push: PushSection,
    pull: Option<PullSection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PushSection {
    prefix: Spanned<String>,
    username: Option<Spanned<String>>,
    password: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullSection {
    auths: Vec<AuthSection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthSection {
    host: Spanned<String>,
    username: Spanned<String>,
    password: Spanned<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(text: &str) -> Result<Option<Docker>, ObeliskK8sError> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DOCKER_FILE), text).unwrap();
        load(dir.path(), Path::new("clusters/c"))
    }

    fn error(text: &str) -> String {
        load_text(text).unwrap_err().to_string()
    }

    #[test]
    fn no_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), Path::new("clusters/c")).unwrap(), None);
    }

    #[test]
    fn loads_push_and_pull() {
        let docker = load_text(
            "docker:\n  push:\n    prefix: reg.internal:5000/krrss\n    username: u\n    password: \"${{obelisk:aws-ssm:/p}}\"\n  pull:\n    auths:\n      - { host: reg.example.com, username: a, password: b }\n      - { host: reg.internal:5000, username: a, password: b }\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(docker.file, Path::new("clusters/c/docker.yaml"));
        assert_eq!(docker.push.prefix, "reg.internal:5000/krrss");
        assert_eq!(docker.push.host(), "reg.internal:5000");
        assert_eq!(
            docker.push.credentials,
            Some(Credentials {
                username: "u".to_owned(),
                password: "${{obelisk:aws-ssm:/p}}".to_owned(),
            })
        );
        let hosts: Vec<_> = docker.pull_auths.iter().map(|a| a.host.as_str()).collect();
        assert_eq!(hosts, ["reg.example.com", "reg.internal:5000"]);

        for prefix in ["localhost", "localhost:5000/a__b/c-d", "reg.io/a.b_c---d/e"] {
            let text = format!("docker:\n  push: {{ prefix: \"{prefix}\" }}\n");
            assert!(load_text(&text).is_ok(), "{prefix}");
        }
        let bare = load_text("docker:\n  push: { prefix: reg.example.com }\n")
            .unwrap()
            .unwrap();
        assert_eq!((bare.push.credentials, bare.pull_auths), (None, vec![]));
    }

    #[test]
    fn refuses_bad_prefixes_hosts_and_credentials() {
        for (text, expected) in [
            (
                "docker:\n  push: { prefix: \"https://reg/x\" }\n",
                "`docker.push.prefix`: `https://reg/x` has a scheme",
            ),
            (
                "docker:\n  push: { prefix: \"reg.io/x:1\" }\n",
                "`docker.push.prefix`: `reg.io/x:1` has a tag or digest",
            ),
            (
                "docker:\n  push: { prefix: \"reg.io//x\" }\n",
                "`docker.push.prefix`: `reg.io//x`'s path segment `` isn't a docker repository one",
            ),
            (
                "docker:\n  push: { prefix: \"reg.io/X\" }\n",
                "`docker.push.prefix`: `reg.io/X`'s path segment `X` isn't a docker repository one",
            ),
            (
                "docker:\n  push: { prefix: \"reg.io/a--\" }\n",
                "path segment `a--` isn't a docker repository one",
            ),
            (
                "docker:\n  push: { prefix: reg.io, username: u, password: \"\" }\n",
                "`docker.push.password` is empty",
            ),
            (
                "docker:\n  push: { prefix: docker.io/me }\n",
                "`docker.push.prefix`: `docker.io/me` is Docker Hub",
            ),
            (
                "docker:\n  push: { prefix: myuser/apps }\n",
                "`docker.push.prefix`: `myuser/apps` doesn't start with a registry host",
            ),
            (
                "docker:\n  push: { prefix: \"reg.io:abc/x\" }\n",
                "`docker.push.prefix`: `reg.io:abc`'s port `abc` isn't a port number",
            ),
            (
                "docker:\n  push: { prefix: \":5000/x\" }\n",
                "`docker.push.prefix`: `:5000` has no host name",
            ),
            (
                "docker:\n  push: { prefix: reg.io }\n  pull:\n    auths:\n      - { host: A.io, username: u, password: p }\n      - { host: a.io, username: u, password: p }\n",
                "`docker.pull.auths[1].host`: `a.io` is already listed",
            ),
            (
                "docker:\n  push: { prefix: \"\" }\n",
                "`docker.push.prefix`: is empty",
            ),
            (
                "docker:\n  push: { prefix: reg.io, username: u }\n",
                "`docker.push`: give both `username` and `password`, or neither",
            ),
            (
                "docker:\n  push: { prefix: reg.io, username: u, password: \"ref+awsssm:///x\" }\n",
                "ref+awsssm",
            ),
            (
                "docker:\n  push: { prefix: reg.io }\n  pull:\n    auths:\n      - { host: a, username: u, password: p }\n      - { host: a, username: u, password: p }\n",
                "`docker.pull.auths[1].host`: `a` is already listed",
            ),
            (
                "docker:\n  push: { prefix: reg.io }\n  pull:\n    auths:\n      - { host: a/b, username: u, password: p }\n",
                "`docker.pull.auths[0].host`: `a/b` must be just a host",
            ),
            (
                "docker:\n  push: { prefix: reg.io }\n  pull:\n    auths:\n      - { host: a, username: u }\n",
                "missing field `password`",
            ),
            (
                "docker:\n  push: { prefix: reg.io, tag: x }\n",
                "unknown field `tag`",
            ),
        ] {
            let e = error(text);
            assert!(e.contains("clusters/c/docker.yaml"), "{text}: {e}");
            assert!(e.contains(expected), "{text}: {e}");
        }
    }
}
